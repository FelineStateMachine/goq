use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use iroh::EndpointId;
use iroh::endpoint::{Connection, SendStream};
use iroh::protocol::ProtocolHandler;
use moq_net::{
    Broadcast, BroadcastProducer, Error as MoqError, GroupProducer, Origin, Track, TrackProducer,
};
use sigil_protocol::{
    AUDIO_HEADER_LEN, AdaptiveBitrateDecisionV1, AdaptiveBitrateReasonFlagsV1,
    AdaptiveBitrateStateV1, AudioFlags, AudioPacket, AudioPacketHeader, Capability, ClientHello,
    FrameFlags, HostHello, InputAck, InvitationGrants, KeyframeRequestReasonV3,
    MAX_AUDIO_PAYLOAD_LEN, MAX_MEDIA_GROUP_BYTES_V3, MAX_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS,
    MAX_MEDIA_OBJECT_ID_V3, MIN_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS, MOQ_VIDEO_H264_TRACK,
    MOQ_VIDEO_TRACK_PRIORITY, MediaControlRequestV3, MediaFeedbackFlags, MediaFeedbackReportV1,
    MediaFrame, MediaFrameHeader, MediaObjectHeaderV3, MediaObjectV3, encode_media_frame_object,
    media_moq_broadcast_name, read_client_hello, read_input_event, read_media_control_request_v3,
    read_media_feedback_report_v1, write_adaptive_bitrate_decision_v1, write_host_hello,
    write_input_ack, write_media_frame, write_media_object_v3,
};
use tracing::{debug, error, info, warn};

use crate::audio::spawn_pipewire_audio;
use crate::authorization::{AuthorizationPolicy, unix_timestamp_now};
use crate::clock::SessionClock;
use crate::config::{GamescopeEncoderBackend, HostConfig, VaapiRateControl, VideoSource};
use crate::cursor::{PointerPositionTracker, PointerState};
use crate::input::{InputBackend, InputDisposition};
use crate::moq_catalog::publish_goq_catalog;
use crate::source::{
    EncodedFrame, EncodedGop, EncodedSource, EncoderControl,
    spawn_gamescope_pipewire_after_static_preflight, spawn_test_pattern,
};

const MEDIA_CAPABILITIES: &[Capability] = &[Capability::VideoH264];
const AUDIO_CAPABILITIES: &[Capability] = &[Capability::AudioOpus];
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const INPUT_ACK_TIMEOUT: Duration = Duration::from_secs(1);
const REJECTION_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_PENDING_HANDSHAKES: usize = 4;
// Allow one frame of ordinary scheduler/write jitter beyond the frame being
// sent, but never replay a suffix already more than two configured periods old.
const MAX_MEDIA_REPLAY_AGE_FRAME_PERIODS: u64 = 2;
const SOURCE_REAP_GRACE_TIMEOUT: Duration = Duration::from_secs(1);
const MOQ_ATTACHMENT_TIMEOUT: Duration = Duration::from_secs(10);
const MOQ_REJECT_CODE: u32 = 0x534d;
const ENCODER_CONTROL_COMMIT_TIMEOUT: Duration = Duration::from_secs(2);

mod adaptive;
mod media_v2;
mod media_v3;
mod session;
mod startup;

#[allow(unused_imports)]
pub(crate) use adaptive::MotionResolutionPolicy;
pub(crate) use adaptive::VideoDimensions;
use adaptive::serve_media_feedback;
use media_v2::{media_frame_for_encoded, serve_media, serve_media_v2};
use media_v3::{
    MediaV3GroupCursor, forward_media_v3_control_requests, new_current_gop_frames, serve_media_v3,
};

