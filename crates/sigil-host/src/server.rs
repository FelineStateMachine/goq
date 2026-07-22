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
const MEDIA_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MEDIA_V2_PEER_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const MEDIA_V2_IN_FLIGHT_CAPACITY: usize = 4;
const MEDIA_V2_KEYFRAME_PRIORITY: i32 = 10;
const MEDIA_V2_DELTA_PRIORITY: i32 = 0;
const MEDIA_V2_RESET_CODE: u32 = 0x5356;
const MEDIA_V3_IN_FLIGHT_CAPACITY: usize = 4;
const MEDIA_V3_KEYFRAME_PUBLISHER_PRIORITY: u8 = 0;
const MEDIA_V3_DELTA_PUBLISHER_PRIORITY: u8 = 128;
const MEDIA_V3_KEYFRAME_TRANSPORT_PRIORITY: i32 = 10;
const MEDIA_V3_DELTA_TRANSPORT_PRIORITY: i32 = 0;
// Wire-visible application reset code. Keep stable across compatible v3 releases.
const MEDIA_V3_RESET_CODE: u32 = 0x5357;
const MEDIA_V3_DELIVERY_FRAME_PERIODS: u64 = 4;
const MEDIA_V3_MAX_CONTROL_REQUESTS_PER_SECOND: u32 = 10;
const MEDIA_V3_CONTROL_REQUEST_INTERVAL: Duration =
    Duration::from_millis(1_000 / MEDIA_V3_MAX_CONTROL_REQUESTS_PER_SECOND as u64);
const INPUT_ACK_TIMEOUT: Duration = Duration::from_secs(1);
const REJECTION_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_PENDING_HANDSHAKES: usize = 4;
// Allow one frame of ordinary scheduler/write jitter beyond the frame being
// sent, but never replay a suffix already more than two configured periods old.
const MAX_MEDIA_REPLAY_AGE_FRAME_PERIODS: u64 = 2;
const GAMESCOPE_STARTUP_SAMPLE_WINDOW: Duration = Duration::from_millis(500);
const GAMESCOPE_STARTUP_MIN_OBSERVATION_SPAN: Duration = Duration::from_millis(100);
const GAMESCOPE_STARTUP_TARGET_MIN_FRAMES: u64 = 8;
const GAMESCOPE_STARTUP_TARGET_MIN_FPS: f64 = 45.0;
const SOURCE_REAP_GRACE_TIMEOUT: Duration = Duration::from_secs(1);
const MOQ_ATTACHMENT_TIMEOUT: Duration = Duration::from_secs(10);
const MOQ_REJECT_CODE: u32 = 0x534d;
const ENCODER_CONTROL_COMMIT_TIMEOUT: Duration = Duration::from_secs(2);

mod adaptive;
mod session;

#[allow(unused_imports)]
pub(crate) use adaptive::MotionResolutionPolicy;
pub(crate) use adaptive::VideoDimensions;
use adaptive::serve_media_feedback;

pub use session::SessionRegistry;
use session::{
    ClaimedMoqAttachment, ForcedIdrCoordinator, ForcedIdrDisposition, MediaV3Telemetry,
    MoqAttachmentWait, SourceTaskGuard,
};
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct StartupCadenceSample {
    first_sequence: Option<u64>,
    last_sequence: Option<u64>,
    first_observed_at: Option<Instant>,
    last_observed_at: Option<Instant>,
    receiver_open: bool,
    decodable_gop_ready: bool,
}

impl StartupCadenceSample {
    fn observe(&mut self, frame: &EncodedFrame) {
        if self.first_sequence.is_none() {
            self.first_sequence = Some(frame.sequence);
            self.first_observed_at = Some(frame.observed_at);
        }
        if self
            .last_sequence
            .is_none_or(|sequence| frame.sequence >= sequence)
        {
            self.last_sequence = Some(frame.sequence);
            self.last_observed_at = Some(frame.observed_at);
        }
    }

    fn frame_progress(self) -> u64 {
        self.first_sequence
            .zip(self.last_sequence)
            .map_or(0, |(first, last)| last.saturating_sub(first) + 1)
    }

    fn fps(self) -> f64 {
        let frames = self.frame_progress();
        let elapsed = self.observation_span();
        if frames < 2 || elapsed.is_zero() {
            return 0.0;
        }
        (frames - 1) as f64 / elapsed.as_secs_f64()
    }

    fn observation_span(self) -> Duration {
        self.first_observed_at
            .zip(self.last_observed_at)
            .map_or(Duration::ZERO, |(first, last)| {
                last.saturating_duration_since(first)
            })
    }

    fn has_representative_span(self) -> bool {
        self.observation_span() >= GAMESCOPE_STARTUP_MIN_OBSERVATION_SPAN
    }

    fn is_usable(self) -> bool {
        self.receiver_open && self.frame_progress() > 0 && self.decodable_gop_ready
    }

    fn meets_target_cadence(self) -> bool {
        self.receiver_open
            && self.has_representative_span()
            && self.frame_progress() >= GAMESCOPE_STARTUP_TARGET_MIN_FRAMES
            && self.fps() >= GAMESCOPE_STARTUP_TARGET_MIN_FPS
    }
}

fn startup_source_needs_restart(sample: StartupCadenceSample) -> bool {
    !sample.is_usable()
}

async fn sample_startup_cadence(
    receiver: &mut tokio::sync::watch::Receiver<Option<EncodedFrame>>,
    current_gop: &tokio::sync::watch::Receiver<Option<EncodedGop>>,
    window: Duration,
) -> StartupCadenceSample {
    let mut sample = StartupCadenceSample {
        receiver_open: true,
        ..StartupCadenceSample::default()
    };
    if let Some(frame) = receiver.borrow_and_update().as_ref() {
        sample.observe(frame);
    }
    sample.decodable_gop_ready = current_gop.borrow().is_some();
    let deadline = tokio::time::Instant::now() + window;
    while !sample.is_usable() || !sample.has_representative_span() {
        match tokio::time::timeout_at(deadline, receiver.changed()).await {
            Ok(Ok(())) => {
                if let Some(frame) = receiver.borrow_and_update().as_ref() {
                    sample.observe(frame);
                }
                sample.decodable_gop_ready = current_gop.borrow().is_some();
            }
            Ok(Err(_)) => {
                sample.receiver_open = false;
                break;
            }
            Err(_) => break,
        }
    }
    sample
}

async fn reap_encoded_source(source: EncodedSource) {
    reap_encoded_source_with_timeout(source, SOURCE_REAP_GRACE_TIMEOUT).await;
}

async fn reap_encoded_source_with_timeout(source: EncodedSource, grace_timeout: Duration) {
    let EncodedSource {
        frames,
        current_gop,
        task,
        pointer_surface_dimensions: _,
        encoder_control: _,
    } = source;
    // Closing both bounded outputs gives the capture task a chance to kill and
    // wait for its GStreamer child itself. Abort is only the bounded fallback.
    drop(frames);
    drop(current_gop);
    SourceTaskGuard::new(task)
        .wait_or_abort(grace_timeout)
        .await;
}