pub use session::SessionRegistry;
use session::{
    ClaimedMoqAttachment, ForcedIdrCoordinator, ForcedIdrDisposition, MediaV3Telemetry,
    MoqAttachmentWait, SourceTaskGuard,
};
use startup::select_gamescope_startup_source;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaReplayDecision {
    Send { discontinuity: bool },
    SkipUntilKeyframe,
    DiscardStaleSuffix { through_sequence: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MediaV3ObjectPosition {
    group_id: u64,
    object_id: u32,
    discontinuity: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaV3GroupDecision {
    Send(MediaV3ObjectPosition),
    SkipUntilKeyframe,
    EnterResync,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MoqGroupDecision {
    Published {
        group_id: u64,
        frame_id: u32,
        cancelled_previous: bool,
    },
    SkipUntilKeyframe,
    EnterResync,
}

/// Owns the single bounded live MoQ track. One configured H.264 GOP maps to
/// one native MoQ group; its application frame sequence remains inside the
/// encoded object envelope and is never reused as the transport group id.
struct MoqGroupPublisher {
    track: TrackProducer,
    current_group: Option<GroupProducer>,
    cursor: MediaV3GroupCursor,
    object_bytes: usize,
}

impl MoqGroupPublisher {
    fn new(track: TrackProducer) -> Self {
        Self {
            track,
            current_group: None,
            cursor: MediaV3GroupCursor::default(),
            object_bytes: 0,
        }
    }

    fn publish(
        &mut self,
        config: &HostConfig,
        frame: &EncodedFrame,
        replay_discontinuity: bool,
    ) -> Result<MoqGroupDecision> {
        let position = match self.cursor.classify(frame) {
            MediaV3GroupDecision::Send(position) => position,
            MediaV3GroupDecision::SkipUntilKeyframe => {
                return Ok(MoqGroupDecision::SkipUntilKeyframe);
            }
            MediaV3GroupDecision::EnterResync => {
                self.abort_current();
                return Ok(MoqGroupDecision::EnterResync);
            }
        };
        let object = encode_media_frame_object(&media_frame_for_encoded(
            config,
            frame,
            replay_discontinuity || position.discontinuity,
        )?)?;
        let next_object_bytes = if position.object_id == 0 {
            Some(object.len())
        } else {
            self.object_bytes.checked_add(object.len())
        };
        let Some(next_object_bytes) =
            next_object_bytes.filter(|bytes| *bytes <= MAX_MEDIA_GROUP_BYTES_V3)
        else {
            self.cursor.request_keyframe();
            self.abort_current();
            return Ok(MoqGroupDecision::EnterResync);
        };

        if position.object_id == 0 {
            // A new independently-decodable GOP supersedes the previous one.
            // Actively aborting it cancels a slow subscriber rather than
            // retaining a playable history behind the live edge.
            let cancelled_previous = self.abort_current().is_some();
            let mut group = self
                .track
                .append_group()
                .context("creating sequential MoQ video group")?;
            let group_id = group.sequence;
            group
                .write_frame(object)
                .context("writing configured keyframe to MoQ group")?;
            self.object_bytes = next_object_bytes;
            self.current_group = Some(group);
            return Ok(MoqGroupDecision::Published {
                group_id,
                frame_id: 0,
                cancelled_previous,
            });
        }

        let group = self
            .current_group
            .as_mut()
            .context("MoQ delta frame has no active configured-keyframe group")?;
        let group_id = group.sequence;
        group
            .write_frame(object)
            .context("writing delta access unit to MoQ group")?;
        self.object_bytes = next_object_bytes;
        Ok(MoqGroupDecision::Published {
            group_id,
            frame_id: position.object_id,
            cancelled_previous: false,
        })
    }

    fn request_keyframe(&mut self) -> Option<u64> {
        self.cursor.request_keyframe();
        self.abort_current()
    }

    fn abort_current(&mut self) -> Option<u64> {
        self.object_bytes = 0;
        let mut group = self.current_group.take()?;
        let group_id = group.sequence;
        let _ = group.abort(MoqError::Cancel);
        Some(group_id)
    }

    fn abort(mut self) {
        self.abort_current();
        let _ = self.track.abort(MoqError::Cancel);
    }
}

fn apply_moq_keyframe_request(
    publisher: &mut MoqGroupPublisher,
    replay_cursor: &mut MediaReplayCursor,
    through_sequence: Option<u64>,
    reason: KeyframeRequestReasonV3,
) -> Option<u64> {
    // The bounded current group is already the late joiner's decodable replay.
    // Aborting it on Join can strand a static source until its next natural IDR.
    if reason == KeyframeRequestReasonV3::Join {
        return None;
    }
    let cancelled_group = publisher.request_keyframe();
    replay_cursor.enter_resync_through(through_sequence);
    cancelled_group
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MediaReplayCursor {
    last_sequence: Option<u64>,
    waiting_for_keyframe: bool,
    discontinuity_pending: bool,
}

impl Default for MediaReplayCursor {
    fn default() -> Self {
        Self {
            last_sequence: None,
            waiting_for_keyframe: true,
            discontinuity_pending: false,
        }
    }
}

impl MediaReplayCursor {
    fn classify(
        &mut self,
        frame: &EncodedFrame,
        replay_through_sequence: u64,
        initial_replay_started_at: Option<Instant>,
        observed_now: Instant,
        maximum_replay_age: Duration,
    ) -> MediaReplayDecision {
        let replay_age = observed_now.saturating_duration_since(frame.observed_at);
        let initial_replay_within_budget = initial_replay_started_at.is_some_and(|started_at| {
            observed_now.saturating_duration_since(started_at) <= maximum_replay_age
        });
        if !initial_replay_within_budget && replay_age > maximum_replay_age {
            self.last_sequence = Some(replay_through_sequence);
            self.waiting_for_keyframe = true;
            self.discontinuity_pending = true;
            return MediaReplayDecision::DiscardStaleSuffix {
                through_sequence: replay_through_sequence,
            };
        }

        let sequence_discontinuity = self
            .last_sequence
            .is_some_and(|previous| previous.checked_add(1) != Some(frame.sequence));
        if sequence_discontinuity {
            self.waiting_for_keyframe = true;
            self.discontinuity_pending = true;
        }
        if self.waiting_for_keyframe && !(frame.keyframe && frame.codec_config) {
            return MediaReplayDecision::SkipUntilKeyframe;
        }
        MediaReplayDecision::Send {
            discontinuity: self.discontinuity_pending,
        }
    }

    fn commit_sent(&mut self, frame: &EncodedFrame) {
        self.last_sequence = Some(frame.sequence);
        self.waiting_for_keyframe = false;
        self.discontinuity_pending = false;
    }

    fn enter_resync_through(&mut self, through_sequence: Option<u64>) {
        if let Some(through_sequence) = through_sequence {
            self.last_sequence = Some(
                self.last_sequence
                    .map_or(through_sequence, |last| last.max(through_sequence)),
            );
        }
        self.waiting_for_keyframe = true;
        self.discontinuity_pending = true;
    }
}

fn maximum_media_replay_age(framerate: u32) -> Duration {
    debug_assert!(framerate > 0);
    let frame_period_nanos = 1_000_000_000_u64.div_ceil(u64::from(framerate.max(1)));
    Duration::from_nanos(frame_period_nanos.saturating_mul(MAX_MEDIA_REPLAY_AGE_FRAME_PERIODS))
}

#[derive(Clone, Debug)]
pub struct MediaHandler {
    pub config: HostConfig,
    pub sessions: Arc<SessionRegistry>,
    pub authorization: AuthorizationPolicy,
}

#[derive(Clone, Debug)]
pub struct MediaV2Handler {
    pub config: HostConfig,
    pub sessions: Arc<SessionRegistry>,
    pub authorization: AuthorizationPolicy,
}

#[derive(Clone, Debug)]
pub struct MediaV3Handler {
    pub config: HostConfig,
    pub sessions: Arc<SessionRegistry>,
    pub authorization: AuthorizationPolicy,
}

#[derive(Clone, Debug)]
pub struct ControlHandler {
    pub config: HostConfig,
    pub sessions: Arc<SessionRegistry>,
    pub authorization: AuthorizationPolicy,
}

#[derive(Clone, Debug)]
pub struct MediaFeedbackHandler {
    pub config: HostConfig,
    pub sessions: Arc<SessionRegistry>,
    pub authorization: AuthorizationPolicy,
}

/// Upstream MoQ admission guarded by an already-authenticated control lease.
///
/// This deliberately does not use `iroh_moq::Moq::protocol_handler`: that
/// actor makes a completed session globally visible before application-level
/// acceptance. Consuming the exact pending attachment first prevents MoQ from
/// bypassing Sigil's invitation, enrollment, and one-client gate.
#[derive(Clone, Debug)]
pub struct AuthorizedMoqHandler {
    pub sessions: Arc<SessionRegistry>,
    pub origin: Origin,
}

impl ProtocolHandler for MediaHandler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        if let Err(error) = serve_media(
            connection,
            self.config.clone(),
            &self.sessions,
            &self.authorization,
        )
        .await
        {
            warn!(%remote, %error, "media connection ended");
        }
        Ok(())
    }
}

impl ProtocolHandler for MediaV2Handler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        if let Err(error) = serve_media_v2(
            connection,
            self.config.clone(),
            &self.sessions,
            &self.authorization,
        )
        .await
        {
            warn!(%remote, %error, "media v2 connection ended");
        }
        Ok(())
    }
}

impl ProtocolHandler for MediaV3Handler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        if let Err(error) = serve_media_v3(
            connection,
            self.config.clone(),
            &self.sessions,
            &self.authorization,
        )
        .await
        {
            warn!(%remote, %error, "media v3 connection ended");
        }
        Ok(())
    }
}

impl ProtocolHandler for ControlHandler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        if let Err(error) = serve_control_moq(
            connection,
            self.config.clone(),
            &self.sessions,
            &self.authorization,
        )
        .await
        {
            warn!(%remote, %error, "MoQ control connection ended");
        }
        Ok(())
    }
}

impl ProtocolHandler for MediaFeedbackHandler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        if let Err(error) = serve_media_feedback(
            connection,
            &self.config,
            &self.sessions,
            &self.authorization,
        )
        .await
        {
            warn!(%remote, %error, "media feedback connection ended");
        }
        Ok(())
    }
}

impl ProtocolHandler for AuthorizedMoqHandler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        let attachment = match self.sessions.claim_moq(remote) {
            Ok(attachment) => attachment,
            Err(error) => {
                connection.close(MOQ_REJECT_CODE.into(), b"unauthorized MoQ attachment");
                warn!(%remote, %error, "rejected unsolicited MoQ connection");
                return Ok(());
            }
        };
        if let Err(error) = serve_authorized_moq(connection, self.origin, attachment).await {
            warn!(%remote, %error, "authorized MoQ connection ended");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct InputHandler {
    pub backend: InputBackend,
    pub pointer_positions: Option<PointerPositionTracker>,
    pub sessions: Arc<SessionRegistry>,
}

#[derive(Clone, Debug)]
pub struct AudioHandler {
    pub config: HostConfig,
    pub sessions: Arc<SessionRegistry>,
}

impl ProtocolHandler for AudioHandler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        if let Err(error) = serve_audio(connection, self.config.clone(), &self.sessions).await {
            warn!(%remote, %error, "audio connection ended");
        }
        Ok(())
    }
}

impl ProtocolHandler for InputHandler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        if let Err(error) = serve_input(
            connection,
            &self.backend,
            self.pointer_positions.as_ref(),
            &self.sessions,
        )
        .await
        {
            warn!(%remote, %error, "input connection ended");
        }
        Ok(())
    }
}

async fn serve_authorized_moq(
    connection: Connection,
    origin: Origin,
    attachment: ClaimedMoqAttachment,
) -> Result<()> {
    let ClaimedMoqAttachment {
        session_id,
        broadcast_name,
        broadcast,
        attached,
        closed,
        telemetry,
    } = attachment;
    let result: Result<()> = async {
        let web_transport = web_transport_iroh::Session::raw(connection);
        let session = tokio::time::timeout(
            MOQ_ATTACHMENT_TIMEOUT,
            iroh_moq::MoqSession::session_accept(web_transport, origin),
        )
        .await
        .context("timed out completing authorized MoQ handshake")?
        .context("completing authorized MoQ handshake")?;
        let broadcast_closed = broadcast.clone();
        session.publish(&broadcast_name, broadcast);
        ensure!(
            attached.send(()).is_ok(),
            "control session ended before MoQ attachment completed"
        );
        info!(
            remote = %session.remote_id(),
            session_id,
            %broadcast_name,
            track = MOQ_VIDEO_H264_TRACK,
            "authorized MoQ media attachment accepted"
        );
        let mut telemetry_interval = tokio::time::interval(Duration::from_secs(1));
        telemetry_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                reason = session.closed() => {
                    debug!(remote = %session.remote_id(), ?reason, "MoQ media session closed");
                    break;
                }
                reason = broadcast_closed.closed() => {
                    debug!(remote = %session.remote_id(), ?reason, "control-owned MoQ broadcast closed");
                    session.close(0, b"control session ended");
                    break;
                }
                _ = telemetry_interval.tick() => {
                    telemetry.record_selected_path(session.conn());
                }
            }
        }
        Ok(())
    }
    .await;
    let _ = closed.send(());
    result
}