async fn select_gamescope_startup_source(
    config: HostConfig,
    session_clock: SessionClock,
    mut primary: EncodedSource,
) -> Result<EncodedSource> {
    let primary_sample = sample_startup_cadence(
        &mut primary.frames,
        &primary.current_gop,
        GAMESCOPE_STARTUP_SAMPLE_WINDOW,
    )
    .await;
    info!(
        frames = primary_sample.frame_progress(),
        fps = primary_sample.fps(),
        receiver_open = primary_sample.receiver_open,
        decodable_gop_ready = primary_sample.decodable_gop_ready,
        target_cadence = primary_sample.meets_target_cadence(),
        "sampled primary Gamescope capture startup"
    );
    if !startup_source_needs_restart(primary_sample) {
        return Ok(primary);
    }

    // Gamescope's PipeWire export does not reliably fan out full cadence to
    // two simultaneous consumers. Reap an unhealthy startup pipeline before
    // opening its replacement; overlapping them can divide vblank deliveries
    // and permanently strand the selected source at half rate.
    reap_encoded_source(primary).await;
    let mut replacement = spawn_gamescope_pipewire_after_static_preflight(config, session_clock)
        .await
        .context("restarting unhealthy Gamescope capture pipeline")?;
    let replacement_sample = sample_startup_cadence(
        &mut replacement.frames,
        &replacement.current_gop,
        GAMESCOPE_STARTUP_SAMPLE_WINDOW,
    )
    .await;
    info!(
        frames = replacement_sample.frame_progress(),
        fps = replacement_sample.fps(),
        receiver_open = replacement_sample.receiver_open,
        decodable_gop_ready = replacement_sample.decodable_gop_ready,
        target_cadence = replacement_sample.meets_target_cadence(),
        "sampled sequential Gamescope capture startup replacement"
    );
    if !replacement_sample.is_usable() {
        let frames = replacement_sample.frame_progress();
        let fps = replacement_sample.fps();
        let receiver_open = replacement_sample.receiver_open;
        let decodable_gop_ready = replacement_sample.decodable_gop_ready;
        reap_encoded_source(replacement).await;
        bail!(
            "replacement Gamescope capture pipeline remained unhealthy: \
             frames={frames}, fps={fps:.2}, receiver_open={receiver_open}, \
             decodable_gop_ready={decodable_gop_ready}"
        );
    }
    Ok(replacement)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaActivity {
    SourceChanged,
    PeerDisconnected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaReplayDecision {
    Send { discontinuity: bool },
    SkipUntilKeyframe,
    DiscardStaleSuffix { through_sequence: u64 },
}

#[derive(Debug, PartialEq, Eq)]
enum MediaV2ScheduleDecision {
    Send {
        discontinuity: bool,
        cancel_sequences: Vec<u64>,
    },
    SkipUntilKeyframe,
    EnterResync {
        cancel_sequences: Vec<u64>,
    },
}

#[derive(Debug)]
struct MediaV2Scheduler {
    in_flight: Vec<u64>,
    last_scheduled_sequence: Option<u64>,
    waiting_for_keyframe: bool,
    discontinuity_pending: bool,
}

impl Default for MediaV2Scheduler {
    fn default() -> Self {
        Self {
            in_flight: Vec::with_capacity(MEDIA_V2_IN_FLIGHT_CAPACITY),
            last_scheduled_sequence: None,
            waiting_for_keyframe: true,
            discontinuity_pending: false,
        }
    }
}

impl MediaV2Scheduler {
    fn schedule(
        &mut self,
        sequence: u64,
        independently_decodable: bool,
    ) -> MediaV2ScheduleDecision {
        if self
            .last_scheduled_sequence
            .is_some_and(|last| sequence <= last)
        {
            return MediaV2ScheduleDecision::SkipUntilKeyframe;
        }

        let sequence_discontinuity = self
            .last_scheduled_sequence
            .is_some_and(|last| last.checked_add(1) != Some(sequence));
        if independently_decodable {
            let cancel_sequences = std::mem::take(&mut self.in_flight);
            let discontinuity = self.discontinuity_pending
                || sequence_discontinuity
                || !cancel_sequences.is_empty();
            self.in_flight.push(sequence);
            self.last_scheduled_sequence = Some(sequence);
            self.waiting_for_keyframe = false;
            self.discontinuity_pending = false;
            return MediaV2ScheduleDecision::Send {
                discontinuity,
                cancel_sequences,
            };
        }

        if self.waiting_for_keyframe {
            return MediaV2ScheduleDecision::SkipUntilKeyframe;
        }
        if sequence_discontinuity || self.in_flight.len() == MEDIA_V2_IN_FLIGHT_CAPACITY {
            return MediaV2ScheduleDecision::EnterResync {
                cancel_sequences: self.enter_resync(),
            };
        }

        self.in_flight.push(sequence);
        self.last_scheduled_sequence = Some(sequence);
        MediaV2ScheduleDecision::Send {
            discontinuity: false,
            cancel_sequences: Vec::new(),
        }
    }

    fn complete(&mut self, sequence: u64) {
        if let Some(index) = self.in_flight.iter().position(|value| *value == sequence) {
            self.in_flight.swap_remove(index);
        }
    }

    fn fail(&mut self, sequence: u64) -> Vec<u64> {
        let Some(index) = self.in_flight.iter().position(|value| *value == sequence) else {
            // A completion from an already-cancelled GOP must not poison the
            // newer GOP which replaced it.
            return Vec::new();
        };
        self.in_flight.swap_remove(index);
        self.enter_resync()
    }

    fn fail_all(&mut self) -> Vec<u64> {
        self.enter_resync()
    }

    fn enter_resync(&mut self) -> Vec<u64> {
        self.waiting_for_keyframe = true;
        self.discontinuity_pending = true;
        std::mem::take(&mut self.in_flight)
    }
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

#[derive(Debug)]
struct MediaV3GroupCursor {
    group_id: Option<u64>,
    last_sequence: Option<u64>,
    last_object_id: Option<u32>,
    payload_bytes: usize,
    waiting_for_keyframe: bool,
    discontinuity_pending: bool,
}

impl Default for MediaV3GroupCursor {
    fn default() -> Self {
        Self {
            group_id: None,
            last_sequence: None,
            last_object_id: None,
            payload_bytes: 0,
            waiting_for_keyframe: true,
            discontinuity_pending: false,
        }
    }
}

impl MediaV3GroupCursor {
    fn classify(&mut self, frame: &EncodedFrame) -> MediaV3GroupDecision {
        let independently_decodable = frame.keyframe && frame.codec_config;
        if independently_decodable {
            if frame.data.len() > MAX_MEDIA_GROUP_BYTES_V3 {
                self.enter_resync();
                return MediaV3GroupDecision::EnterResync;
            }
            let sequence_discontinuity = self
                .last_sequence
                .is_some_and(|last| last.checked_add(1) != Some(frame.sequence));
            let position = MediaV3ObjectPosition {
                group_id: frame.sequence,
                object_id: 0,
                discontinuity: self.discontinuity_pending || sequence_discontinuity,
            };
            self.group_id = Some(frame.sequence);
            self.last_sequence = Some(frame.sequence);
            self.last_object_id = Some(0);
            self.payload_bytes = frame.data.len();
            self.waiting_for_keyframe = false;
            self.discontinuity_pending = false;
            return MediaV3GroupDecision::Send(position);
        }

        if self.waiting_for_keyframe {
            return MediaV3GroupDecision::SkipUntilKeyframe;
        }
        if frame.keyframe || frame.codec_config {
            self.enter_resync();
            return MediaV3GroupDecision::EnterResync;
        }
        let Some(group_id) = self.group_id else {
            self.enter_resync();
            return MediaV3GroupDecision::EnterResync;
        };
        let contiguous =
            self.last_sequence.and_then(|last| last.checked_add(1)) == Some(frame.sequence);
        let object_id = self.last_object_id.and_then(|last| last.checked_add(1));
        let payload_bytes = self.payload_bytes.checked_add(frame.data.len());
        let (Some(object_id), Some(payload_bytes)) = (object_id, payload_bytes) else {
            self.enter_resync();
            return MediaV3GroupDecision::EnterResync;
        };
        if !contiguous
            || object_id > MAX_MEDIA_OBJECT_ID_V3
            || payload_bytes > MAX_MEDIA_GROUP_BYTES_V3
        {
            self.enter_resync();
            return MediaV3GroupDecision::EnterResync;
        }

        self.last_sequence = Some(frame.sequence);
        self.last_object_id = Some(object_id);
        self.payload_bytes = payload_bytes;
        MediaV3GroupDecision::Send(MediaV3ObjectPosition {
            group_id,
            object_id,
            discontinuity: false,
        })
    }

    fn request_keyframe(&mut self) {
        self.enter_resync();
    }

    fn enter_resync(&mut self) {
        self.group_id = None;
        self.last_object_id = None;
        self.payload_bytes = 0;
        self.waiting_for_keyframe = true;
        self.discontinuity_pending = true;
    }
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

#[derive(Debug, PartialEq, Eq)]
enum MediaV3ScheduleDecision {
    Send {
        discontinuity: bool,
        cancel_sequences: Vec<u64>,
    },
    SkipUntilKeyframe,
    EnterResync {
        cancel_sequences: Vec<u64>,
    },
}

#[derive(Debug)]
struct MediaV3Scheduler {
    in_flight: Vec<u64>,
    last_scheduled_sequence: Option<u64>,
    waiting_for_keyframe: bool,
    discontinuity_pending: bool,
}

impl Default for MediaV3Scheduler {
    fn default() -> Self {
        Self {
            in_flight: Vec::with_capacity(MEDIA_V3_IN_FLIGHT_CAPACITY),
            last_scheduled_sequence: None,
            waiting_for_keyframe: true,
            discontinuity_pending: false,
        }
    }
}

impl MediaV3Scheduler {
    fn schedule(
        &mut self,
        sequence: u64,
        independently_decodable: bool,
    ) -> MediaV3ScheduleDecision {
        if self
            .last_scheduled_sequence
            .is_some_and(|last| sequence <= last)
        {
            return MediaV3ScheduleDecision::SkipUntilKeyframe;
        }

        let sequence_discontinuity = self
            .last_scheduled_sequence
            .is_some_and(|last| last.checked_add(1) != Some(sequence));
        if independently_decodable {
            let cancel_sequences = std::mem::take(&mut self.in_flight);
            let discontinuity = self.discontinuity_pending
                || sequence_discontinuity
                || !cancel_sequences.is_empty();
            self.in_flight.push(sequence);
            self.last_scheduled_sequence = Some(sequence);
            self.waiting_for_keyframe = false;
            self.discontinuity_pending = false;
            return MediaV3ScheduleDecision::Send {
                discontinuity,
                cancel_sequences,
            };
        }

        if self.waiting_for_keyframe {
            return MediaV3ScheduleDecision::SkipUntilKeyframe;
        }
        if sequence_discontinuity || self.in_flight.len() == MEDIA_V3_IN_FLIGHT_CAPACITY {
            return MediaV3ScheduleDecision::EnterResync {
                cancel_sequences: self.enter_resync(),
            };
        }

        self.in_flight.push(sequence);
        self.last_scheduled_sequence = Some(sequence);
        MediaV3ScheduleDecision::Send {
            discontinuity: false,
            cancel_sequences: Vec::new(),
        }
    }

    fn complete(&mut self, sequence: u64) {
        if let Some(index) = self.in_flight.iter().position(|value| *value == sequence) {
            self.in_flight.swap_remove(index);
        }
    }

    fn fail(&mut self, sequence: u64) -> Option<Vec<u64>> {
        let index = self.in_flight.iter().position(|value| *value == sequence)?;
        self.in_flight.swap_remove(index);
        Some(self.enter_resync())
    }

    fn fail_all(&mut self) -> Vec<u64> {
        self.enter_resync()
    }

    fn request_keyframe(&mut self) -> Vec<u64> {
        if self.waiting_for_keyframe {
            self.discontinuity_pending = true;
            return Vec::new();
        }
        self.enter_resync()
    }

    fn enter_resync(&mut self) -> Vec<u64> {
        self.waiting_for_keyframe = true;
        self.discontinuity_pending = true;
        std::mem::take(&mut self.in_flight)
    }
}

fn apply_media_v3_keyframe_request(
    scheduler: &mut MediaV3Scheduler,
    group_cursor: &mut MediaV3GroupCursor,
    replay_cursor: &mut MediaReplayCursor,
    through_sequence: Option<u64>,
    reason: KeyframeRequestReasonV3,
) -> (bool, Vec<u64>) {
    // Every v3 session already begins by replaying the bounded current GOP
    // from object zero. A Join arriving after that replay was scheduled must
    // not cancel the only decodable image on a damage-driven static source.
    if reason == KeyframeRequestReasonV3::Join || scheduler.waiting_for_keyframe {
        return (false, Vec::new());
    }
    let cancel_sequences = scheduler.request_keyframe();
    group_cursor.request_keyframe();
    replay_cursor.enter_resync_through(through_sequence);
    (true, cancel_sequences)
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

fn apply_media_v3_send_failure(
    scheduler: &mut MediaV3Scheduler,
    group_cursor: &mut MediaV3GroupCursor,
    replay_cursor: &mut MediaReplayCursor,
    sequence: u64,
    through_sequence: Option<u64>,
) -> Option<Vec<u64>> {
    let cancel_sequences = scheduler.fail(sequence)?;
    group_cursor.request_keyframe();
    replay_cursor.enter_resync_through(through_sequence);
    Some(cancel_sequences)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaV3ControlDecision {
    Accept,
    Pace(Duration),
}

#[derive(Debug, Default)]
struct MediaV3ControlGate {
    last_request_id: Option<u64>,
    last_accepted_at: Option<Instant>,
}

impl MediaV3ControlGate {
    fn accept(
        &mut self,
        request: MediaControlRequestV3,
        now: Instant,
    ) -> Result<MediaV3ControlDecision> {
        ensure!(
            self.last_request_id
                .is_none_or(|last| request.request_id > last),
            "v3 media control request IDs must be strictly increasing"
        );
        self.last_request_id = Some(request.request_id);
        if let Some(last) = self.last_accepted_at {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < MEDIA_V3_CONTROL_REQUEST_INTERVAL {
                return Ok(MediaV3ControlDecision::Pace(
                    MEDIA_V3_CONTROL_REQUEST_INTERVAL - elapsed,
                ));
            }
        }
        self.last_accepted_at = Some(now);
        Ok(MediaV3ControlDecision::Accept)
    }
}

async fn forward_media_v3_control_requests<R>(
    mut reader: R,
    sender: tokio::sync::watch::Sender<Option<MediaControlRequestV3>>,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut gate = MediaV3ControlGate::default();
    while let Some(request) = read_media_control_request_v3(&mut reader).await? {
        match gate.accept(request, Instant::now())? {
            MediaV3ControlDecision::Accept => {}
            MediaV3ControlDecision::Pace(retry_after) => {
                // Stop reading during the rejection interval so QUIC flow
                // control, rather than this task, absorbs an abusive burst.
                // This also bounds rejection logging to the configured rate.
                debug!(
                    request_id = request.request_id,
                    retry_after_ms = retry_after.as_millis(),
                    "paced rate-limited v3 keyframe request"
                );
                tokio::time::sleep(retry_after).await;
                continue;
            }
        }
        sender.send_replace(Some(request));
        if sender.is_closed() {
            return Ok(());
        }
    }
    Ok(())
}

struct ResetOnDropSendStreamV3(Option<SendStream>);

impl ResetOnDropSendStreamV3 {
    fn new(stream: SendStream) -> Self {
        Self(Some(stream))
    }

    fn stream(&self) -> &SendStream {
        self.0.as_ref().expect("send stream guard is armed")
    }

    fn stream_mut(&mut self) -> &mut SendStream {
        self.0.as_mut().expect("send stream guard is armed")
    }

    fn disarm(&mut self) {
        self.0.take();
    }
}

impl Drop for ResetOnDropSendStreamV3 {
    fn drop(&mut self) {
        if let Some(stream) = self.0.as_mut() {
            let _ = stream.reset(MEDIA_V3_RESET_CODE.into());
        }
    }
}

struct ResetOnDropSendStream(Option<SendStream>);

impl ResetOnDropSendStream {
    fn new(stream: SendStream) -> Self {
        Self(Some(stream))
    }

    fn stream(&self) -> &SendStream {
        self.0.as_ref().expect("send stream guard is armed")
    }

    fn stream_mut(&mut self) -> &mut SendStream {
        self.0.as_mut().expect("send stream guard is armed")
    }

    fn disarm(&mut self) {
        self.0.take();
    }
}

impl Drop for ResetOnDropSendStream {
    fn drop(&mut self) {
        if let Some(stream) = self.0.as_mut() {
            let _ = stream.reset(MEDIA_V2_RESET_CODE.into());
        }
    }
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

async fn wait_for_media_activity<T, F>(
    receiver: &mut tokio::sync::watch::Receiver<T>,
    peer_disconnected: Pin<&mut F>,
) -> Result<MediaActivity>
where
    T: Clone + Send + Sync,
    F: Future<Output = ()>,
{
    tokio::select! {
        changed = receiver.changed() => {
            changed.context("encoded source stopped")?;
            Ok(MediaActivity::SourceChanged)
        }
        () = peer_disconnected => Ok(MediaActivity::PeerDisconnected),
    }
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

async fn serve_media(
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
        .context("timed out accepting media stream")?
        .context("accepting media stream")?;
    let hello = receive_hello(&mut recv, Capability::VideoH264).await?;
    drop(handshake_permit);
    debug!(%remote, agent = %hello.agent, "media hello received");

    let grants = match authorization.authorize_or_redeem(
        remote,
        hello.invitation.as_deref(),
        unix_timestamp_now()?,
    ) {
        Ok(grants) => grants,
        Err(error) => {
            send_rejection(&mut send, "Portal peer is not authorized").await?;
            return Err(error.context("authorizing media peer"));
        }
    };
    ensure!(
        grants.contains(InvitationGrants::VIEW),
        "authorized media peer lacks view permission"
    );

    let lease = match sessions.claim(remote, hello.nonce, grants) {
        Ok(lease) => lease,
        Err(error) => {
            send_rejection(&mut send, "host already has an active client").await?;
            return Err(error);
        }
    };

    // `serve` has already completed the static executable and encoder
    // preflight. Resolve the live PipeWire node and create the bounded source
    // before accepting the session so plugin discovery can never sit behind an
    // accepted HostHello and the client's media-idle timeout.
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
    let _encoder_control = encoder_control;

    let mut media_hello = HostHello::accepted(
        lease.session_id,
        negotiated_capabilities(&hello, MEDIA_CAPABILITIES),
    );
    if let Some(dimensions) = pointer_surface_dimensions {
        media_hello = media_hello.with_pointer_surface_dimensions(dimensions);
    }
    write_host_hello(&mut send, &media_hello).await?;
    info!(%remote, session_id = lease.session_id, "media client accepted");

    let session_result: Result<()> = async {
        let maximum_replay_age = maximum_media_replay_age(config.framerate);
        let mut replay_cursor = MediaReplayCursor::default();
        let peer_disconnected = async {
            let result = connection.closed().await;
            debug!(%remote, ?result, "media connection closed");
        };
        tokio::pin!(peer_disconnected);
        loop {
            match wait_for_media_activity(&mut current_gop_receiver, peer_disconnected.as_mut())
                .await?
            {
                MediaActivity::PeerDisconnected => return Ok(()),
                MediaActivity::SourceChanged => {}
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
                let discontinuity = match replay_cursor.classify(
                    &frame,
                    replay_through_sequence,
                    initial_replay_started_at,
                    Instant::now(),
                    maximum_replay_age,
                ) {
                    MediaReplayDecision::Send { discontinuity } => discontinuity,
                    MediaReplayDecision::SkipUntilKeyframe => {
                        debug!(
                            sequence = frame.sequence,
                            "waiting for keyframe with codec configuration"
                        );
                        continue;
                    }
                    MediaReplayDecision::DiscardStaleSuffix { through_sequence } => {
                        debug!(
                            sequence = frame.sequence,
                            through_sequence,
                            replay_age_micros = frame.observed_at.elapsed().as_micros(),
                            maximum_replay_age_micros = maximum_replay_age.as_micros(),
                            "discarding stale media suffix and waiting for keyframe"
                        );
                        break;
                    }
                };
                let media_frame = media_frame_for_encoded(&config, &frame, discontinuity)?;
                tokio::time::timeout(
                    MEDIA_WRITE_TIMEOUT,
                    write_media_frame(&mut send, &media_frame),
                )
                .await
                .context("timed out writing media frame")??;
                replay_cursor.commit_sent(&frame);
            }
        }
    }
    .await;

    drop(current_gop_receiver);
    drop(frame_receiver);
    source_task.wait_or_abort(SOURCE_REAP_GRACE_TIMEOUT).await;
    drop(lease);
    info!(%remote, "media client released");
    session_result
}

async fn serve_media_v2(
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
        .context("timed out accepting media v2 handshake stream")?
        .context("accepting media v2 handshake stream")?;
    let hello = receive_hello(&mut recv, Capability::VideoH264).await?;
    drop(handshake_permit);
    debug!(%remote, agent = %hello.agent, "media v2 hello received");

    let grants = match authorization.authorize_or_redeem(
        remote,
        hello.invitation.as_deref(),
        unix_timestamp_now()?,
    ) {
        Ok(grants) => grants,
        Err(error) => {
            send_rejection(&mut send, "Portal peer is not authorized").await?;
            return Err(error.context("authorizing media v2 peer"));
        }
    };
    ensure!(
        grants.contains(InvitationGrants::VIEW),
        "authorized media peer lacks view permission"
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
    let _encoder_control = encoder_control;

    let mut media_hello = HostHello::accepted(
        lease.session_id,
        negotiated_capabilities(&hello, MEDIA_CAPABILITIES),
    );
    if let Some(dimensions) = pointer_surface_dimensions {
        media_hello = media_hello.with_pointer_surface_dimensions(dimensions);
    }
    write_host_hello(&mut send, &media_hello).await?;
    send.finish()
        .context("finishing media v2 handshake response")?;
    drop(send);
    drop(recv);
    info!(%remote, session_id = lease.session_id, "media v2 client accepted");

    let session_result =
        run_media_v2_session(&connection, &config, &mut current_gop_receiver, remote).await;

    drop(current_gop_receiver);
    drop(frame_receiver);
    source_task.wait_or_abort(SOURCE_REAP_GRACE_TIMEOUT).await;
    drop(lease);
    info!(%remote, "media v2 client released");
    session_result
}

async fn serve_media_v3(
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
        .context("timed out accepting media v3 handshake stream")?
        .context("accepting media v3 handshake stream")?;
    let hello = receive_hello(&mut recv, Capability::VideoH264).await?;
    drop(handshake_permit);
    debug!(%remote, agent = %hello.agent, "media v3 hello received");

    let grants = match authorization.authorize_or_redeem(
        remote,
        hello.invitation.as_deref(),
        unix_timestamp_now()?,
    ) {
        Ok(grants) => grants,
        Err(error) => {
            send_rejection(&mut send, "Portal peer is not authorized").await?;
            return Err(error.context("authorizing media v3 peer"));
        }
    };
    ensure!(
        grants.contains(InvitationGrants::VIEW),
        "authorized media peer lacks view permission"
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

    let mut media_hello = HostHello::accepted(
        lease.session_id,
        negotiated_capabilities(&hello, MEDIA_CAPABILITIES),
    );
    if let Some(dimensions) = pointer_surface_dimensions {
        media_hello = media_hello.with_pointer_surface_dimensions(dimensions);
    }
    write_host_hello(&mut send, &media_hello).await?;
    send.finish()
        .context("finishing media v3 handshake response")?;
    drop(send);
    info!(%remote, session_id = lease.session_id, "media v3 client accepted");

    let session_result = run_media_v3_session(
        &connection,
        &config,
        &mut current_gop_receiver,
        recv,
        remote,
        encoder_control,
        Arc::clone(&lease.media_v3_telemetry),
    )
    .await;

    drop(current_gop_receiver);
    drop(frame_receiver);
    source_task.wait_or_abort(SOURCE_REAP_GRACE_TIMEOUT).await;
    drop(lease);
    info!(%remote, "media v3 client released");
    session_result
}

async fn run_media_v2_session(
    connection: &Connection,
    config: &HostConfig,
    current_gop_receiver: &mut tokio::sync::watch::Receiver<Option<EncodedGop>>,
    remote: EndpointId,
) -> Result<()> {
    let maximum_replay_age = maximum_media_replay_age(config.framerate);
    let mut replay_cursor = MediaReplayCursor::default();
    let mut scheduler = MediaV2Scheduler::default();
    let mut send_tasks = tokio::task::JoinSet::new();

    let result = loop {
        tokio::select! {
            closed = connection.closed() => {
                debug!(%remote, ?closed, "media v2 connection closed");
                break Ok(());
            }
            task = send_tasks.join_next(), if !send_tasks.is_empty() => {
                match task.expect("guarded by non-empty send task set") {
                    Ok((sequence, Ok(()))) => scheduler.complete(sequence),
                    Ok((sequence, Err(error))) => {
                        warn!(sequence, %error, "media v2 object send failed; waiting for keyframe");
                        if !scheduler.fail(sequence).is_empty() {
                            send_tasks.abort_all();
                        }
                    }
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => {
                        warn!(%error, "media v2 object task failed; waiting for keyframe");
                        if !scheduler.fail_all().is_empty() {
                            send_tasks.abort_all();
                        }
                    }
                }
            }
            changed = current_gop_receiver.changed() => {
                if let Err(error) = changed {
                    break Err(error).context("encoded source stopped");
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
                        MediaReplayDecision::SkipUntilKeyframe => continue,
                        MediaReplayDecision::DiscardStaleSuffix { .. } => break,
                    };
                    let independently_decodable = frame.keyframe && frame.codec_config;
                    let (scheduler_discontinuity, cancel_sequences) =
                        match scheduler.schedule(frame.sequence, independently_decodable) {
                            MediaV2ScheduleDecision::Send {
                                discontinuity,
                                cancel_sequences,
                            } => (discontinuity, cancel_sequences),
                            MediaV2ScheduleDecision::SkipUntilKeyframe => continue,
                            MediaV2ScheduleDecision::EnterResync { cancel_sequences } => {
                                if !cancel_sequences.is_empty() {
                                    send_tasks.abort_all();
                                }
                                continue;
                            }
                        };
                    if !cancel_sequences.is_empty() {
                        debug!(
                            sequence = frame.sequence,
                            ?cancel_sequences,
                            "keyframe superseding media v2 objects"
                        );
                        send_tasks.abort_all();
                    }

                    let media_frame = media_frame_for_encoded(
                        config,
                        &frame,
                        replay_discontinuity || scheduler_discontinuity,
                    )?;
                    let sequence = frame.sequence;
                    let keyframe = independently_decodable;
                    // Reserve stream IDs in encoded-frame order. Opening them
                    // inside the concurrent writer tasks would let task
                    // scheduling reorder the QUIC object sequence even though
                    // each object carries a monotonic media sequence number.
                    let stream = match tokio::time::timeout(
                        MEDIA_WRITE_TIMEOUT,
                        connection.open_uni(),
                    )
                    .await
                    {
                        Ok(Ok(stream)) => stream,
                        Ok(Err(error)) => {
                            warn!(sequence, %error, "opening media v2 object stream failed");
                            if !scheduler.fail(sequence).is_empty() {
                                send_tasks.abort_all();
                            }
                            continue;
                        }
                        Err(_) => {
                            warn!(sequence, "opening media v2 object stream timed out");
                            if !scheduler.fail(sequence).is_empty() {
                                send_tasks.abort_all();
                            }
                            continue;
                        }
                    };
                    let stream = ResetOnDropSendStream::new(stream);
                    if let Err(error) = stream
                        .stream()
                        .set_priority(media_v2_priority(keyframe))
                    {
                        warn!(sequence, %error, "setting media v2 object priority failed");
                        if !scheduler.fail(sequence).is_empty() {
                            send_tasks.abort_all();
                        }
                        continue;
                    }
                    send_tasks.spawn(async move {
                        (
                            sequence,
                            send_media_v2_object(stream, media_frame).await,
                        )
                    });
                    replay_cursor.commit_sent(&frame);
                }
            }
        }
    };

    send_tasks.abort_all();
    while send_tasks.join_next().await.is_some() {}
    result
}

fn media_frame_for_encoded(
    _config: &HostConfig,
    frame: &EncodedFrame,
    discontinuity: bool,
) -> Result<MediaFrame> {
    let mut flags = FrameFlags::NONE;
    if frame.keyframe {
        flags = flags.union(FrameFlags::KEYFRAME);
    }
    if frame.codec_config {
        flags = flags.union(FrameFlags::CODEC_CONFIG);
    }
    if discontinuity || frame.discontinuity {
        flags = flags.union(FrameFlags::DISCONTINUITY);
    }
    let header = MediaFrameHeader::h264(
        frame.width,
        frame.height,
        frame.data.len(),
        frame.sequence,
        frame.capture_timestamp_micros,
        frame.presentation_timestamp_micros,
        flags,
    )?;
    MediaFrame::new(header, frame.data.as_ref().to_vec()).map_err(Into::into)
}

async fn send_media_v2_object(mut stream: ResetOnDropSendStream, frame: MediaFrame) -> Result<()> {
    tokio::time::timeout(
        MEDIA_WRITE_TIMEOUT,
        write_media_frame(stream.stream_mut(), &frame),
    )
    .await
    .context("timed out writing media v2 object")??;
    stream
        .stream_mut()
        .finish()
        .context("finishing media v2 object stream")?;
    match tokio::time::timeout(MEDIA_V2_PEER_ACK_TIMEOUT, stream.stream().stopped())
        .await
        .context("timed out waiting for media v2 object acknowledgement")?
        .context("waiting for media v2 object acknowledgement")?
    {
        None => {
            stream.disarm();
            Ok(())
        }
        Some(code) => bail!("peer stopped media v2 object stream with code {code}"),
    }
}

fn media_v2_priority(keyframe: bool) -> i32 {
    if keyframe {
        MEDIA_V2_KEYFRAME_PRIORITY
    } else {
        MEDIA_V2_DELTA_PRIORITY
    }
}

async fn run_media_v3_session(
    connection: &Connection,
    config: &HostConfig,
    current_gop_receiver: &mut tokio::sync::watch::Receiver<Option<EncodedGop>>,
    control_recv: iroh::endpoint::RecvStream,
    remote: EndpointId,
    encoder_control: Option<EncoderControl>,
    telemetry: Arc<MediaV3Telemetry>,
) -> Result<()> {
    let maximum_replay_age = maximum_media_replay_age(config.framerate);
    let delivery_timeout_ms = media_v3_delivery_timeout_ms(config.framerate);
    let mut replay_cursor = MediaReplayCursor::default();
    let mut group_cursor = MediaV3GroupCursor::default();
    let mut scheduler = MediaV3Scheduler::default();
    let mut send_tasks = tokio::task::JoinSet::new();
    let (control_sender, mut control_requests) = tokio::sync::watch::channel(None);
    let mut control_task = tokio::spawn(forward_media_v3_control_requests(
        control_recv,
        control_sender,
    ));
    let mut control_task_finished = false;
    let mut control_receiver_open = true;
    let mut forced_idr = ForcedIdrCoordinator::new(encoder_control, Arc::clone(&telemetry));

    let result = loop {
        telemetry.record_selected_path(connection);
        tokio::select! {
            biased;
            control_result = &mut control_task, if !control_task_finished => {
                control_task_finished = true;
                match control_result {
                    Ok(Ok(())) => {
                        // Keep polling the watch receiver after clean EOF so a
                        // final request published immediately before the
                        // sender closed cannot be lost.
                        debug!(%remote, "media v3 control stream closed");
                    }
                    Ok(Err(error)) => {
                        break Err(error).context("reading media v3 control stream");
                    }
                    Err(error) => {
                        break Err(error).context("media v3 control task failed");
                    }
                }
            }
            changed = control_requests.changed(), if control_receiver_open => {
                if changed.is_err() {
                    control_receiver_open = false;
                    continue;
                }
                let request = *control_requests.borrow_and_update();
                let Some(request) = request else {
                    continue;
                };
                let through_sequence = current_gop_receiver
                    .borrow()
                    .as_ref()
                    .and_then(|gop| gop.frames.last())
                    .map(|frame| frame.sequence);
                let (transitioned, cancel_sequences) = apply_media_v3_keyframe_request(
                    &mut scheduler,
                    &mut group_cursor,
                    &mut replay_cursor,
                    through_sequence,
                    request.reason,
                );
                if !cancel_sequences.is_empty() {
                    telemetry.scheduler_cancellations.fetch_add(
                        u64::try_from(cancel_sequences.len()).unwrap_or(u64::MAX),
                        Ordering::Relaxed,
                    );
                    send_tasks.abort_all();
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
                    coalesced = !transitioned,
                    ?cancel_sequences,
                    ?forced_idr_disposition,
                    "accepted media v3 keyframe request"
                );
            }
            acknowledgement = forced_idr.acknowledgements.join_next(),
                if forced_idr.pending_revision.is_some() =>
            {
                forced_idr.complete(acknowledgement, remote, "grouped-v3");
            }
            closed = connection.closed() => {
                debug!(%remote, ?closed, "media v3 connection closed");
                break Ok(());
            }
            task = send_tasks.join_next(), if !send_tasks.is_empty() => {
                match task.expect("guarded by non-empty send task set") {
                    Ok((sequence, Ok(()))) => scheduler.complete(sequence),
                    Ok((sequence, Err(error))) => {
                        let through_sequence = current_gop_receiver
                            .borrow()
                            .as_ref()
                            .and_then(|gop| gop.frames.last())
                            .map(|frame| frame.sequence);
                        if let Some(cancel_sequences) = apply_media_v3_send_failure(
                            &mut scheduler,
                            &mut group_cursor,
                            &mut replay_cursor,
                            sequence,
                            through_sequence,
                        ) {
                            telemetry.send_failures.fetch_add(1, Ordering::Relaxed);
                            telemetry.scheduler_cancellations.fetch_add(
                                u64::try_from(cancel_sequences.len()).unwrap_or(u64::MAX),
                                Ordering::Relaxed,
                            );
                            warn!(
                                sequence,
                                %error,
                                "media v3 object send failed; waiting for keyframe"
                            );
                            if !cancel_sequences.is_empty() {
                                send_tasks.abort_all();
                            }
                        } else {
                            debug!(
                                sequence,
                                %error,
                                "ignored stale media v3 object failure from a superseded group"
                            );
                        }
                    }
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => {
                        warn!(%error, "media v3 object task failed; waiting for keyframe");
                        let cancel_sequences = scheduler.fail_all();
                        telemetry.send_failures.fetch_add(1, Ordering::Relaxed);
                        telemetry.scheduler_cancellations.fetch_add(
                            u64::try_from(cancel_sequences.len()).unwrap_or(u64::MAX),
                            Ordering::Relaxed,
                        );
                        group_cursor.request_keyframe();
                        let through_sequence = current_gop_receiver
                            .borrow()
                            .as_ref()
                            .and_then(|gop| gop.frames.last())
                            .map(|frame| frame.sequence);
                        replay_cursor.enter_resync_through(through_sequence);
                        if !cancel_sequences.is_empty() {
                            send_tasks.abort_all();
                        }
                    }
                }
            }
            changed = current_gop_receiver.changed() => {
                if let Err(error) = changed {
                    break Err(error).context("encoded source stopped");
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
                            replay_cursor.enter_resync_through(Some(replay_through_sequence));
                            break;
                        }
                        MediaReplayDecision::DiscardStaleSuffix { .. } => {
                            group_cursor.request_keyframe();
                            let cancel_sequences = scheduler.fail_all();
                            telemetry.scheduler_cancellations.fetch_add(
                                u64::try_from(cancel_sequences.len()).unwrap_or(u64::MAX),
                                Ordering::Relaxed,
                            );
                            if !cancel_sequences.is_empty() {
                                send_tasks.abort_all();
                            }
                            break;
                        }
                    };
                    let position = match group_cursor.classify(&frame) {
                        MediaV3GroupDecision::Send(position) => position,
                        MediaV3GroupDecision::SkipUntilKeyframe => {
                            replay_cursor.enter_resync_through(Some(replay_through_sequence));
                            break;
                        }
                        MediaV3GroupDecision::EnterResync => {
                            replay_cursor.enter_resync_through(Some(replay_through_sequence));
                            let cancel_sequences = scheduler.fail_all();
                            telemetry.scheduler_cancellations.fetch_add(
                                u64::try_from(cancel_sequences.len()).unwrap_or(u64::MAX),
                                Ordering::Relaxed,
                            );
                            if !cancel_sequences.is_empty() {
                                send_tasks.abort_all();
                            }
                            break;
                        }
                    };
                    let independently_decodable = position.object_id == 0;
                    let (scheduler_discontinuity, cancel_sequences) =
                        match scheduler.schedule(frame.sequence, independently_decodable) {
                            MediaV3ScheduleDecision::Send {
                                discontinuity,
                                cancel_sequences,
                            } => (discontinuity, cancel_sequences),
                            MediaV3ScheduleDecision::SkipUntilKeyframe => continue,
                            MediaV3ScheduleDecision::EnterResync { cancel_sequences } => {
                                group_cursor.request_keyframe();
                                replay_cursor.enter_resync_through(Some(replay_through_sequence));
                                telemetry.scheduler_cancellations.fetch_add(
                                    u64::try_from(cancel_sequences.len()).unwrap_or(u64::MAX),
                                    Ordering::Relaxed,
                                );
                                if !cancel_sequences.is_empty() {
                                    send_tasks.abort_all();
                                }
                                break;
                            }
                        };
                    if !cancel_sequences.is_empty() {
                        telemetry.scheduler_cancellations.fetch_add(
                            u64::try_from(cancel_sequences.len()).unwrap_or(u64::MAX),
                            Ordering::Relaxed,
                        );
                        debug!(
                            sequence = frame.sequence,
                            group_id = position.group_id,
                            ?cancel_sequences,
                            "configured keyframe superseding media v3 objects"
                        );
                        send_tasks.abort_all();
                    }

                    let media_object = media_v3_object_for_encoded(
                        config,
                        &frame,
                        position,
                        replay_discontinuity
                            || position.discontinuity
                            || scheduler_discontinuity,
                        delivery_timeout_ms,
                    )?;
                    let sequence = frame.sequence;
                    let stream = match tokio::time::timeout(
                        Duration::from_millis(u64::from(delivery_timeout_ms)),
                        connection.open_uni(),
                    )
                    .await
                    {
                        Ok(Ok(stream)) => stream,
                        Ok(Err(error)) => {
                            telemetry.send_failures.fetch_add(1, Ordering::Relaxed);
                            warn!(sequence, %error, "opening media v3 object stream failed");
                            if let Some(cancel_sequences) = apply_media_v3_send_failure(
                                &mut scheduler,
                                &mut group_cursor,
                                &mut replay_cursor,
                                sequence,
                                Some(replay_through_sequence),
                            ) && !cancel_sequences.is_empty()
                            {
                                send_tasks.abort_all();
                            }
                            break;
                        }
                        Err(_) => {
                            telemetry.send_failures.fetch_add(1, Ordering::Relaxed);
                            warn!(sequence, "opening media v3 object stream timed out");
                            if let Some(cancel_sequences) = apply_media_v3_send_failure(
                                &mut scheduler,
                                &mut group_cursor,
                                &mut replay_cursor,
                                sequence,
                                Some(replay_through_sequence),
                            ) && !cancel_sequences.is_empty()
                            {
                                send_tasks.abort_all();
                            }
                            break;
                        }
                    };
                    let stream = ResetOnDropSendStreamV3::new(stream);
                    let transport_priority =
                        media_v3_transport_priority(media_object.header.publisher_priority)?;
                    if let Err(error) = stream.stream().set_priority(transport_priority) {
                        telemetry.send_failures.fetch_add(1, Ordering::Relaxed);
                        warn!(sequence, %error, "setting media v3 object priority failed");
                        if let Some(cancel_sequences) = apply_media_v3_send_failure(
                            &mut scheduler,
                            &mut group_cursor,
                            &mut replay_cursor,
                            sequence,
                            Some(replay_through_sequence),
                        ) && !cancel_sequences.is_empty()
                        {
                            send_tasks.abort_all();
                        }
                        break;
                    }
                    send_tasks.spawn(async move {
                        (
                            sequence,
                            send_media_v3_object(stream, media_object).await,
                        )
                    });
                    replay_cursor.commit_sent(&frame);
                }
            }
        }
    };

    forced_idr.abort_and_drain(remote, "grouped-v3").await;
    send_tasks.abort_all();
    while send_tasks.join_next().await.is_some() {}
    if !control_task_finished {
        control_task.abort();
        let _ = control_task.await;
    }
    result
}

fn media_v3_delivery_timeout_ms(framerate: u32) -> u32 {
    debug_assert!(framerate > 0);
    let timeout_ms = 1_000_u64
        .saturating_mul(MEDIA_V3_DELIVERY_FRAME_PERIODS)
        .div_ceil(u64::from(framerate.max(1)));
    u32::try_from(timeout_ms).unwrap_or(u32::MAX).clamp(
        MIN_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS,
        MAX_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS,
    )
}

fn media_v3_object_for_encoded(
    _config: &HostConfig,
    frame: &EncodedFrame,
    position: MediaV3ObjectPosition,
    discontinuity: bool,
    delivery_timeout_ms: u32,
) -> Result<MediaObjectV3> {
    let mut flags = FrameFlags::NONE;
    if frame.keyframe {
        flags = flags.union(FrameFlags::KEYFRAME);
    }
    if frame.codec_config {
        flags = flags.union(FrameFlags::CODEC_CONFIG);
    }
    if discontinuity || frame.discontinuity {
        flags = flags.union(FrameFlags::DISCONTINUITY);
    }
    let publisher_priority = if position.object_id == 0 {
        MEDIA_V3_KEYFRAME_PUBLISHER_PRIORITY
    } else {
        MEDIA_V3_DELTA_PUBLISHER_PRIORITY
    };
    let header = MediaObjectHeaderV3::h264(
        frame.width,
        frame.height,
        frame.data.len(),
        publisher_priority,
        flags,
        position.object_id,
        position.group_id,
        frame.sequence,
        frame.capture_timestamp_micros,
        frame.presentation_timestamp_micros,
        delivery_timeout_ms,
    )?;
    MediaObjectV3::new(header, frame.data.as_ref().to_vec()).map_err(Into::into)
}

async fn send_media_v3_object(
    mut stream: ResetOnDropSendStreamV3,
    object: MediaObjectV3,
) -> Result<()> {
    let delivery_timeout = Duration::from_millis(u64::from(object.header.delivery_timeout_ms));
    tokio::time::timeout(delivery_timeout, async {
        write_media_object_v3(stream.stream_mut(), &object)
            .await
            .context("writing media v3 object")?;
        stream
            .stream_mut()
            .finish()
            .context("finishing media v3 object stream")?;
        match stream
            .stream()
            .stopped()
            .await
            .context("waiting for media v3 object acknowledgement")?
        {
            None => {
                stream.disarm();
                Ok(())
            }
            Some(code) => bail!("peer stopped media v3 object stream with code {code}"),
        }
    })
    .await
    .context("media v3 object exceeded its delivery timeout")?
}

fn media_v3_transport_priority(publisher_priority: u8) -> Result<i32> {
    // V3 follows MoQ's lower-is-higher publisher priority, while Iroh/QUIC
    // uses larger integers for more important streams. Keep the inversion
    // explicit rather than leaking either convention across the boundary.
    match publisher_priority {
        MEDIA_V3_KEYFRAME_PUBLISHER_PRIORITY => Ok(MEDIA_V3_KEYFRAME_TRANSPORT_PRIORITY),
        MEDIA_V3_DELTA_PUBLISHER_PRIORITY => Ok(MEDIA_V3_DELTA_TRANSPORT_PRIORITY),
        _ => bail!("unsupported media v3 publisher priority {publisher_priority}"),
    }
}

fn new_current_gop_frames(
    current_gop: EncodedGop,
    last_sequence: Option<u64>,
) -> impl Iterator<Item = EncodedFrame> {
    current_gop
        .frames
        .into_iter()
        .skip_while(move |frame| last_sequence.is_some_and(|last| frame.sequence <= last))
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
mod tests {
    use super::*;

    struct DropNotify(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DropNotify {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[test]
    fn media_v2_scheduler_is_bounded_and_drops_a_saturated_delta_suffix() {
        let mut scheduler = MediaV2Scheduler::default();
        assert_eq!(
            scheduler.schedule(10, true),
            MediaV2ScheduleDecision::Send {
                discontinuity: false,
                cancel_sequences: vec![],
            }
        );
        for sequence in 11..14 {
            assert!(matches!(
                scheduler.schedule(sequence, false),
                MediaV2ScheduleDecision::Send { .. }
            ));
        }
        assert_eq!(scheduler.in_flight.len(), MEDIA_V2_IN_FLIGHT_CAPACITY);

        assert_eq!(
            scheduler.schedule(14, false),
            MediaV2ScheduleDecision::EnterResync {
                cancel_sequences: vec![10, 11, 12, 13],
            }
        );
        assert!(scheduler.in_flight.is_empty());
        assert_eq!(
            scheduler.schedule(15, false),
            MediaV2ScheduleDecision::SkipUntilKeyframe
        );
    }

    #[test]
    fn blocked_old_streams_cannot_prevent_a_keyframe_decision() {
        let mut scheduler = MediaV2Scheduler::default();
        for sequence in 20..24 {
            assert!(matches!(
                scheduler.schedule(sequence, sequence == 20),
                MediaV2ScheduleDecision::Send { .. }
            ));
        }
        assert_eq!(scheduler.in_flight.len(), MEDIA_V2_IN_FLIGHT_CAPACITY);

        assert_eq!(
            scheduler.schedule(30, true),
            MediaV2ScheduleDecision::Send {
                discontinuity: true,
                cancel_sequences: vec![20, 21, 22, 23],
            }
        );
        assert_eq!(scheduler.in_flight, vec![30]);
    }

    #[test]
    fn media_v2_send_failure_cancels_dependents_and_resyncs_on_keyframe() {
        let mut scheduler = MediaV2Scheduler::default();
        assert!(matches!(
            scheduler.schedule(40, true),
            MediaV2ScheduleDecision::Send { .. }
        ));
        assert!(matches!(
            scheduler.schedule(41, false),
            MediaV2ScheduleDecision::Send { .. }
        ));

        assert_eq!(scheduler.fail(40), vec![41]);
        assert_eq!(
            scheduler.schedule(42, false),
            MediaV2ScheduleDecision::SkipUntilKeyframe
        );
        assert_eq!(
            scheduler.schedule(50, true),
            MediaV2ScheduleDecision::Send {
                discontinuity: true,
                cancel_sequences: vec![],
            }
        );
        scheduler.complete(50);
        assert!(scheduler.in_flight.is_empty());
        assert!(matches!(
            scheduler.schedule(51, false),
            MediaV2ScheduleDecision::Send {
                discontinuity: false,
                ..
            }
        ));
    }

    #[test]
    fn cancelled_old_completion_cannot_poison_the_replacement_gop() {
        let mut scheduler = MediaV2Scheduler::default();
        assert!(matches!(
            scheduler.schedule(60, true),
            MediaV2ScheduleDecision::Send { .. }
        ));
        assert!(matches!(
            scheduler.schedule(61, false),
            MediaV2ScheduleDecision::Send { .. }
        ));
        assert!(matches!(
            scheduler.schedule(70, true),
            MediaV2ScheduleDecision::Send {
                discontinuity: true,
                ..
            }
        ));

        assert!(scheduler.fail(60).is_empty());
        assert!(matches!(
            scheduler.schedule(71, false),
            MediaV2ScheduleDecision::Send {
                discontinuity: false,
                ..
            }
        ));
    }

    #[test]
    fn media_v2_keyframes_have_strictly_higher_transport_priority() {
        assert!(media_v2_priority(true) > media_v2_priority(false));
    }

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

    #[test]
    fn media_v3_groups_begin_at_configured_idr_and_assign_contiguous_objects() {
        let mut cursor = MediaV3GroupCursor::default();
        assert_eq!(
            cursor.classify(&media_v3_encoded_frame(10, true, true, 4)),
            MediaV3GroupDecision::Send(MediaV3ObjectPosition {
                group_id: 10,
                object_id: 0,
                discontinuity: false,
            })
        );
        assert_eq!(
            cursor.classify(&media_v3_encoded_frame(11, false, false, 3)),
            MediaV3GroupDecision::Send(MediaV3ObjectPosition {
                group_id: 10,
                object_id: 1,
                discontinuity: false,
            })
        );

        cursor.request_keyframe();
        assert_eq!(
            cursor.classify(&media_v3_encoded_frame(12, false, false, 3)),
            MediaV3GroupDecision::SkipUntilKeyframe
        );
        assert_eq!(
            cursor.classify(&media_v3_encoded_frame(20, true, true, 4)),
            MediaV3GroupDecision::Send(MediaV3ObjectPosition {
                group_id: 20,
                object_id: 0,
                discontinuity: true,
            })
        );
    }

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
    fn media_v3_group_rejects_noncontiguous_or_unconfigured_frames() {
        let mut cursor = MediaV3GroupCursor::default();
        assert_eq!(
            cursor.classify(&media_v3_encoded_frame(10, true, false, 1)),
            MediaV3GroupDecision::SkipUntilKeyframe
        );
        assert!(matches!(
            cursor.classify(&media_v3_encoded_frame(20, true, true, 1)),
            MediaV3GroupDecision::Send(_)
        ));
        assert_eq!(
            cursor.classify(&media_v3_encoded_frame(22, false, false, 1)),
            MediaV3GroupDecision::EnterResync
        );
        assert!(cursor.waiting_for_keyframe);
    }

    #[test]
    fn media_v3_group_accepts_exact_limits_and_rejects_overflow() {
        let mut object_cursor = MediaV3GroupCursor::default();
        assert!(matches!(
            object_cursor.classify(&media_v3_encoded_frame(0, true, true, 1)),
            MediaV3GroupDecision::Send(_)
        ));
        for sequence in 1..=u64::from(MAX_MEDIA_OBJECT_ID_V3) {
            assert!(matches!(
                object_cursor.classify(&media_v3_encoded_frame(sequence, false, false, 1)),
                MediaV3GroupDecision::Send(MediaV3ObjectPosition { object_id, .. })
                    if u64::from(object_id) == sequence
            ));
        }
        assert_eq!(
            object_cursor.classify(&media_v3_encoded_frame(
                u64::from(MAX_MEDIA_OBJECT_ID_V3) + 1,
                false,
                false,
                1,
            )),
            MediaV3GroupDecision::EnterResync
        );

        let mut byte_cursor = MediaV3GroupCursor::default();
        assert!(matches!(
            byte_cursor.classify(&media_v3_encoded_frame(10, true, true, 1)),
            MediaV3GroupDecision::Send(_)
        ));
        byte_cursor.payload_bytes = MAX_MEDIA_GROUP_BYTES_V3 - 1;
        assert!(matches!(
            byte_cursor.classify(&media_v3_encoded_frame(11, false, false, 1)),
            MediaV3GroupDecision::Send(_)
        ));
        assert_eq!(byte_cursor.payload_bytes, MAX_MEDIA_GROUP_BYTES_V3);
        assert_eq!(
            byte_cursor.classify(&media_v3_encoded_frame(12, false, false, 1)),
            MediaV3GroupDecision::EnterResync
        );
    }

    #[test]
    fn media_v3_keyframe_requests_cancel_once_and_recover_discontinuously() {
        let mut scheduler = MediaV3Scheduler::default();
        assert!(matches!(
            scheduler.schedule(10, true),
            MediaV3ScheduleDecision::Send { .. }
        ));
        assert!(matches!(
            scheduler.schedule(11, false),
            MediaV3ScheduleDecision::Send { .. }
        ));
        assert_eq!(scheduler.request_keyframe(), vec![10, 11]);
        assert!(scheduler.request_keyframe().is_empty());
        assert_eq!(
            scheduler.schedule(12, false),
            MediaV3ScheduleDecision::SkipUntilKeyframe
        );
        assert_eq!(
            scheduler.schedule(20, true),
            MediaV3ScheduleDecision::Send {
                discontinuity: true,
                cancel_sequences: vec![],
            }
        );
    }

    #[test]
    fn media_v3_join_request_preserves_initial_current_gop_replay() {
        let mut scheduler = MediaV3Scheduler::default();
        let mut group_cursor = MediaV3GroupCursor::default();
        let mut replay_cursor = MediaReplayCursor::default();

        let (transitioned, cancel_sequences) = apply_media_v3_keyframe_request(
            &mut scheduler,
            &mut group_cursor,
            &mut replay_cursor,
            Some(42),
            KeyframeRequestReasonV3::Join,
        );

        assert!(!transitioned);
        assert!(cancel_sequences.is_empty());
        assert_eq!(replay_cursor.last_sequence, None);
        assert!(replay_cursor.waiting_for_keyframe);
        assert!(!replay_cursor.discontinuity_pending);
        assert!(group_cursor.waiting_for_keyframe);
        assert!(!group_cursor.discontinuity_pending);
        assert!(!scheduler.discontinuity_pending);

        assert!(matches!(
            group_cursor.classify(&media_v3_encoded_frame(10, true, true, 1)),
            MediaV3GroupDecision::Send(MediaV3ObjectPosition {
                discontinuity: false,
                ..
            })
        ));
        assert!(matches!(
            scheduler.schedule(10, true),
            MediaV3ScheduleDecision::Send {
                discontinuity: false,
                ..
            }
        ));
    }

    #[test]
    fn media_v3_late_join_preserves_active_static_current_gop() {
        let mut scheduler = MediaV3Scheduler::default();
        let mut group_cursor = MediaV3GroupCursor::default();
        let mut replay_cursor = MediaReplayCursor::default();
        let keyframe = media_v3_encoded_frame(10, true, true, 1);
        let delta = media_v3_encoded_frame(11, false, false, 1);

        assert!(matches!(
            group_cursor.classify(&keyframe),
            MediaV3GroupDecision::Send(_)
        ));
        assert!(matches!(
            scheduler.schedule(keyframe.sequence, true),
            MediaV3ScheduleDecision::Send { .. }
        ));
        replay_cursor.commit_sent(&keyframe);
        assert!(matches!(
            group_cursor.classify(&delta),
            MediaV3GroupDecision::Send(_)
        ));
        assert!(matches!(
            scheduler.schedule(delta.sequence, false),
            MediaV3ScheduleDecision::Send { .. }
        ));
        replay_cursor.commit_sent(&delta);

        let (transitioned, cancel_sequences) = apply_media_v3_keyframe_request(
            &mut scheduler,
            &mut group_cursor,
            &mut replay_cursor,
            Some(delta.sequence),
            KeyframeRequestReasonV3::Join,
        );

        assert!(!transitioned);
        assert!(cancel_sequences.is_empty());
        assert_eq!(scheduler.in_flight, vec![10, 11]);
        assert!(!scheduler.waiting_for_keyframe);
        assert!(!scheduler.discontinuity_pending);
        assert_eq!(group_cursor.group_id, Some(10));
        assert_eq!(group_cursor.last_sequence, Some(11));
        assert!(!group_cursor.waiting_for_keyframe);
        assert_eq!(replay_cursor.last_sequence, Some(11));
        assert!(!replay_cursor.waiting_for_keyframe);
    }

    #[test]
    fn media_v3_stale_old_group_failure_preserves_replacement_group() {
        let mut scheduler = MediaV3Scheduler::default();
        let mut group_cursor = MediaV3GroupCursor::default();
        let mut replay_cursor = MediaReplayCursor::default();
        let old_keyframe = media_v3_encoded_frame(60, true, true, 1);
        let old_delta = media_v3_encoded_frame(61, false, false, 1);
        let replacement = media_v3_encoded_frame(70, true, true, 1);

        for frame in [&old_keyframe, &old_delta] {
            assert!(matches!(
                group_cursor.classify(frame),
                MediaV3GroupDecision::Send(_)
            ));
            assert!(matches!(
                scheduler.schedule(frame.sequence, frame.keyframe && frame.codec_config),
                MediaV3ScheduleDecision::Send { .. }
            ));
            replay_cursor.commit_sent(frame);
        }
        assert!(matches!(
            group_cursor.classify(&replacement),
            MediaV3GroupDecision::Send(_)
        ));
        assert_eq!(
            scheduler.schedule(replacement.sequence, true),
            MediaV3ScheduleDecision::Send {
                discontinuity: true,
                cancel_sequences: vec![60, 61],
            }
        );
        replay_cursor.commit_sent(&replacement);

        assert_eq!(
            apply_media_v3_send_failure(
                &mut scheduler,
                &mut group_cursor,
                &mut replay_cursor,
                old_keyframe.sequence,
                Some(replacement.sequence),
            ),
            None
        );
        assert_eq!(scheduler.in_flight, vec![70]);
        assert!(!scheduler.waiting_for_keyframe);
        assert_eq!(group_cursor.group_id, Some(70));
        assert!(!group_cursor.waiting_for_keyframe);
        assert_eq!(replay_cursor.last_sequence, Some(70));
        assert!(!replay_cursor.waiting_for_keyframe);

        let next_delta = media_v3_encoded_frame(71, false, false, 1);
        assert!(matches!(
            group_cursor.classify(&next_delta),
            MediaV3GroupDecision::Send(MediaV3ObjectPosition {
                group_id: 70,
                object_id: 1,
                ..
            })
        ));
        assert!(matches!(
            scheduler.schedule(next_delta.sequence, false),
            MediaV3ScheduleDecision::Send {
                discontinuity: false,
                ..
            }
        ));
    }

    #[test]
    fn media_v3_control_gate_is_monotonic_and_accepts_at_most_ten_per_second() {
        let started = Instant::now();
        let request = |request_id| {
            MediaControlRequestV3::request_keyframe(
                request_id,
                None,
                sigil_protocol::KeyframeRequestReasonV3::TransportGap,
            )
        };
        let mut gate = MediaV3ControlGate::default();
        assert_eq!(
            gate.accept(request(1), started).unwrap(),
            MediaV3ControlDecision::Accept
        );
        assert_eq!(
            gate.accept(
                request(2),
                started + MEDIA_V3_CONTROL_REQUEST_INTERVAL - Duration::from_nanos(1),
            )
            .unwrap(),
            MediaV3ControlDecision::Pace(Duration::from_nanos(1))
        );
        assert_eq!(
            gate.accept(request(3), started + MEDIA_V3_CONTROL_REQUEST_INTERVAL)
                .unwrap(),
            MediaV3ControlDecision::Accept
        );
        assert!(
            gate.accept(request(3), started + Duration::from_secs(1))
                .is_err()
        );
    }

    #[tokio::test]
    async fn media_v3_rejected_control_requests_apply_read_side_pacing() {
        use tokio::io::AsyncWriteExt as _;

        let requests = [1, 2, 3].map(|request_id| {
            MediaControlRequestV3::request_keyframe(
                request_id,
                None,
                KeyframeRequestReasonV3::TransportGap,
            )
        });
        let (mut writer, reader) = tokio::io::duplex(128);
        for request in &requests {
            sigil_protocol::write_media_control_request_v3(&mut writer, request)
                .await
                .unwrap();
        }
        writer.shutdown().await.unwrap();
        let (sender, mut receiver) = tokio::sync::watch::channel(None);

        let started = Instant::now();
        forward_media_v3_control_requests(reader, sender)
            .await
            .unwrap();

        assert!(started.elapsed() >= MEDIA_V3_CONTROL_REQUEST_INTERVAL);
        receiver.changed().await.unwrap();
        assert_eq!(*receiver.borrow_and_update(), Some(requests[2]));
    }

    #[tokio::test]
    async fn media_v3_control_eof_is_clean_and_malformed_input_is_terminal() {
        use tokio::io::AsyncWriteExt as _;

        let (writer, reader) = tokio::io::duplex(64);
        let (sender, mut receiver) = tokio::sync::watch::channel(None);
        drop(writer);
        forward_media_v3_control_requests(reader, sender)
            .await
            .unwrap();
        assert!(receiver.changed().await.is_err());

        let final_request = MediaControlRequestV3::request_keyframe(
            1,
            Some(9),
            sigil_protocol::KeyframeRequestReasonV3::DecoderReset,
        );
        let (mut writer, reader) = tokio::io::duplex(64);
        let (sender, mut receiver) = tokio::sync::watch::channel(None);
        sigil_protocol::write_media_control_request_v3(&mut writer, &final_request)
            .await
            .unwrap();
        writer.shutdown().await.unwrap();
        forward_media_v3_control_requests(reader, sender)
            .await
            .unwrap();
        receiver.changed().await.unwrap();
        assert_eq!(*receiver.borrow_and_update(), Some(final_request));

        let (mut writer, reader) = tokio::io::duplex(64);
        let (sender, _receiver) = tokio::sync::watch::channel(None);
        writer
            .write_all(&[0; sigil_protocol::MEDIA_CONTROL_REQUEST_V3_LEN])
            .await
            .unwrap();
        writer.shutdown().await.unwrap();
        assert!(
            forward_media_v3_control_requests(reader, sender)
                .await
                .is_err()
        );
    }

    #[test]
    fn media_v3_deadline_ceil_clamps_four_frame_periods() {
        assert_eq!(media_v3_delivery_timeout_ms(60), 67);
        assert_eq!(media_v3_delivery_timeout_ms(240), 17);
        assert_eq!(
            media_v3_delivery_timeout_ms(1_000),
            MIN_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS
        );
        assert_eq!(
            media_v3_delivery_timeout_ms(1),
            MAX_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS
        );
    }

    #[test]
    fn media_v3_publisher_priority_maps_to_inverse_transport_priority() {
        let keyframe_transport =
            media_v3_transport_priority(MEDIA_V3_KEYFRAME_PUBLISHER_PRIORITY).unwrap();
        let delta_transport =
            media_v3_transport_priority(MEDIA_V3_DELTA_PUBLISHER_PRIORITY).unwrap();
        assert_eq!(keyframe_transport, MEDIA_V3_KEYFRAME_TRANSPORT_PRIORITY);
        assert_eq!(delta_transport, MEDIA_V3_DELTA_TRANSPORT_PRIORITY);
        assert!(keyframe_transport > delta_transport);
        assert!(media_v3_transport_priority(1).is_err());
    }

    fn startup_sample(fps: f64, frames: u64, receiver_open: bool) -> StartupCadenceSample {
        if frames == 0 {
            return StartupCadenceSample {
                receiver_open,
                ..StartupCadenceSample::default()
            };
        }
        let first = Instant::now();
        let span = if frames > 1 && fps > 0.0 {
            Duration::from_secs_f64((frames - 1) as f64 / fps)
        } else {
            Duration::ZERO
        };
        StartupCadenceSample {
            first_sequence: Some(0),
            last_sequence: Some(frames - 1),
            first_observed_at: Some(first),
            last_observed_at: Some(first + span),
            receiver_open,
            decodable_gop_ready: true,
        }
    }

    #[test]
    fn startup_restart_requires_a_live_decodable_source_not_target_cadence() {
        let slow = startup_sample(12.0, 6, true);
        let target = startup_sample(60.0, 8, true);
        assert!(!startup_source_needs_restart(slow));
        assert!(!slow.meets_target_cadence());
        assert!(!startup_source_needs_restart(target));
        assert!(target.meets_target_cadence());
        assert!(startup_source_needs_restart(startup_sample(60.0, 8, false)));
        assert!(startup_source_needs_restart(startup_sample(0.0, 0, true)));
        let mut undecodable = startup_sample(60.0, 8, true);
        undecodable.decodable_gop_ready = false;
        assert!(startup_source_needs_restart(undecodable));
    }

    #[test]
    fn startup_bursts_are_not_sustained_health() {
        let burst = startup_sample(10_000.0, 8, true);
        assert!(burst.fps() > GAMESCOPE_STARTUP_TARGET_MIN_FPS);
        assert!(!burst.has_representative_span());
        assert!(!burst.meets_target_cadence());
        assert!(!startup_source_needs_restart(burst));
    }

    #[tokio::test]
    async fn startup_sampling_preserves_current_decodable_gop() {
        let now = Instant::now();
        let frame = EncodedFrame {
            sequence: 0,
            width: 1_280,
            height: 800,
            capture_timestamp_micros: 0,
            presentation_timestamp_micros: 0,
            observed_at: now,
            keyframe: true,
            codec_config: true,
            discontinuity: false,
            data: Arc::from([1_u8, 2, 3]),
        };
        let (_frame_sender, mut frame_receiver) = tokio::sync::watch::channel(Some(frame.clone()));
        let (_gop_sender, gop_receiver) = tokio::sync::watch::channel(Some(EncodedGop {
            frames: vec![frame],
            payload_bytes: 3,
        }));

        let sample =
            sample_startup_cadence(&mut frame_receiver, &gop_receiver, Duration::ZERO).await;
        assert_eq!(sample.frame_progress(), 1);
        let current_gop = gop_receiver.borrow().clone().unwrap();
        assert_eq!(current_gop.frames.len(), 1);
        assert!(current_gop.frames[0].keyframe && current_gop.frames[0].codec_config);
    }

    #[tokio::test]
    async fn reaping_source_closes_outputs_before_waiting_for_task_cleanup() {
        let (frame_sender, frame_receiver) = tokio::sync::watch::channel(None);
        let (gop_sender, gop_receiver) = tokio::sync::watch::channel(None);
        let (reaped_tx, reaped_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            frame_sender.closed().await;
            gop_sender.closed().await;
            let _ = reaped_tx.send(());
            Ok(())
        });
        let source = EncodedSource {
            frames: frame_receiver,
            current_gop: gop_receiver,
            task,
            pointer_surface_dimensions: None,
            encoder_control: None,
        };

        reap_encoded_source_with_timeout(source, Duration::from_millis(100)).await;
        reaped_rx
            .await
            .expect("source task did not observe both closed outputs");
    }

    #[tokio::test]
    async fn reaping_stalled_source_aborts_after_bounded_grace() {
        let (_frame_sender, frame_receiver) = tokio::sync::watch::channel(None);
        let (_gop_sender, gop_receiver) = tokio::sync::watch::channel(None);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (reaped_tx, reaped_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _notify = DropNotify(Some(reaped_tx));
            let _ = started_tx.send(());
            std::future::pending::<Result<()>>().await
        });
        let source = EncodedSource {
            frames: frame_receiver,
            current_gop: gop_receiver,
            task,
            pointer_surface_dimensions: None,
            encoder_control: None,
        };

        started_rx.await.unwrap();
        reap_encoded_source_with_timeout(source, Duration::from_millis(10)).await;
        tokio::time::timeout(Duration::from_millis(100), reaped_rx)
            .await
            .expect("stalled source task was not aborted and reaped")
            .unwrap();
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
    fn current_gop_replay_is_complete_and_skips_already_sent_frames() {
        let frame = |sequence, keyframe| EncodedFrame {
            sequence,
            width: 1_280,
            height: 800,
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: std::time::Instant::now(),
            keyframe,
            codec_config: keyframe,
            discontinuity: false,
            data: Arc::from([sequence as u8]),
        };

        let gop = || EncodedGop {
            frames: vec![frame(10, true), frame(11, false), frame(12, false)],
            payload_bytes: 3,
        };

        let initial = new_current_gop_frames(gop(), None).collect::<Vec<_>>();
        assert_eq!(
            initial
                .iter()
                .map(|frame| frame.sequence)
                .collect::<Vec<_>>(),
            vec![10, 11, 12]
        );
        assert!(initial[0].keyframe && initial[0].codec_config);

        let resumed = new_current_gop_frames(gop(), Some(10)).collect::<Vec<_>>();
        assert_eq!(
            resumed
                .iter()
                .map(|frame| frame.sequence)
                .collect::<Vec<_>>(),
            vec![11, 12]
        );

        let behind_current_gop = new_current_gop_frames(gop(), Some(8)).collect::<Vec<_>>();
        assert_eq!(behind_current_gop[0].sequence, 10);
        assert!(behind_current_gop[0].keyframe && behind_current_gop[0].codec_config);

        assert!(new_current_gop_frames(gop(), Some(12)).next().is_none());
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

    #[tokio::test]
    async fn no_frame_peer_drop_reaps_source_and_allows_reconnect() {
        let sessions = Arc::new(SessionRegistry::default());
        let remote = endpoint(1);
        let nonce = [7; 16];
        let media = sessions
            .claim(remote, nonce, InvitationGrants::ALL)
            .unwrap();
        let input = sessions.claim_input(remote, nonce).unwrap();
        let session_id = media.session_id;

        let input_sessions = Arc::clone(&sessions);
        let input_shutdown = tokio::spawn(async move {
            loop {
                let notified = input_sessions.session_changed.notified();
                if !input_sessions.is_active(remote, session_id) {
                    break;
                }
                notified.await;
            }
            drop(input);
        });

        let (_frame_sender, mut frame_receiver) =
            tokio::sync::watch::channel(Option::<EncodedFrame>::None);
        let (source_started_tx, source_started_rx) = tokio::sync::oneshot::channel();
        let (source_reaped_tx, source_reaped_rx) = tokio::sync::oneshot::channel();
        let source_task = tokio::spawn(async move {
            let _notify = DropNotify(Some(source_reaped_tx));
            let _ = source_started_tx.send(());
            std::future::pending::<Result<()>>().await
        });
        let source_task = SourceTaskGuard::new(source_task);
        source_started_rx.await.unwrap();

        let (peer_alive, peer_disconnected) = tokio::sync::oneshot::channel::<()>();
        let disconnected = async move {
            let _ = peer_disconnected.await;
        };
        tokio::pin!(disconnected);
        drop(peer_alive);

        let activity = tokio::time::timeout(
            Duration::from_millis(100),
            wait_for_media_activity(&mut frame_receiver, disconnected.as_mut()),
        )
        .await
        .expect("no-frame media loop ignored peer disconnect")
        .unwrap();
        assert_eq!(activity, MediaActivity::PeerDisconnected);

        source_task.abort_and_wait().await;
        tokio::time::timeout(Duration::from_millis(100), source_reaped_rx)
            .await
            .expect("source task was not reaped after peer disconnect")
            .unwrap();
        drop(media);
        tokio::time::timeout(Duration::from_millis(100), input_shutdown)
            .await
            .expect("input lease did not observe media shutdown")
            .unwrap();

        assert!(
            sessions
                .claim(endpoint(2), [8; 16], InvitationGrants::ALL)
                .is_ok()
        );
    }
}