async fn serve_control_moq(
    connection: Connection,
    config: HostConfig,
    sessions: &Arc<SessionRegistry>,
    authorization: &AuthorizationPolicy,
) -> Result<()> {
    let remote = connection.remote_id();
    let handshake_permit = sessions
        .pending_handshakes
        .try_acquire()
        .context("too many pending handshakes")?;
    let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, connection.accept_bi())
        .await
        .context("timed out accepting MoQ control stream")?
        .context("accepting MoQ control stream")?;
    let hello = receive_hello(&mut recv, Capability::VideoH264).await?;
    drop(handshake_permit);
    debug!(%remote, agent = %hello.agent, "MoQ control hello received");

    let grants = match authorization.authorize_or_redeem(
        remote,
        hello.invitation.as_deref(),
        unix_timestamp_now()?,
    ) {
        Ok(grants) => grants,
        Err(error) => {
            send_rejection(&mut send, "Portal peer is not authorized").await?;
            return Err(error.context("authorizing MoQ control peer"));
        }
    };
    ensure!(
        grants.contains(InvitationGrants::VIEW),
        "authorized MoQ control peer lacks view permission"
    );
    let lease = match sessions.claim(remote, hello.nonce, grants) {
        Ok(lease) => lease,
        Err(error) => {
            send_rejection(&mut send, "host already has an active client").await?;
            return Err(error);
        }
    };

    let source = match config.source {
        VideoSource::TestPattern => Ok(spawn_test_pattern(config.clone(), lease.session_clock)),
        VideoSource::GamescopePipewire => {
            let primary = spawn_gamescope_pipewire_after_static_preflight(
                config.clone(),
                lease.session_clock,
            )
            .await?;
            select_gamescope_startup_source(config.clone(), lease.session_clock, primary).await
        }
    };
    let EncodedSource {
        frames: frame_receiver,
        current_gop: mut current_gop_receiver,
        task: source_task,
        pointer_surface_dimensions,
        encoder_control,
    } = match source {
        Ok(source) => source,
        Err(error) => {
            send_rejection(&mut send, "video source is unavailable").await?;
            return Err(error);
        }
    };
    let source_task = SourceTaskGuard::new(source_task);
    sessions.install_encoder_control(remote, lease.session_id, encoder_control.clone())?;

    let mut broadcast = Broadcast::new().produce();
    let track = broadcast
        .create_track(Track {
            name: MOQ_VIDEO_H264_TRACK.to_owned(),
            priority: MOQ_VIDEO_TRACK_PRIORITY,
        })
        .context("creating static MoQ H.264 track")?;
    let catalog = publish_goq_catalog(&mut broadcast)?;
    let broadcast_name = media_moq_broadcast_name(lease.session_id)?;
    let attachment = sessions.expect_moq(
        remote,
        lease.session_id,
        broadcast_name.clone(),
        broadcast.consume(),
    )?;

    let mut control_hello = HostHello::accepted(
        lease.session_id,
        negotiated_capabilities(&hello, MEDIA_CAPABILITIES),
    );
    if let Some(dimensions) = pointer_surface_dimensions {
        control_hello = control_hello.with_pointer_surface_dimensions(dimensions);
    }
    write_host_hello(&mut send, &control_hello).await?;
    send.finish().context("finishing MoQ control response")?;
    drop(send);
    info!(
        %remote,
        session_id = lease.session_id,
        %broadcast_name,
        "MoQ control client accepted; awaiting authorized media attachment"
    );

    let MoqAttachmentWait {
        mut attached,
        closed,
    } = attachment;
    tokio::time::timeout(MOQ_ATTACHMENT_TIMEOUT, async {
        tokio::select! {
            result = &mut attached => {
                result.context("authorized MoQ handler ended before attachment")
            }
            reason = connection.closed() => {
                Err(anyhow::anyhow!("control connection closed before MoQ attachment: {reason:?}"))
            }
        }
    })
    .await
    .context("timed out waiting for authorized MoQ attachment")??;

    let session_result = run_control_moq_session(
        &connection,
        &config,
        &mut current_gop_receiver,
        recv,
        remote,
        closed,
        track,
        &mut broadcast,
        encoder_control,
        Arc::clone(&lease.media_v3_telemetry),
    )
    .await;
    let catalog_result = catalog.finish();

    drop(current_gop_receiver);
    drop(frame_receiver);
    source_task.wait_or_abort(SOURCE_REAP_GRACE_TIMEOUT).await;
    drop(lease);
    info!(%remote, "MoQ control client released");
    match session_result {
        Err(error) => Err(error),
        Ok(()) => catalog_result,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_control_moq_session(
    connection: &Connection,
    config: &HostConfig,
    current_gop_receiver: &mut tokio::sync::watch::Receiver<Option<EncodedGop>>,
    control_recv: iroh::endpoint::RecvStream,
    remote: EndpointId,
    mut moq_closed: tokio::sync::oneshot::Receiver<()>,
    track: TrackProducer,
    broadcast: &mut BroadcastProducer,
    encoder_control: Option<EncoderControl>,
    telemetry: Arc<MediaV3Telemetry>,
) -> Result<()> {
    let maximum_replay_age = maximum_media_replay_age(config.framerate);
    let mut replay_cursor = MediaReplayCursor::default();
    let mut publisher = MoqGroupPublisher::new(track);
    let (control_sender, mut control_requests) = tokio::sync::watch::channel(None);
    let mut control_task = tokio::spawn(forward_media_v3_control_requests(
        control_recv,
        control_sender,
    ));
    let mut control_task_finished = false;
    let mut control_receiver_open = true;
    let mut forced_idr = ForcedIdrCoordinator::new(encoder_control, Arc::clone(&telemetry));

    let result = async {
        loop {
            tokio::select! {
                biased;
                control_result = &mut control_task, if !control_task_finished => {
                    control_task_finished = true;
                    match control_result {
                        Ok(Ok(())) => {
                            debug!(%remote, "MoQ keyframe-control stream closed");
                        }
                        Ok(Err(error)) => {
                            return Err(error).context("reading MoQ keyframe-control stream");
                        }
                        Err(error) => {
                            return Err(error).context("MoQ keyframe-control task failed");
                        }
                    }
                }
                changed = control_requests.changed(), if control_receiver_open => {
                    if changed.is_err() {
                        control_receiver_open = false;
                        continue;
                    }
                    let Some(request) = *control_requests.borrow_and_update() else {
                        continue;
                    };
                    let through_sequence = current_gop_receiver
                        .borrow()
                        .as_ref()
                        .and_then(|gop| gop.frames.last())
                        .map(|frame| frame.sequence);
                    let cancelled_group = apply_moq_keyframe_request(
                        &mut publisher,
                        &mut replay_cursor,
                        through_sequence,
                        request.reason,
                    );
                    if cancelled_group.is_some() {
                        telemetry
                            .scheduler_cancellations
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    let forced_idr_disposition = forced_idr.request(request.reason);
                    if let ForcedIdrDisposition::Failed { error } = &forced_idr_disposition {
                        warn!(
                            %remote,
                            request_id = request.request_id,
                            ?request.reason,
                            %error,
                            "forced-IDR request failed; retaining natural-IDR fallback"
                        );
                    }
                    debug!(
                        %remote,
                        request_id = request.request_id,
                        ?request.reason,
                        advisory_last_sequence = ?request.last_sequence,
                        coalesced = cancelled_group.is_none(),
                        ?cancelled_group,
                        ?forced_idr_disposition,
                        "accepted MoQ keyframe request"
                    );
                }
                acknowledgement = forced_idr.acknowledgements.join_next(),
                    if forced_idr.pending_revision.is_some() =>
                {
                    forced_idr.complete(acknowledgement, remote, "iroh-moq");
                }
                reason = connection.closed() => {
                    debug!(%remote, ?reason, "MoQ control connection closed");
                    return Ok(());
                }
                result = &mut moq_closed => {
                    debug!(%remote, ?result, "authorized MoQ media attachment closed");
                    return Ok(());
                }
                changed = current_gop_receiver.changed() => {
                    if let Err(error) = changed {
                        return Err(error).context("encoded source stopped");
                    }
                    let Some(current_gop) = current_gop_receiver.borrow_and_update().clone() else {
                        continue;
                    };
                    let initial_replay_started_at =
                        replay_cursor.last_sequence.is_none().then(Instant::now);
                    let replay_through_sequence = current_gop
                        .frames
                        .last()
                        .map(|frame| frame.sequence)
                        .context("current GOP snapshot is empty")?;
                    for frame in new_current_gop_frames(current_gop, replay_cursor.last_sequence) {
                        let replay_discontinuity = match replay_cursor.classify(
                            &frame,
                            replay_through_sequence,
                            initial_replay_started_at,
                            Instant::now(),
                            maximum_replay_age,
                        ) {
                            MediaReplayDecision::Send { discontinuity } => discontinuity,
                            MediaReplayDecision::SkipUntilKeyframe => {
                                if publisher.request_keyframe().is_some() {
                                    telemetry
                                        .scheduler_cancellations
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                                replay_cursor.enter_resync_through(Some(replay_through_sequence));
                                break;
                            }
                            MediaReplayDecision::DiscardStaleSuffix { through_sequence } => {
                                let cancelled_group = publisher.request_keyframe();
                                if cancelled_group.is_some() {
                                    telemetry
                                        .scheduler_cancellations
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                                debug!(
                                    %remote,
                                    through_sequence,
                                    ?cancelled_group,
                                    "cancelled stale MoQ media suffix"
                                );
                                break;
                            }
                        };
                        let decision = publisher
                            .publish(config, &frame, replay_discontinuity)
                            .inspect_err(|_error| {
                                telemetry.send_failures.fetch_add(1, Ordering::Relaxed);
                            })?;
                        match decision {
                            MoqGroupDecision::Published {
                                group_id,
                                frame_id,
                                cancelled_previous,
                            } => {
                                if cancelled_previous {
                                    telemetry
                                        .scheduler_cancellations
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                                debug!(
                                    sequence = frame.sequence,
                                    group_id,
                                    frame_id,
                                    cancelled_previous,
                                    "published upstream MoQ video frame"
                                );
                                replay_cursor.commit_sent(&frame);
                            }
                            MoqGroupDecision::SkipUntilKeyframe => {
                                replay_cursor.enter_resync_through(Some(replay_through_sequence));
                                break;
                            }
                            MoqGroupDecision::EnterResync => {
                                replay_cursor.enter_resync_through(Some(replay_through_sequence));
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    .await;

    forced_idr.abort_and_drain(remote, "iroh-moq").await;
    publisher.abort();
    let _ = broadcast.abort(MoqError::Cancel);
    if !control_task_finished {
        control_task.abort();
        let _ = control_task.await;
    }
    result
}

async fn serve_input(
    connection: Connection,
    backend: &InputBackend,
    pointer_positions: Option<&PointerPositionTracker>,
    sessions: &Arc<SessionRegistry>,
) -> Result<()> {
    let remote = connection.remote_id();
    let handshake_permit = sessions
        .pending_handshakes
        .try_acquire()
        .context("too many pending handshakes")?;
    let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, connection.accept_bi())
        .await
        .context("timed out accepting input stream")?
        .context("accepting input stream")?;
    let hello = receive_hello_unconstrained(&mut recv).await?;
    drop(handshake_permit);
    debug!(%remote, agent = %hello.agent, "input hello received");
    ensure!(
        hello.invitation.is_none(),
        "invitations are accepted only on the first media handshake"
    );
    let lease = match sessions.claim_input(remote, hello.nonce) {
        Ok(lease) => lease,
        Err(error) => {
            send_rejection(&mut send, error.to_string()).await?;
            return Err(error);
        }
    };
    let supported =
        supported_input_capabilities(backend.capabilities(), pointer_positions.is_some());
    let negotiated = negotiated_input_capabilities(&hello, &supported, lease.grants);

    let ack_enabled = negotiated.contains(&Capability::InputAck);
    let feedback_enabled = negotiated.contains(&Capability::PointerPositionFeedback);
    let visibility_feedback_enabled = negotiated.contains(&Capability::PointerVisibilityFeedback);
    let mut pointer_positions = pointer_positions
        .filter(|_| feedback_enabled)
        .map(PointerPositionTracker::subscribe);
    write_host_hello(
        &mut send,
        &HostHello::accepted(lease.session_id, negotiated.clone()),
    )
    .await?;
    info!(%remote, session_id = lease.session_id, "input client accepted");

    let session_result: Result<()> = async {
        let mut received_events = 0_u64;
        if let Some(pointer_positions) = pointer_positions.as_ref() {
            let pointer_state = *pointer_positions.borrow();
            let (pointer_position, pointer_visible) =
                pointer_feedback_fields(Some(pointer_state), visibility_feedback_enabled);
            tokio::time::timeout(
                INPUT_ACK_TIMEOUT,
                write_input_ack(
                    &mut send,
                    &InputAck {
                        sequence: received_events,
                        pointer_position,
                        pointer_visible,
                    },
                ),
            )
            .await
            .context("timed out writing initial pointer position")??;
        }
        loop {
            if !sessions.is_active(remote, lease.session_id) {
                debug!(%remote, session_id = lease.session_id, "media session ended; closing input");
                break;
            }
            tokio::select! {
                _ = sessions.session_changed.notified() => continue,
                changed = async {
                    pointer_positions
                        .as_mut()
                        .expect("feedback branch is guarded")
                        .changed()
                        .await
                }, if pointer_positions.is_some() => {
                    changed.context("Xwayland pointer tracker stopped")?;
                    let pointer_state = {
                        let receiver = pointer_positions
                            .as_mut()
                            .expect("feedback branch is guarded");
                        *receiver.borrow_and_update()
                    };
                    let (pointer_position, pointer_visible) =
                        pointer_feedback_fields(Some(pointer_state), visibility_feedback_enabled);
                    tokio::time::timeout(
                        INPUT_ACK_TIMEOUT,
                        write_input_ack(
                            &mut send,
                            &InputAck {
                                sequence: received_events,
                                pointer_position,
                                pointer_visible,
                            },
                        ),
                    )
                    .await
                    .context("timed out writing pointer position feedback")??;
                }
                event = read_input_event(&mut recv) => {
                    let Some(event) = event? else {
                        break;
                    };
                    if !sessions.is_active(remote, lease.session_id) {
                        debug!(%remote, session_id = lease.session_id, "discarding input after media ended");
                        break;
                    }
                    match backend.apply(&event, &negotiated)? {
                        InputDisposition::Probed => {
                            debug!(%remote, "input liveness probe acknowledged");
                        }
                        InputDisposition::Observed => {
                            info!(%remote, event_type = input_event_type(&event), "input event observed");
                        }
                        InputDisposition::Disabled => {
                            debug!(%remote, event_type = input_event_type(&event), "input event ignored because injection is disabled");
                        }
                        #[cfg(target_os = "linux")]
                        InputDisposition::Injected => {
                            debug!(%remote, event_type = input_event_type(&event), "input event injected");
                        }
                        InputDisposition::TextIgnored => {
                            debug!(%remote, event_type = "text", "text input is unsupported and was ignored");
                        }
                    }
                    received_events = received_events.saturating_add(1);
                    if ack_enabled {
                        let pointer_state = pointer_positions
                            .as_ref()
                            .map(|positions| *positions.borrow());
                        let (pointer_position, pointer_visible) =
                            pointer_feedback_fields(pointer_state, visibility_feedback_enabled);
                        tokio::time::timeout(
                            INPUT_ACK_TIMEOUT,
                            write_input_ack(
                                &mut send,
                                &InputAck {
                                    sequence: received_events,
                                    pointer_position,
                                    pointer_visible,
                                },
                            ),
                        )
                        .await
                        .context("timed out writing input acknowledgment")??;
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    let reset_result = backend
        .reset_session()
        .context("releasing held input transitions at session end");
    let result = session_result.and(reset_result);
    drop(lease);
    info!(%remote, "input client released");
    result
}

async fn serve_audio(
    connection: Connection,
    config: HostConfig,
    sessions: &Arc<SessionRegistry>,
) -> Result<()> {
    let remote = connection.remote_id();
    let handshake_permit = sessions
        .pending_handshakes
        .try_acquire()
        .context("too many pending handshakes")?;
    let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, connection.accept_bi())
        .await
        .context("timed out accepting audio handshake stream")?
        .context("accepting audio handshake stream")?;
    let hello = receive_hello(&mut recv, Capability::AudioOpus).await?;
    drop(handshake_permit);
    debug!(%remote, agent = %hello.agent, "audio hello received");
    ensure!(
        hello.invitation.is_none(),
        "invitations are accepted only on the first media handshake"
    );

    if config.audio.is_none() {
        send_rejection(&mut send, "audio is unavailable").await?;
        bail!("audio is not configured");
    }
    let maximum_datagram = connection.max_datagram_size();
    if maximum_datagram.is_none_or(|maximum| maximum < AUDIO_HEADER_LEN + MAX_AUDIO_PAYLOAD_LEN) {
        send_rejection(&mut send, "peer cannot carry v1 audio datagrams").await?;
        bail!(
            "peer audio datagram limit {:?} is below {}",
            maximum_datagram,
            AUDIO_HEADER_LEN + MAX_AUDIO_PAYLOAD_LEN
        );
    }
    let lease = match sessions.claim_audio(remote, hello.nonce) {
        Ok(lease) => lease,
        Err(error) => {
            send_rejection(&mut send, error.to_string()).await?;
            return Err(error);
        }
    };
    ensure!(
        lease.grants.contains(InvitationGrants::VIEW),
        "active Portal session lacks audio view permission"
    );
    let (mut audio_receiver, audio_task) =
        match spawn_pipewire_audio(config, lease.session_clock).await {
            Ok(source) => source,
            Err(error) => {
                send_rejection(&mut send, "audio source is unavailable").await?;
                return Err(error);
            }
        };
    let audio_task = SourceTaskGuard::new(audio_task);
    write_host_hello(
        &mut send,
        &HostHello::accepted(
            lease.session_id,
            negotiated_capabilities(&hello, AUDIO_CAPABILITIES),
        ),
    )
    .await?;
    send.finish()?;
    info!(%remote, session_id = lease.session_id, "audio client accepted");

    let session_result: Result<()> = async {
        loop {
            if !sessions.is_active(remote, lease.session_id) {
                break;
            }
            tokio::select! {
                _ = sessions.session_changed.notified() => continue,
                packet = audio_receiver.recv() => {
                    let packet = packet.context("audio source stopped")?;
                    let flags = if packet.discontinuity {
                        AudioFlags::DISCONTINUITY
                    } else {
                        AudioFlags::NONE
                    };
                    let header = AudioPacketHeader::opus(
                        packet.payload.len(),
                        packet.sequence,
                        packet.capture_timestamp_us,
                        packet.pts_us,
                        flags,
                    )?;
                    let datagram = AudioPacket::new(header, packet.payload.as_ref().to_vec())?
                        .encode_datagram()?;
                    match connection.send_datagram(datagram.into()) {
                        Ok(()) => {}
                        Err(error) => {
                            // The non-waiting API bounds the QUIC datagram buffer by
                            // evicting stale datagrams. Its errors mean the negotiated
                            // path cannot carry the fixed v1 packet and are terminal.
                            return Err(error).context("sending bounded audio datagram");
                        }
                    }
                }
                result = connection.closed() => {
                    debug!(%remote, ?result, "audio connection closed");
                    break;
                }
            }
        }
        Ok(())
    }
    .await;
    audio_task.abort_and_wait().await;
    drop(lease);
    info!(%remote, "audio client released");
    session_result
}

async fn send_rejection(
    send: &mut iroh::endpoint::SendStream,
    message: impl Into<String>,
) -> Result<()> {
    write_host_hello(send, &HostHello::rejected(message)).await?;
    send.finish()?;
    if tokio::time::timeout(REJECTION_DRAIN_TIMEOUT, send.stopped())
        .await
        .is_err()
    {
        debug!("timed out waiting for peer to acknowledge handshake rejection");
    }
    Ok(())
}

fn negotiated_capabilities(hello: &ClientHello, supported: &[Capability]) -> Vec<Capability> {
    supported
        .iter()
        .copied()
        .filter(|capability| hello.capabilities.contains(capability))
        .collect()
}

fn negotiated_input_capabilities(
    hello: &ClientHello,
    supported: &[Capability],
    grants: InvitationGrants,
) -> Vec<Capability> {
    let mut negotiated = negotiated_capabilities(hello, supported);
    negotiated.retain(|capability| input_capability_authorized(*capability, grants));
    if !negotiated.contains(&Capability::PointerPositionFeedback) {
        negotiated.retain(|capability| *capability != Capability::PointerVisibilityFeedback);
    }
    negotiated
}

fn input_capability_authorized(capability: Capability, grants: InvitationGrants) -> bool {
    match capability {
        Capability::AbsolutePointer
        | Capability::RelativePointer
        | Capability::Keyboard
        | Capability::Text
        | Capability::PointerPositionFeedback
        | Capability::PointerVisibilityFeedback => {
            grants.contains(InvitationGrants::POINTER_KEYBOARD)
        }
        Capability::Gamepad => grants.contains(InvitationGrants::GAMEPAD),
        Capability::InputAck => {
            grants.contains(InvitationGrants::POINTER_KEYBOARD)
                || grants.contains(InvitationGrants::GAMEPAD)
        }
        Capability::VideoH264 | Capability::AudioOpus => false,
    }
}

fn pointer_feedback_fields(
    pointer_state: Option<PointerState>,
    visibility_feedback_enabled: bool,
) -> (Option<sigil_protocol::PointerPosition>, Option<bool>) {
    match pointer_state {
        Some(state) if visibility_feedback_enabled => (state.position, Some(state.visible)),
        Some(state) => (state.position.filter(|_| state.visible), None),
        None => (None, None),
    }
}

fn supported_input_capabilities(
    backend: &[Capability],
    pointer_feedback_available: bool,
) -> Vec<Capability> {
    let mut supported = backend.to_vec();
    if pointer_feedback_available && supported.contains(&Capability::RelativePointer) {
        supported.push(Capability::PointerPositionFeedback);
        supported.push(Capability::PointerVisibilityFeedback);
    }
    supported
}

fn input_event_type(event: &sigil_protocol::InputEvent) -> &'static str {
    match event {
        sigil_protocol::InputEvent::Probe => "probe",
        sigil_protocol::InputEvent::MouseMove { .. } => "mouse-move",
        sigil_protocol::InputEvent::MouseMoveRelative { .. } => "mouse-move-relative",
        sigil_protocol::InputEvent::MousePositionSync { .. } => "mouse-position-sync",
        sigil_protocol::InputEvent::MouseClick { .. } => "mouse-click",
        sigil_protocol::InputEvent::MouseDown { .. } => "mouse-down",
        sigil_protocol::InputEvent::MouseUp { .. } => "mouse-up",
        sigil_protocol::InputEvent::MouseScroll { .. } => "mouse-scroll",
        sigil_protocol::InputEvent::KeyDown { .. } => "key-down",
        sigil_protocol::InputEvent::KeyUp { .. } => "key-up",
        sigil_protocol::InputEvent::KeyClick { .. } => "key-click",
        sigil_protocol::InputEvent::Text { .. } => "text",
        sigil_protocol::InputEvent::Gamepad { .. } => "gamepad-snapshot",
    }
}

async fn receive_hello<R>(reader: &mut R, required: Capability) -> Result<ClientHello>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_client_hello(reader))
        .await
        .context("timed out waiting for client hello")??
        .context("client closed before hello")?;
    ensure!(
        hello.capabilities.contains(&required),
        "client did not offer required capability {required:?}"
    );
    Ok(hello)
}

async fn receive_hello_unconstrained<R>(reader: &mut R) -> Result<ClientHello>
where
    R: tokio::io::AsyncRead + Unpin,
{
    tokio::time::timeout(HANDSHAKE_TIMEOUT, read_client_hello(reader))
        .await
        .context("timed out waiting for client hello")??
        .context("client closed before hello")
}

pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        error!(%info, "host panic");
    }));
}

#[cfg(test)]
fn endpoint(byte: u8) -> EndpointId {
    iroh::SecretKey::from_bytes(&[byte; 32]).public()
}

#[cfg(test)]
fn moq_test_config() -> HostConfig {
    HostConfig {
        identity_path: "identity".into(),
        state_path: "state".into(),
        source: VideoSource::TestPattern,
        width: Some(1280),
        height: Some(800),
        framerate: 60,
        codec: "h264".to_owned(),
        input_mode: crate::config::InputMode::Disabled,
        uinput: None,
        ffmpeg_path: "ffmpeg".into(),
        gamescope_pipewire: None,
        audio: None,
    }
}

#[cfg(test)]
fn media_v3_encoded_frame(
    sequence: u64,
    keyframe: bool,
    codec_config: bool,
    payload_len: usize,
) -> EncodedFrame {
    EncodedFrame {
        sequence,
        width: 1_280,
        height: 800,
        capture_timestamp_micros: sequence,
        presentation_timestamp_micros: sequence as i64,
        observed_at: Instant::now(),
        keyframe,
        codec_config,
        discontinuity: false,
        data: Arc::from(vec![sequence as u8; payload_len]),
    }
}

#[cfg(test)]
struct DropNotify(Option<tokio::sync::oneshot::Sender<()>>);

#[cfg(test)]
impl Drop for DropNotify {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn upstream_moq_groups_are_sequential_and_cancel_the_superseded_gop() {
        let track_info = Track {
            name: MOQ_VIDEO_H264_TRACK.to_owned(),
            priority: MOQ_VIDEO_TRACK_PRIORITY,
        };
        let mut broadcast = Broadcast::new().produce();
        let track = broadcast.create_track(track_info.clone()).unwrap();
        let mut consumer = broadcast.consume().subscribe_track(&track_info).unwrap();
        let mut publisher = MoqGroupPublisher::new(track);
        let config = moq_test_config();

        assert_eq!(
            publisher
                .publish(&config, &media_v3_encoded_frame(100, true, true, 4), false)
                .unwrap(),
            MoqGroupDecision::Published {
                group_id: 0,
                frame_id: 0,
                cancelled_previous: false,
            }
        );
        let mut first_group = consumer.recv_group().await.unwrap().unwrap();
        assert_eq!(first_group.sequence, 0);
        assert!(first_group.read_frame().await.unwrap().is_some());

        assert_eq!(
            publisher
                .publish(
                    &config,
                    &media_v3_encoded_frame(101, false, false, 4),
                    false,
                )
                .unwrap(),
            MoqGroupDecision::Published {
                group_id: 0,
                frame_id: 1,
                cancelled_previous: false,
            }
        );
        assert!(first_group.read_frame().await.unwrap().is_some());

        assert_eq!(
            publisher
                .publish(&config, &media_v3_encoded_frame(200, true, true, 4), false)
                .unwrap(),
            MoqGroupDecision::Published {
                group_id: 1,
                frame_id: 0,
                cancelled_previous: true,
            }
        );
        assert!(first_group.finished().await.is_err());
        let mut second_group = consumer.recv_group().await.unwrap().unwrap();
        assert_eq!(second_group.sequence, 1);
        let object = second_group.read_frame().await.unwrap().unwrap();
        let frame = sigil_protocol::decode_media_frame_object(&object).unwrap();
        assert_eq!(frame.header.sequence, 200);
        assert!(frame.header.flags.contains(FrameFlags::DISCONTINUITY));
    }

    #[tokio::test]
    async fn upstream_moq_late_join_preserves_active_static_group() {
        let track_info = Track {
            name: MOQ_VIDEO_H264_TRACK.to_owned(),
            priority: MOQ_VIDEO_TRACK_PRIORITY,
        };
        let mut broadcast = Broadcast::new().produce();
        let track = broadcast.create_track(track_info.clone()).unwrap();
        let mut consumer = broadcast.consume().subscribe_track(&track_info).unwrap();
        let mut publisher = MoqGroupPublisher::new(track);
        let mut replay_cursor = MediaReplayCursor::default();
        let config = moq_test_config();
        let keyframe = media_v3_encoded_frame(10, true, true, 1);
        let first_delta = media_v3_encoded_frame(11, false, false, 1);
        let next_delta = media_v3_encoded_frame(12, false, false, 1);

        publisher.publish(&config, &keyframe, false).unwrap();
        replay_cursor.commit_sent(&keyframe);
        let mut active_group = consumer.recv_group().await.unwrap().unwrap();
        assert!(active_group.read_frame().await.unwrap().is_some());

        publisher.publish(&config, &first_delta, false).unwrap();
        replay_cursor.commit_sent(&first_delta);
        assert!(active_group.read_frame().await.unwrap().is_some());

        assert_eq!(
            apply_moq_keyframe_request(
                &mut publisher,
                &mut replay_cursor,
                Some(first_delta.sequence),
                KeyframeRequestReasonV3::Join,
            ),
            None
        );
        assert_eq!(replay_cursor.last_sequence, Some(first_delta.sequence));
        assert!(!replay_cursor.waiting_for_keyframe);
        assert_eq!(
            publisher.publish(&config, &next_delta, false).unwrap(),
            MoqGroupDecision::Published {
                group_id: 0,
                frame_id: 2,
                cancelled_previous: false,
            }
        );
        assert!(active_group.read_frame().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn upstream_moq_resync_aborts_current_group_and_waits_for_configured_idr() {
        let track_info = Track {
            name: MOQ_VIDEO_H264_TRACK.to_owned(),
            priority: MOQ_VIDEO_TRACK_PRIORITY,
        };
        let mut broadcast = Broadcast::new().produce();
        let track = broadcast.create_track(track_info.clone()).unwrap();
        let mut consumer = broadcast.consume().subscribe_track(&track_info).unwrap();
        let mut publisher = MoqGroupPublisher::new(track);
        let config = moq_test_config();

        publisher
            .publish(&config, &media_v3_encoded_frame(10, true, true, 1), false)
            .unwrap();
        let mut cancelled = consumer.recv_group().await.unwrap().unwrap();
        assert_eq!(publisher.request_keyframe(), Some(0));
        assert!(cancelled.finished().await.is_err());
        assert_eq!(
            publisher
                .publish(&config, &media_v3_encoded_frame(11, false, false, 1), false,)
                .unwrap(),
            MoqGroupDecision::SkipUntilKeyframe
        );
        assert_eq!(
            publisher
                .publish(&config, &media_v3_encoded_frame(20, true, true, 1), false)
                .unwrap(),
            MoqGroupDecision::Published {
                group_id: 1,
                frame_id: 0,
                cancelled_previous: false,
            }
        );
    }

    #[tokio::test]
    async fn upstream_moq_group_counts_envelope_bytes_before_upstream_cache_eviction() {
        let track_info = Track {
            name: MOQ_VIDEO_H264_TRACK.to_owned(),
            priority: MOQ_VIDEO_TRACK_PRIORITY,
        };
        let mut broadcast = Broadcast::new().produce();
        let track = broadcast.create_track(track_info.clone()).unwrap();
        let mut consumer = broadcast.consume().subscribe_track(&track_info).unwrap();
        let mut publisher = MoqGroupPublisher::new(track);
        let config = moq_test_config();
        publisher
            .publish(&config, &media_v3_encoded_frame(10, true, true, 1), false)
            .unwrap();
        let mut cancelled = consumer.recv_group().await.unwrap().unwrap();

        // Payload-only accounting would accept this next one-byte access unit,
        // but its fixed application envelope would overflow moq-net's 32 MiB
        // group cache and silently evict the keyframe.
        publisher.object_bytes = MAX_MEDIA_GROUP_BYTES_V3 - 1;
        assert_eq!(
            publisher
                .publish(&config, &media_v3_encoded_frame(11, false, false, 1), false,)
                .unwrap(),
            MoqGroupDecision::EnterResync
        );
        assert!(cancelled.finished().await.is_err());
        assert_eq!(publisher.object_bytes, 0);
    }

    #[test]
    fn capability_negotiation_is_an_exact_intersection() {
        let hello = ClientHello::new(
            "test",
            [0; 16],
            vec![
                Capability::AbsolutePointer,
                Capability::RelativePointer,
                Capability::Keyboard,
                Capability::Text,
                Capability::Gamepad,
                Capability::VideoH264,
                Capability::AudioOpus,
            ],
        );
        assert_eq!(
            negotiated_capabilities(
                &hello,
                &[
                    Capability::RelativePointer,
                    Capability::Keyboard,
                    Capability::Gamepad,
                ]
            ),
            vec![
                Capability::RelativePointer,
                Capability::Keyboard,
                Capability::Gamepad,
            ]
        );
        assert!(negotiated_capabilities(&hello, &[Capability::InputAck]).is_empty());
        assert_eq!(
            negotiated_capabilities(&hello, MEDIA_CAPABILITIES),
            vec![Capability::VideoH264]
        );
        assert_eq!(
            negotiated_capabilities(&hello, AUDIO_CAPABILITIES),
            vec![Capability::AudioOpus]
        );
    }

    #[test]
    fn enrollment_grants_are_a_strict_input_capability_ceiling() {
        let hello = ClientHello::new(
            "test",
            [0; 16],
            vec![
                Capability::RelativePointer,
                Capability::AbsolutePointer,
                Capability::Keyboard,
                Capability::Text,
                Capability::Gamepad,
                Capability::InputAck,
                Capability::PointerPositionFeedback,
                Capability::PointerVisibilityFeedback,
            ],
        );
        let supported = hello.capabilities.clone();

        assert!(
            negotiated_input_capabilities(&hello, &supported, InvitationGrants::VIEW).is_empty()
        );
        assert_eq!(
            negotiated_input_capabilities(
                &hello,
                &supported,
                InvitationGrants::VIEW.union(InvitationGrants::POINTER_KEYBOARD),
            ),
            vec![
                Capability::RelativePointer,
                Capability::AbsolutePointer,
                Capability::Keyboard,
                Capability::Text,
                Capability::InputAck,
                Capability::PointerPositionFeedback,
                Capability::PointerVisibilityFeedback,
            ]
        );
        assert_eq!(
            negotiated_input_capabilities(
                &hello,
                &supported,
                InvitationGrants::VIEW.union(InvitationGrants::GAMEPAD),
            ),
            vec![Capability::Gamepad, Capability::InputAck]
        );
    }

    #[test]
    fn pointer_feedback_is_advertised_only_with_tracker_and_relative_input() {
        assert_eq!(
            supported_input_capabilities(&[Capability::RelativePointer], false),
            vec![Capability::RelativePointer]
        );
        assert_eq!(
            supported_input_capabilities(&[Capability::RelativePointer], true),
            vec![
                Capability::RelativePointer,
                Capability::PointerPositionFeedback,
                Capability::PointerVisibilityFeedback,
            ]
        );
        assert_eq!(
            supported_input_capabilities(&[Capability::InputAck], true),
            vec![Capability::InputAck]
        );
    }

    #[test]
    fn old_pointer_feedback_client_gets_legacy_host_hello_and_ack_shape() {
        let hello = ClientHello::new(
            "old-client",
            [0; 16],
            vec![
                Capability::RelativePointer,
                Capability::PointerPositionFeedback,
            ],
        );
        let supported = supported_input_capabilities(&[Capability::RelativePointer], true);
        let negotiated = negotiated_input_capabilities(&hello, &supported, InvitationGrants::ALL);
        assert_eq!(
            negotiated,
            vec![
                Capability::RelativePointer,
                Capability::PointerPositionFeedback,
            ]
        );
        assert_eq!(
            serde_json::to_string(&HostHello::accepted(7, negotiated)).unwrap(),
            r#"{"version":1,"accepted":true,"session_id":7,"capabilities":["relative_pointer","pointer_position_feedback"],"message":null}"#
        );

        let position = sigil_protocol::PointerPosition { x: 320, y: 200 };
        let (pointer_position, pointer_visible) = pointer_feedback_fields(
            Some(PointerState {
                position: Some(position),
                visible: true,
            }),
            false,
        );
        assert_eq!(
            serde_json::to_string(&InputAck {
                sequence: 1,
                pointer_position,
                pointer_visible,
            })
            .unwrap(),
            r#"{"sequence":1,"pointer_position":{"x":320,"y":200}}"#
        );

        let (pointer_position, pointer_visible) = pointer_feedback_fields(
            Some(PointerState {
                position: Some(position),
                visible: false,
            }),
            false,
        );
        assert_eq!(
            serde_json::to_string(&InputAck {
                sequence: 1,
                pointer_position,
                pointer_visible,
            })
            .unwrap(),
            r#"{"sequence":1}"#
        );
    }

    #[test]
    fn pointer_visibility_feedback_requires_position_feedback() {
        let visibility_only = ClientHello::new(
            "invalid-client",
            [0; 16],
            vec![Capability::PointerVisibilityFeedback],
        );
        let supported = supported_input_capabilities(&[Capability::RelativePointer], true);
        assert!(
            negotiated_input_capabilities(&visibility_only, &supported, InvitationGrants::ALL)
                .is_empty()
        );

        let upgraded = ClientHello::new(
            "upgraded-client",
            [0; 16],
            vec![
                Capability::RelativePointer,
                Capability::PointerPositionFeedback,
                Capability::PointerVisibilityFeedback,
            ],
        );
        assert_eq!(
            negotiated_input_capabilities(&upgraded, &supported, InvitationGrants::ALL),
            supported
        );

        let position = sigil_protocol::PointerPosition { x: 320, y: 200 };
        assert_eq!(
            pointer_feedback_fields(
                Some(PointerState {
                    position: Some(position),
                    visible: false,
                }),
                true,
            ),
            (Some(position), Some(false))
        );
    }

    #[test]
    fn initial_current_gop_replay_preserves_complete_startup_snapshot() {
        let observed_now = Instant::now();
        let maximum_replay_age = maximum_media_replay_age(60);
        let old_observation = observed_now - Duration::from_secs(1);
        let frame = |sequence, keyframe| EncodedFrame {
            sequence,
            width: 1_280,
            height: 800,
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: old_observation,
            keyframe,
            codec_config: keyframe,
            discontinuity: false,
            data: Arc::from([sequence as u8]),
        };
        let mut cursor = MediaReplayCursor::default();

        let keyframe = frame(10, true);
        assert_eq!(
            cursor.classify(
                &keyframe,
                12,
                Some(observed_now),
                observed_now,
                maximum_replay_age,
            ),
            MediaReplayDecision::Send {
                discontinuity: false,
            }
        );
        cursor.commit_sent(&keyframe);

        let delta = frame(11, false);
        assert_eq!(
            cursor.classify(
                &delta,
                12,
                Some(observed_now),
                observed_now,
                maximum_replay_age,
            ),
            MediaReplayDecision::Send {
                discontinuity: false,
            }
        );
    }

    #[test]
    fn stalled_initial_current_gop_discards_its_remaining_suffix() {
        let replay_started_at = Instant::now();
        let maximum_replay_age = maximum_media_replay_age(60);
        let old_observation = replay_started_at - Duration::from_secs(1);
        let frame = |sequence, keyframe| EncodedFrame {
            sequence,
            width: 1_280,
            height: 800,
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: old_observation,
            keyframe,
            codec_config: keyframe,
            discontinuity: false,
            data: Arc::from([sequence as u8]),
        };
        let mut cursor = MediaReplayCursor::default();

        let keyframe = frame(10, true);
        assert_eq!(
            cursor.classify(
                &keyframe,
                12,
                Some(replay_started_at),
                replay_started_at,
                maximum_replay_age,
            ),
            MediaReplayDecision::Send {
                discontinuity: false,
            }
        );
        cursor.commit_sent(&keyframe);

        let stalled_now = replay_started_at + maximum_replay_age + Duration::from_nanos(1);
        let delta = frame(11, false);
        assert_eq!(
            cursor.classify(
                &delta,
                12,
                Some(replay_started_at),
                stalled_now,
                maximum_replay_age,
            ),
            MediaReplayDecision::DiscardStaleSuffix {
                through_sequence: 12,
            }
        );
        assert_eq!(cursor.last_sequence, Some(12));
        assert!(cursor.waiting_for_keyframe);
        assert!(cursor.discontinuity_pending);
    }

    #[test]
    fn fresh_current_gop_suffix_replays_normally() {
        let observed_now = Instant::now();
        let maximum_replay_age = maximum_media_replay_age(60);
        assert_eq!(maximum_replay_age, Duration::from_nanos(33_333_334));
        let frame = EncodedFrame {
            sequence: 11,
            width: 1_280,
            height: 800,
            capture_timestamp_micros: 11,
            presentation_timestamp_micros: 11,
            observed_at: observed_now - maximum_replay_age,
            keyframe: false,
            codec_config: false,
            discontinuity: false,
            data: Arc::from([11]),
        };
        let mut cursor = MediaReplayCursor {
            last_sequence: Some(10),
            waiting_for_keyframe: false,
            discontinuity_pending: false,
        };

        assert_eq!(
            cursor.classify(&frame, 12, None, observed_now, maximum_replay_age),
            MediaReplayDecision::Send {
                discontinuity: false,
            }
        );
        cursor.commit_sent(&frame);
        assert_eq!(cursor.last_sequence, Some(11));
    }

    #[test]
    fn stale_current_gop_suffix_is_discarded_as_one_bounded_unit() {
        let observed_now = Instant::now();
        let maximum_replay_age = maximum_media_replay_age(60);
        let frame = EncodedFrame {
            sequence: 11,
            width: 1_280,
            height: 800,
            capture_timestamp_micros: 11,
            presentation_timestamp_micros: 11,
            observed_at: observed_now - maximum_replay_age - Duration::from_nanos(1),
            keyframe: false,
            codec_config: false,
            discontinuity: false,
            data: Arc::from([11]),
        };
        let mut cursor = MediaReplayCursor {
            last_sequence: Some(10),
            waiting_for_keyframe: false,
            discontinuity_pending: false,
        };

        assert_eq!(
            cursor.classify(&frame, 13, None, observed_now, maximum_replay_age),
            MediaReplayDecision::DiscardStaleSuffix {
                through_sequence: 13,
            }
        );
        assert_eq!(cursor.last_sequence, Some(13));
        assert!(cursor.waiting_for_keyframe);
        assert!(cursor.discontinuity_pending);
    }

    #[test]
    fn stale_suffix_recovers_only_on_idr_marked_discontinuity() {
        let observed_now = Instant::now();
        let maximum_replay_age = maximum_media_replay_age(60);
        let frame = |sequence, keyframe| EncodedFrame {
            sequence,
            width: 1_280,
            height: 800,
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: observed_now,
            keyframe,
            codec_config: keyframe,
            discontinuity: false,
            data: Arc::from([sequence as u8]),
        };
        let mut cursor = MediaReplayCursor {
            last_sequence: Some(13),
            waiting_for_keyframe: true,
            discontinuity_pending: true,
        };

        let delta = frame(14, false);
        assert_eq!(
            cursor.classify(&delta, 14, None, observed_now, maximum_replay_age),
            MediaReplayDecision::SkipUntilKeyframe
        );
        assert_eq!(cursor.last_sequence, Some(13));

        let idr = frame(15, true);
        assert_eq!(
            cursor.classify(&idr, 15, None, observed_now, maximum_replay_age),
            MediaReplayDecision::Send {
                discontinuity: true,
            }
        );
        cursor.commit_sent(&idr);
        assert_eq!(cursor.last_sequence, Some(15));
        assert!(!cursor.waiting_for_keyframe);
        assert!(!cursor.discontinuity_pending);

        let next_delta = frame(16, false);
        assert_eq!(
            cursor.classify(&next_delta, 16, None, observed_now, maximum_replay_age,),
            MediaReplayDecision::Send {
                discontinuity: false,
            }
        );
    }
}
