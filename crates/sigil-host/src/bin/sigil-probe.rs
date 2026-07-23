use std::{
    collections::BTreeMap,
    fs,
    fs::OpenOptions,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey, endpoint::presets};
use iroh_moq::{Moq, MoqSession};
use moq_net::{BroadcastConsumer, GroupConsumer, TrackConsumer};
use sigil_protocol::{
    AdaptiveBitrateDecisionV1, AdaptiveBitrateReasonFlagsV1, AdaptiveBitrateStateV1,
    CONTROL_ALPN_V1, Capability, ClientHello, FrameFlags, GAMEPAD_AXIS_MAX, GAMEPAD_AXIS_MIN,
    GAMEPAD_TRIGGER_MAX, GamepadState, INPUT_ALPN_V1, INVITATION_CLOCK_SKEW_SECS, InputAck,
    InputEvent, InvitationGrants, KeyframeRequestReasonV3, MAX_INVITATION_TOKEN_LEN,
    MAX_MEDIA_GROUP_BYTES_V3, MAX_MEDIA_OBJECT_ID_V3, MEDIA_ALPN_V3, MEDIA_FEEDBACK_ALPN_V1,
    MediaCodec, MediaControlRequestV3, MediaFeedbackFlags, MediaFeedbackReportV1, MediaFrame,
    MediaObjectV3, PointerPosition, PointerSurfaceDimensions, ProtocolError, SignedInvitation,
    decode_media_frame_object, media_moq_broadcast_name, read_adaptive_bitrate_decision_v1,
    read_host_hello, read_input_ack, read_media_object_v3, write_client_hello, write_input_event,
    write_media_control_request_v3, write_media_feedback_report_v1,
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

#[allow(dead_code)]
#[path = "../identity.rs"]
mod identity;

mod moq_catalog;

use moq_catalog::{MoqCatalogMode, subscribe_goq_video_track};

const MEDIA_OBJECT_CAPACITY: usize = 4;

#[derive(Debug, Parser)]
#[command(name = "sigil-probe", version, about = "Bounded Sigil transport probe")]
struct Args {
    #[arg(long)]
    node_id: EndpointId,
    /// Owner-only 32-byte Iroh identity. Omit for the existing ephemeral probe.
    #[arg(long, value_name = "PATH")]
    identity: Option<PathBuf>,
    /// Remove all direct IP transports from the probe endpoint so control,
    /// media, and input are forced through Iroh relay paths.
    #[arg(long)]
    relay_only: bool,
    /// Owner-only one-time Sigil invitation file. Sent only on the first media
    /// handshake and requires a persistent identity bound to that invitation.
    #[arg(long, value_name = "PATH", requires = "identity")]
    invitation: Option<PathBuf>,
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u32).range(1..=36_000))]
    frames: u32,
    #[arg(long, default_value_t = 15)]
    timeout_seconds: u64,
    /// Fail unless accepted media frames sustain at least this first-to-last
    /// cadence. Intended for live appliance performance gates.
    #[arg(long, value_parser = parse_minimum_fps)]
    minimum_fps: Option<f64>,
    /// Optional strict encoded-size assertion. When omitted, accept and report
    /// the host's observed bounded dimensions.
    #[arg(long, value_parser = parse_size)]
    expect_size: Option<(u16, u16)>,
    /// Exercise custom grouped v3 media instead of upstream MoQ. Intended only
    /// for compatibility validation.
    #[arg(long)]
    media_v3: bool,
    /// Request a configured recovery keyframe after three accepted frames,
    /// then prove no delta history is delivered before the recovery barrier.
    #[arg(long)]
    keyframe_smoke: bool,
    /// Correlation identifier for `--keyframe-smoke` host evidence.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..))]
    keyframe_request_id: u64,
    /// Stop reading upstream MoQ after the first configured IDR, then require
    /// the transport to cancel that stale GOP and resume at a newer configured
    /// IDR within a bounded interval. Intended for latency-growth proofs.
    #[arg(
        long,
        value_parser = clap::value_parser!(u64).range(250..=5_000),
        conflicts_with = "media_v3"
    )]
    slow_consumer_ms: Option<u64>,
    /// Require gamepad negotiation and emit one bounded non-neutral snapshot
    /// followed by neutral. Intended for evtest-backed uinput proof.
    #[arg(long)]
    gamepad_smoke: bool,
    /// Require relative-pointer negotiation and emit bounded motion plus one
    /// complete left-click. Intended for libinput/Gamescope-backed proof.
    #[arg(long)]
    pointer_smoke: bool,
    /// Require the host to return an immediately available compositor pointer
    /// position and visibility sample. Intended for Gamescope restart proofs.
    #[arg(long, requires = "pointer_smoke")]
    pointer_feedback_smoke: bool,
    /// Attach the media-feedback stream to the active session, submit one
    /// complete report, and print the host's bounded decision. Intended for
    /// validating shadow-only encoder configurations.
    #[arg(
        long,
        conflicts_with_all = ["media_v3", "resolution_stall_smoke"]
    )]
    feedback_smoke: bool,
    /// Prove adaptive-resolution liveness across a controlled Gamescope stall.
    /// GATE_DIR coordinates the external SIGSTOP/SIGCONT harness.
    #[arg(
        long,
        value_name = "GATE_DIR",
        conflicts_with_all = [
            "media_v3",
            "keyframe_smoke",
            "slow_consumer_ms",
            "minimum_fps",
            "feedback_smoke"
        ]
    )]
    resolution_stall_smoke: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaTransport {
    UpstreamMoq,
    GroupedV3,
}

impl MediaTransport {
    fn alpn(self) -> &'static [u8] {
        match self {
            Self::UpstreamMoq => CONTROL_ALPN_V1,
            Self::GroupedV3 => MEDIA_ALPN_V3,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::UpstreamMoq => "iroh-moq",
            Self::GroupedV3 => "grouped-v3",
        }
    }

    fn media_alpn_label(self) -> &'static str {
        match self {
            Self::UpstreamMoq => std::str::from_utf8(iroh_moq::ALPN)
                .expect("pinned iroh-moq ALPN must be printable UTF-8"),
            Self::GroupedV3 => "sigil/media/3",
        }
    }
}

const KEYFRAME_SMOKE_REQUEST_AFTER_FRAMES: u32 = 3;
const KEYFRAME_SMOKE_MINIMUM_FRAMES: u32 = KEYFRAME_SMOKE_REQUEST_AFTER_FRAMES + 1;
const SLOW_CONSUMER_MINIMUM_FRAMES: u32 = 2;
const SLOW_CONSUMER_RECOVERY_TIMEOUT: Duration = Duration::from_secs(2);
const RESOLUTION_STALL_FEEDBACK_INTERVAL: Duration = Duration::from_secs(1);
const RESOLUTION_STALL_FRESH_REPORTS: u64 = 10;
const RESOLUTION_STALL_FEEDBACK_TIMEOUT: Duration = Duration::from_secs(5);
const RESOLUTION_STALL_RECOVERY_TIMEOUT: Duration = Duration::from_secs(2);
const RESOLUTION_STALL_GATE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_INVITATION_FILE_BYTES: u64 = (MAX_INVITATION_TOKEN_LEN + 2) as u64;

#[derive(Debug)]
enum MediaObjectOutcomeV3 {
    Object {
        accept_index: u64,
        object: MediaObjectV3,
    },
    Dropped {
        accept_index: u64,
    },
    Malformed {
        accept_index: u64,
        error: ProtocolError,
    },
}

impl MediaObjectOutcomeV3 {
    fn accept_index(&self) -> u64 {
        match self {
            Self::Object { accept_index, .. }
            | Self::Dropped { accept_index }
            | Self::Malformed { accept_index, .. } => *accept_index,
        }
    }

    fn is_fast_forward_barrier(&self) -> bool {
        let Self::Object { object, .. } = self else {
            return false;
        };
        object.header.object_id == 0
            && object.header.flags.contains(FrameFlags::KEYFRAME)
            && object.header.flags.contains(FrameFlags::CODEC_CONFIG)
            && object.header.flags.contains(FrameFlags::DISCONTINUITY)
    }
}

#[derive(Debug)]
struct MediaObjectReorderV3 {
    next_accept_index: u64,
    completed: BTreeMap<u64, MediaObjectOutcomeV3>,
}

impl MediaObjectReorderV3 {
    fn new(first_accept_index: u64) -> Self {
        Self {
            next_accept_index: first_accept_index,
            completed: BTreeMap::new(),
        }
    }

    fn pending_len(&self) -> usize {
        self.completed.len()
    }

    fn push(&mut self, outcome: MediaObjectOutcomeV3) -> Result<Option<MediaObjectOutcomeV3>> {
        if matches!(outcome, MediaObjectOutcomeV3::Malformed { .. }) {
            return Ok(Some(outcome));
        }
        let accept_index = outcome.accept_index();
        if accept_index < self.next_accept_index {
            // A discontinuity barrier may advance beyond older in-flight
            // reads. Their eventual timeout/reset outcomes belong to the
            // superseded GOP and must not poison the recovered sequence.
            return Ok(None);
        }
        let fast_forward = outcome.is_fast_forward_barrier();
        ensure!(
            self.completed.insert(accept_index, outcome).is_none(),
            "media v3 accept index {accept_index} completed more than once"
        );
        if fast_forward {
            self.completed.retain(|index, _| *index >= accept_index);
            self.next_accept_index = accept_index;
        }
        self.take_next()
    }

    fn take_next(&mut self) -> Result<Option<MediaObjectOutcomeV3>> {
        let Some(outcome) = self.completed.remove(&self.next_accept_index) else {
            return Ok(None);
        };
        self.next_accept_index = self
            .next_accept_index
            .checked_add(1)
            .context("media v3 accept index overflowed")?;
        Ok(Some(outcome))
    }
}

struct MediaObjectReceiverV3 {
    connection: iroh::endpoint::Connection,
    reads: tokio::task::JoinSet<MediaObjectOutcomeV3>,
    reorder: MediaObjectReorderV3,
    next_accept_index: u64,
    accepting: bool,
    read_timeout: Duration,
}

impl MediaObjectReceiverV3 {
    fn new(connection: iroh::endpoint::Connection, read_timeout: Duration) -> Self {
        Self {
            connection,
            reads: tokio::task::JoinSet::new(),
            reorder: MediaObjectReorderV3::new(0),
            next_accept_index: 0,
            accepting: true,
            read_timeout,
        }
    }

    async fn next(&mut self) -> Result<Option<MediaObjectOutcomeV3>> {
        loop {
            if let Some(completed) = self.reorder.take_next()? {
                return Ok(Some(completed));
            }
            if !self.accepting && self.reads.is_empty() {
                ensure!(
                    self.reorder.pending_len() == 0,
                    "media v3 connection closed with an incomplete object order"
                );
                return Ok(None);
            }

            tokio::select! {
                accepted = self.connection.accept_uni(),
                    if self.accepting
                        && self.reads.len() + self.reorder.pending_len()
                            < MEDIA_OBJECT_CAPACITY => {
                    match accepted {
                        Ok(mut stream) => {
                            let accept_index = self.next_accept_index;
                            self.next_accept_index = self
                                .next_accept_index
                                .checked_add(1)
                                .context("media v3 accept index overflowed")?;
                            let read_timeout = self.read_timeout;
                            self.reads.spawn(async move {
                                match tokio::time::timeout(
                                    read_timeout,
                                    read_media_object_v3(&mut stream),
                                )
                                .await
                                {
                                    Ok(Ok(object)) => MediaObjectOutcomeV3::Object {
                                        accept_index,
                                        object,
                                    },
                                    Ok(Err(ProtocolError::Io(_))) | Err(_) => {
                                        MediaObjectOutcomeV3::Dropped { accept_index }
                                    }
                                    Ok(Err(error)) => MediaObjectOutcomeV3::Malformed {
                                        accept_index,
                                        error,
                                    },
                                }
                            });
                        }
                        Err(_) => self.accepting = false,
                    }
                }
                completed = self.reads.join_next(), if !self.reads.is_empty() => {
                    match completed.expect("guarded by non-empty media v3 object task set") {
                        Ok(outcome) => {
                            if let Some(completed) = self.reorder.push(outcome)? {
                                return Ok(Some(completed));
                            }
                        }
                        Err(error) if error.is_cancelled() => continue,
                        Err(error) => return Err(error).context("media v3 object read task failed"),
                    }
                }
            }
        }
    }
}

impl Drop for MediaObjectReceiverV3 {
    fn drop(&mut self) {
        self.reads.abort_all();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaObjectDecisionV3 {
    Deliver { discontinuity: bool },
    DropLate,
    DropUntilKeyframe,
}

#[derive(Debug, Default)]
struct MediaObjectSequenceV3 {
    group_id: Option<u64>,
    last_object_id: Option<u32>,
    last_sequence: Option<u64>,
    group_payload_bytes: usize,
    waiting_for_keyframe: bool,
}

impl MediaObjectSequenceV3 {
    fn new() -> Self {
        Self {
            waiting_for_keyframe: true,
            ..Self::default()
        }
    }

    fn request_recovery(&mut self) {
        self.waiting_for_keyframe = true;
    }

    fn note_drop(&mut self) -> bool {
        let entered = !self.waiting_for_keyframe;
        self.waiting_for_keyframe = true;
        entered
    }

    fn classify(&mut self, object: &MediaObjectV3) -> MediaObjectDecisionV3 {
        let header = &object.header;
        if self
            .group_id
            .is_some_and(|group_id| header.group_id < group_id)
            || self
                .last_sequence
                .is_some_and(|sequence| header.sequence <= sequence)
        {
            return MediaObjectDecisionV3::DropLate;
        }

        let new_group = self.group_id != Some(header.group_id);
        let configured_group_start = header.object_id == 0
            && header.flags.contains(FrameFlags::KEYFRAME)
            && header.flags.contains(FrameFlags::CODEC_CONFIG);
        if new_group && !configured_group_start {
            self.waiting_for_keyframe = true;
            return MediaObjectDecisionV3::DropUntilKeyframe;
        }
        if !new_group && self.waiting_for_keyframe {
            return MediaObjectDecisionV3::DropUntilKeyframe;
        }

        let sequence_contiguous = self
            .last_sequence
            .is_none_or(|sequence| sequence.checked_add(1) == Some(header.sequence));
        let object_contiguous = new_group
            || self
                .last_object_id
                .is_some_and(|object_id| object_id.checked_add(1) == Some(header.object_id));
        let Some(next_group_bytes) = (if new_group {
            Some(object.payload.len())
        } else {
            self.group_payload_bytes.checked_add(object.payload.len())
        }) else {
            self.waiting_for_keyframe = true;
            return MediaObjectDecisionV3::DropUntilKeyframe;
        };
        let recovery_requires_discontinuity = new_group
            && self.group_id.is_some()
            && (self.waiting_for_keyframe || !sequence_contiguous)
            && !header.flags.contains(FrameFlags::DISCONTINUITY);
        if (!sequence_contiguous && !new_group)
            || !object_contiguous
            || next_group_bytes > MAX_MEDIA_GROUP_BYTES_V3
            || recovery_requires_discontinuity
        {
            self.waiting_for_keyframe = true;
            return MediaObjectDecisionV3::DropUntilKeyframe;
        }

        let discontinuity = header.flags.contains(FrameFlags::DISCONTINUITY)
            || self.waiting_for_keyframe
            || !sequence_contiguous;
        self.group_id = Some(header.group_id);
        self.last_object_id = Some(header.object_id);
        self.last_sequence = Some(header.sequence);
        self.group_payload_bytes = next_group_bytes;
        self.waiting_for_keyframe = false;
        MediaObjectDecisionV3::Deliver { discontinuity }
    }
}

#[derive(Debug)]
enum MoqProbeOutcome {
    Frame {
        frame: MediaFrame,
        group_sequence: u64,
        recovery: bool,
    },
    Dropped,
}

struct MoqProbeGroupCursor {
    group: GroupConsumer,
    sequence: u64,
    object_count: usize,
    object_bytes: usize,
    recovery: bool,
}

/// Own all upstream handles for the complete proof session. Dropping `Moq`
/// stops its actor, while dropping the consumers cancels their subscriptions.
struct MoqProbeLifetime {
    _moq: Moq,
    session: MoqSession,
    _broadcast: BroadcastConsumer,
}

struct MoqProbeReceiver {
    lifetime: MoqProbeLifetime,
    track: TrackConsumer,
    catalog_mode: MoqCatalogMode,
    current_group: Option<MoqProbeGroupCursor>,
    last_group_sequence: Option<u64>,
    last_frame_sequence: Option<u64>,
    waiting_for_keyframe: bool,
    pending_recovery: bool,
    cancelled_groups: u64,
    group_gaps: u64,
    maximum_group_objects: usize,
    maximum_group_bytes: usize,
    historical_suffix_frames: u64,
}

impl MoqProbeReceiver {
    async fn connect(
        endpoint: &Endpoint,
        address: EndpointAddr,
        session_id: u64,
        timeout: Duration,
    ) -> Result<Self> {
        let broadcast_name = media_moq_broadcast_name(session_id)
            .context("deriving session-scoped MoQ broadcast name")?;
        let moq = Moq::new(endpoint.clone());
        let mut session = tokio::time::timeout(timeout, moq.connect(address.clone()))
            .await
            .context("timed out connecting upstream MoQ media session")?
            .context("connecting upstream MoQ media session")?;
        ensure!(
            session.remote_id() == address.id,
            "upstream MoQ connected to unexpected peer {} instead of {}",
            session.remote_id(),
            address.id
        );
        let broadcast = tokio::time::timeout(timeout, session.subscribe(&broadcast_name))
            .await
            .with_context(|| {
                format!("timed out subscribing to upstream MoQ broadcast {broadcast_name}")
            })?
            .with_context(|| format!("subscribing to upstream MoQ broadcast {broadcast_name}"))?;
        let catalog = subscribe_goq_video_track(&broadcast, timeout)
            .await
            .context("resolving Goq MoQ catalog")?;
        Ok(Self {
            lifetime: MoqProbeLifetime {
                _moq: moq,
                session,
                _broadcast: broadcast,
            },
            track: catalog.track,
            catalog_mode: catalog.mode,
            current_group: None,
            last_group_sequence: None,
            last_frame_sequence: None,
            waiting_for_keyframe: true,
            pending_recovery: false,
            cancelled_groups: 0,
            group_gaps: 0,
            maximum_group_objects: 0,
            maximum_group_bytes: 0,
            historical_suffix_frames: 0,
        })
    }

    fn connection(&self) -> &iroh::endpoint::Connection {
        self.lifetime.session.conn()
    }

    /// Stop consuming the current group immediately. The track cursor already
    /// advanced when the group was selected, so a subsequent read can only
    /// enter a newer native group; no cancelled delta suffix can be accepted.
    fn request_recovery(&mut self) -> Option<u64> {
        self.waiting_for_keyframe = true;
        self.pending_recovery = true;
        self.current_group.take().map(|cursor| {
            self.cancelled_groups = self.cancelled_groups.saturating_add(1);
            self.last_group_sequence = Some(cursor.sequence);
            cursor.sequence
        })
    }

    async fn next(&mut self) -> Result<Option<MoqProbeOutcome>> {
        loop {
            if self.current_group.is_none() {
                let group = self
                    .track
                    .next_group()
                    .await
                    .context("reading the next sequential upstream MoQ group")?;
                let Some(group) = group else {
                    return Ok(None);
                };
                let sequence = group.sequence;
                let group_gap = classify_moq_probe_group(self.last_group_sequence, sequence)?;
                if group_gap {
                    self.group_gaps = self.group_gaps.saturating_add(1);
                    self.waiting_for_keyframe = true;
                    self.pending_recovery = true;
                }
                self.current_group = Some(MoqProbeGroupCursor {
                    group,
                    sequence,
                    object_count: 0,
                    object_bytes: 0,
                    recovery: self.pending_recovery,
                });
                self.pending_recovery = false;
            }

            let cursor = self
                .current_group
                .as_mut()
                .expect("MoQ group cursor was initialized");
            let object = match cursor.group.read_frame().await {
                Ok(object) => object,
                Err(error) if moq_probe_error_is_recoverable(&error) => {
                    self.cancelled_groups = self.cancelled_groups.saturating_add(1);
                    self.last_group_sequence = Some(cursor.sequence);
                    self.current_group = None;
                    self.waiting_for_keyframe = true;
                    self.pending_recovery = true;
                    return Ok(Some(MoqProbeOutcome::Dropped));
                }
                Err(error) => {
                    bail!(
                        "upstream MoQ group {} failed terminally: {error}",
                        cursor.sequence
                    );
                }
            };
            let Some(object) = object else {
                ensure!(
                    cursor.object_count > 0,
                    "upstream MoQ group {} was empty",
                    cursor.sequence
                );
                self.last_group_sequence = Some(cursor.sequence);
                self.current_group = None;
                continue;
            };

            let next_bytes = validate_moq_probe_object_bounds(
                cursor.sequence,
                cursor.object_count,
                cursor.object_bytes,
                object.len(),
            )?;
            let frame = decode_media_frame_object(&object).with_context(|| {
                format!(
                    "decoding upstream MoQ group {} object {}",
                    cursor.sequence, cursor.object_count
                )
            })?;
            let first_object = cursor.object_count == 0;
            let contiguous = validate_moq_probe_frame(
                cursor.sequence,
                first_object,
                self.last_frame_sequence,
                &frame,
            )?;
            let recovery = cursor.recovery
                || self.waiting_for_keyframe
                || (first_object && !contiguous)
                || frame.header.flags.contains(FrameFlags::DISCONTINUITY);
            if self.waiting_for_keyframe && !first_object {
                self.historical_suffix_frames = self.historical_suffix_frames.saturating_add(1);
                bail!(
                    "upstream MoQ delivered a historical delta suffix while recovery was pending"
                );
            }
            cursor.object_count += 1;
            cursor.object_bytes = next_bytes;
            self.maximum_group_objects = self.maximum_group_objects.max(cursor.object_count);
            self.maximum_group_bytes = self.maximum_group_bytes.max(next_bytes);
            self.last_frame_sequence = Some(frame.header.sequence);
            self.waiting_for_keyframe = false;
            return Ok(Some(MoqProbeOutcome::Frame {
                frame,
                group_sequence: cursor.sequence,
                recovery,
            }));
        }
    }
}

fn classify_moq_probe_group(previous: Option<u64>, current: u64) -> Result<bool> {
    let Some(previous) = previous else {
        return Ok(false);
    };
    ensure!(
        current > previous,
        "upstream MoQ group sequence did not increase: previous={previous}, current={current}"
    );
    Ok(previous.checked_add(1) != Some(current))
}

fn validate_moq_probe_object_bounds(
    group_sequence: u64,
    object_count: usize,
    object_bytes: usize,
    object_len: usize,
) -> Result<usize> {
    let maximum_objects = MAX_MEDIA_OBJECT_ID_V3 as usize + 1;
    ensure!(
        object_count < maximum_objects,
        "upstream MoQ group {group_sequence} exceeded {maximum_objects} objects"
    );
    let next_bytes = object_bytes
        .checked_add(object_len)
        .context("upstream MoQ group byte count overflowed")?;
    ensure!(
        next_bytes <= MAX_MEDIA_GROUP_BYTES_V3,
        "upstream MoQ group {group_sequence} exceeded {MAX_MEDIA_GROUP_BYTES_V3} bytes"
    );
    Ok(next_bytes)
}

fn validate_moq_probe_frame(
    group_sequence: u64,
    first_object: bool,
    last_frame_sequence: Option<u64>,
    frame: &MediaFrame,
) -> Result<bool> {
    if first_object {
        ensure!(
            frame.header.codec == MediaCodec::H264
                && frame.header.flags.contains(FrameFlags::KEYFRAME)
                && frame.header.flags.contains(FrameFlags::CODEC_CONFIG),
            "upstream MoQ group {group_sequence} did not begin with a configured H.264 keyframe"
        );
        // A native group cancellation is itself the transport discontinuity.
        // Portal marks the first configured IDR in the replacement group as a
        // decoder discontinuity even when the embedded compatibility header
        // does not redundantly carry the v3 DISCONTINUITY flag.
    }
    let contiguous = last_frame_sequence
        .is_none_or(|previous| previous.checked_add(1) == Some(frame.header.sequence));
    ensure!(
        last_frame_sequence.is_none_or(|previous| frame.header.sequence > previous),
        "upstream MoQ group {group_sequence} contains a non-monotonic frame sequence"
    );
    ensure!(
        first_object || contiguous,
        "upstream MoQ group {group_sequence} contains a non-contiguous frame sequence"
    );
    Ok(contiguous)
}

fn moq_probe_error_is_recoverable(error: &moq_net::Error) -> bool {
    matches!(
        error,
        moq_net::Error::Cancel
            | moq_net::Error::Old
            | moq_net::Error::Timeout
            | moq_net::Error::Dropped
            | moq_net::Error::CacheFull
            | moq_net::Error::Remote(0 | 2 | 3 | 24 | 26)
    )
}

struct AcceptedMedia {
    flags: FrameFlags,
    width: u16,
    height: u16,
    sequence: u64,
    capture_timestamp_us: u64,
    payload_len: usize,
    v3_group_id: Option<u64>,
}

impl From<MediaObjectV3> for AcceptedMedia {
    fn from(object: MediaObjectV3) -> Self {
        Self {
            flags: object.header.flags,
            width: object.header.width,
            height: object.header.height,
            sequence: object.header.sequence,
            capture_timestamp_us: object.header.capture_timestamp_us,
            payload_len: object.payload.len(),
            v3_group_id: Some(object.header.group_id),
        }
    }
}

#[derive(Debug)]
struct ResolutionStallEvidence {
    native_dimensions: (u16, u16),
    reduced_dimensions: (u16, u16),
    stall_sequence: u64,
    fresh_duration: Duration,
    stall_input_ack_micros: u64,
    resume_input_ack_micros: u64,
    resume_media_micros: u64,
    resume_sequence_advance: u64,
}

struct ResolutionStallGate {
    ready: PathBuf,
    stalled: PathBuf,
    resume_ready: PathBuf,
    resumed: PathBuf,
}

impl ResolutionStallGate {
    fn new(directory: &Path) -> Result<Self> {
        ensure!(
            directory.is_dir(),
            "resolution stall gate is not a directory"
        );
        let gate = Self {
            ready: directory.join("ready"),
            stalled: directory.join("stalled"),
            resume_ready: directory.join("resume-ready"),
            resumed: directory.join("resumed"),
        };
        for marker in [
            &gate.ready,
            &gate.stalled,
            &gate.resume_ready,
            &gate.resumed,
        ] {
            ensure!(
                !marker.exists(),
                "resolution stall gate marker already exists: {}",
                marker.display()
            );
        }
        Ok(gate)
    }
}

fn complete_feedback_report(
    report_id: u64,
    flags: MediaFeedbackFlags,
    last_sequence: u64,
) -> MediaFeedbackReportV1 {
    MediaFeedbackReportV1 {
        report_id,
        interval_ms: 1_000,
        flags,
        last_sequence: Some(last_sequence),
        transport_dropped_delta: 0,
        frontend_dropped_delta: 0,
        decoder_dropped_delta: 0,
        presenter_dropped_delta: 0,
        frontend_queue_depth: 0,
        frontend_queue_capacity: 4,
        decode_queue_depth: 0,
        decode_queue_capacity: 4,
        presenter_queue_depth: 0,
        presenter_queue_capacity: 2,
        transport_delivery_p95_ms: Some(10),
        decode_p95_ms: Some(3),
        presentation_p95_ms: Some(5),
    }
}

fn resolution_stall_reduced_dimensions(native: (u16, u16)) -> Result<(u16, u16)> {
    ensure!(
        native.0.is_multiple_of(4) && native.1.is_multiple_of(4),
        "native dimensions must be divisible by four for an exact three-quarter tier"
    );
    let reduced = (native.0 / 4 * 3, native.1 / 4 * 3);
    ensure!(
        reduced.0 >= 64 && reduced.1 >= 64,
        "three-quarter dimensions are below the H.264 minimum"
    );
    ensure!(
        reduced.0.is_multiple_of(2) && reduced.1.is_multiple_of(2),
        "three-quarter H.264 dimensions must be even"
    );
    ensure!(
        u32::from(native.0) * u32::from(reduced.1) == u32::from(native.1) * u32::from(reduced.0),
        "three-quarter dimensions must preserve the native aspect ratio"
    );
    Ok(reduced)
}

async fn exchange_feedback_report(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    report: &MediaFeedbackReportV1,
) -> Result<AdaptiveBitrateDecisionV1> {
    tokio::time::timeout(
        RESOLUTION_STALL_FEEDBACK_TIMEOUT,
        write_media_feedback_report_v1(send, report),
    )
    .await
    .context("timed out writing media feedback")??;
    let decision = tokio::time::timeout(
        RESOLUTION_STALL_FEEDBACK_TIMEOUT,
        read_adaptive_bitrate_decision_v1(recv),
    )
    .await
    .context("timed out waiting for media feedback decision")??
    .context("host closed the media feedback stream")?;
    ensure!(
        decision.report_id == report.report_id,
        "feedback decision report ID {} does not match report {}",
        decision.report_id,
        report.report_id
    );
    Ok(decision)
}

async fn run_feedback_smoke(
    endpoint: &Endpoint,
    address: EndpointAddr,
    nonce: [u8; 16],
    session_id: u64,
    last_sequence: u64,
) -> Result<(AdaptiveBitrateDecisionV1, &'static str, Option<f64>)> {
    let connection = endpoint
        .connect(address, MEDIA_FEEDBACK_ALPN_V1)
        .await
        .context("connecting media feedback protocol")?;
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .context("opening media feedback stream")?;
    let negotiation = negotiate(
        &mut send,
        &mut recv,
        nonce,
        vec![Capability::VideoH264],
        Capability::VideoH264,
        "media feedback",
        None,
    )
    .await?;
    ensure!(
        negotiation.session_id == session_id,
        "media and feedback session IDs differ"
    );

    let report = complete_feedback_report(1, MediaFeedbackFlags::NONE, last_sequence);
    let decision = exchange_feedback_report(&mut send, &mut recv, &report).await?;
    let (path_mode, path_rtt_ms) = selected_path_diagnostics(&connection);
    send.finish().context("finishing media feedback stream")?;
    connection.close(0_u32.into(), b"feedback probe complete");
    Ok((decision, path_mode, path_rtt_ms))
}

async fn next_resolution_stall_moq_frame(receiver: &mut MoqProbeReceiver) -> Result<AcceptedMedia> {
    loop {
        match receiver.next().await? {
            Some(MoqProbeOutcome::Frame {
                frame,
                group_sequence,
                ..
            }) => {
                return Ok(AcceptedMedia {
                    flags: frame.header.flags,
                    width: frame.header.width,
                    height: frame.header.height,
                    sequence: frame.header.sequence,
                    capture_timestamp_us: frame.header.capture_timestamp_us,
                    payload_len: frame.payload.len(),
                    v3_group_id: Some(group_sequence),
                });
            }
            Some(MoqProbeOutcome::Dropped) => {}
            None => bail!("host closed upstream MoQ during resolution-stall smoke"),
        }
    }
}

fn create_resolution_stall_marker(path: &Path, contents: &str) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut marker = options
        .open(path)
        .with_context(|| format!("creating resolution stall marker {}", path.display()))?;
    marker
        .write_all(contents.as_bytes())
        .with_context(|| format!("writing resolution stall marker {}", path.display()))?;
    marker
        .sync_all()
        .with_context(|| format!("syncing resolution stall marker {}", path.display()))?;
    Ok(())
}

async fn wait_for_resolution_stall_marker(path: &Path, timeout: Duration) -> Result<()> {
    tokio::time::timeout(timeout, async {
        loop {
            match fs::metadata(path) {
                Ok(metadata) => {
                    ensure!(
                        metadata.is_file(),
                        "resolution stall gate marker is not a file: {}",
                        path.display()
                    );
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::time::sleep(RESOLUTION_STALL_GATE_POLL_INTERVAL).await;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("reading resolution stall gate marker {}", path.display())
                    });
                }
            }
        }
    })
    .await
    .with_context(|| {
        format!(
            "timed out waiting for resolution stall marker {}",
            path.display()
        )
    })?
}

async fn send_resolution_stall_input_probe(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    expected_ack: &mut u64,
) -> Result<u64> {
    let started = Instant::now();
    write_input_event(send, &InputEvent::Probe)
        .await
        .context("writing input probe during resolution stall")?;
    *expected_ack = expected_ack.saturating_add(1);
    tokio::time::timeout(
        RESOLUTION_STALL_RECOVERY_TIMEOUT,
        read_expected_input_ack(recv, 2, *expected_ack),
    )
    .await
    .context("input acknowledgement exceeded the resolution-stall bound")??;
    Ok(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX))
}

#[allow(clippy::too_many_arguments)]
async fn run_resolution_stall_smoke(
    gate_directory: &Path,
    receiver: &mut MoqProbeReceiver,
    feedback_send: &mut iroh::endpoint::SendStream,
    feedback_recv: &mut iroh::endpoint::RecvStream,
    input_send: &mut iroh::endpoint::SendStream,
    input_recv: &mut iroh::endpoint::RecvStream,
    expected_ack: &mut u64,
    expected_native: Option<(u16, u16)>,
    timeout: Duration,
) -> Result<ResolutionStallEvidence> {
    let gate = ResolutionStallGate::new(gate_directory)?;
    let initial = tokio::time::timeout(timeout, next_resolution_stall_moq_frame(receiver))
        .await
        .context("timed out waiting for initial resolution-stall media")??;
    ensure!(
        initial.flags.contains(FrameFlags::KEYFRAME)
            && initial.flags.contains(FrameFlags::CODEC_CONFIG),
        "resolution-stall smoke did not begin at a configured IDR"
    );
    let native_dimensions = (initial.width, initial.height);
    if let Some(expected) = expected_native {
        ensure!(
            native_dimensions == expected,
            "expected {}x{} native media but received {}x{}",
            expected.0,
            expected.1,
            native_dimensions.0,
            native_dimensions.1
        );
    }
    let reduced_dimensions = resolution_stall_reduced_dimensions(native_dimensions)?;

    let mut report_id = 1_u64;
    let seed = complete_feedback_report(report_id, MediaFeedbackFlags::NONE, initial.sequence);
    let _ = exchange_feedback_report(feedback_send, feedback_recv, &seed).await?;
    tokio::time::sleep(RESOLUTION_STALL_FEEDBACK_INTERVAL).await;

    report_id += 1;
    let pressure = complete_feedback_report(
        report_id,
        MediaFeedbackFlags::RESYNC_ACTIVE,
        initial.sequence,
    );
    let pressure_decision =
        exchange_feedback_report(feedback_send, feedback_recv, &pressure).await?;
    ensure!(
        pressure_decision
            .reasons
            .contains(AdaptiveBitrateReasonFlagsV1::RECEIVER_QUEUE),
        "resolution-stall pressure report did not register receiver pressure"
    );

    let reduced = tokio::time::timeout(timeout, async {
        loop {
            let frame = next_resolution_stall_moq_frame(receiver).await?;
            let dimensions = (frame.width, frame.height);
            ensure!(
                dimensions == native_dimensions || dimensions == reduced_dimensions,
                "resolution-stall media changed to unexpected dimensions {}x{}",
                dimensions.0,
                dimensions.1
            );
            if dimensions == reduced_dimensions
                && frame.flags.contains(FrameFlags::KEYFRAME)
                && frame.flags.contains(FrameFlags::CODEC_CONFIG)
            {
                break Ok::<_, anyhow::Error>(frame);
            }
        }
    })
    .await
    .context("timed out waiting for pressure-triggered reduced configured IDR")??;
    ensure!(
        reduced.sequence > initial.sequence,
        "reduced configured IDR did not advance media sequence"
    );

    create_resolution_stall_marker(
        &gate.ready,
        &format!(
            "native={}x{}\nreduced={}x{}\nsequence={}\n",
            native_dimensions.0,
            native_dimensions.1,
            reduced_dimensions.0,
            reduced_dimensions.1,
            reduced.sequence
        ),
    )?;
    println!("resolution_stall_ready=1");
    println!("resolution_stall_ready_sequence={}", reduced.sequence);
    std::io::stdout()
        .flush()
        .context("flushing resolution-stall readiness marker")?;
    wait_for_resolution_stall_marker(&gate.stalled, timeout).await?;

    report_id += 1;
    let reseed = complete_feedback_report(report_id, MediaFeedbackFlags::NONE, reduced.sequence);
    let _ = exchange_feedback_report(feedback_send, feedback_recv, &reseed).await?;
    let mut last_feedback_at = Instant::now();
    let mut fresh_started = None;
    let mut stall_input_ack_micros = None;
    for index in 0..RESOLUTION_STALL_FRESH_REPORTS {
        tokio::time::sleep_until(tokio::time::Instant::from_std(
            last_feedback_at + RESOLUTION_STALL_FEEDBACK_INTERVAL,
        ))
        .await;
        report_id += 1;
        let report =
            complete_feedback_report(report_id, MediaFeedbackFlags::NONE, reduced.sequence);
        let sent_at = Instant::now();
        fresh_started.get_or_insert(sent_at);
        let decision = exchange_feedback_report(feedback_send, feedback_recv, &report).await?;
        ensure!(
            decision.state != AdaptiveBitrateStateV1::Increase,
            "no-progress feedback unexpectedly increased adaptive bitrate"
        );
        ensure!(
            !decision
                .reasons
                .contains(AdaptiveBitrateReasonFlagsV1::CLEAN_RECOVERY),
            "no-progress feedback was incorrectly classified as clean recovery"
        );
        last_feedback_at = sent_at;
        if index + 1 == RESOLUTION_STALL_FRESH_REPORTS / 2 {
            stall_input_ack_micros = Some(
                send_resolution_stall_input_probe(input_send, input_recv, expected_ack).await?,
            );
        }
    }
    let fresh_duration = fresh_started
        .context("resolution-stall fresh feedback interval did not start")?
        .elapsed();
    ensure!(
        fresh_duration > Duration::from_secs(8),
        "resolution-stall fresh feedback lasted only {}ms",
        fresh_duration.as_millis()
    );

    create_resolution_stall_marker(
        &gate.resume_ready,
        &format!(
            "fresh_reports={RESOLUTION_STALL_FRESH_REPORTS}\nfresh_duration_ms={}\n",
            fresh_duration.as_millis()
        ),
    )?;
    println!("resolution_stall_resume_ready=1");
    std::io::stdout()
        .flush()
        .context("flushing resolution-stall resume marker")?;
    wait_for_resolution_stall_marker(&gate.resumed, timeout).await?;

    let resume_input_ack_micros =
        send_resolution_stall_input_probe(input_send, input_recv, expected_ack).await?;
    let media_resumed_at = Instant::now();
    let resumed = tokio::time::timeout(
        RESOLUTION_STALL_RECOVERY_TIMEOUT,
        next_resolution_stall_moq_frame(receiver),
    )
    .await
    .context("media did not resume within the resolution-stall bound")??;
    ensure!(
        (resumed.width, resumed.height) == reduced_dimensions,
        "media resumed at {}x{} instead of reduced {}x{}",
        resumed.width,
        resumed.height,
        reduced_dimensions.0,
        reduced_dimensions.1
    );
    let resume_sequence_advance = resumed
        .sequence
        .checked_sub(reduced.sequence)
        .context("resumed media sequence did not advance")?;
    ensure!(
        resume_sequence_advance > 0,
        "resumed media sequence did not advance"
    );
    let resume_media_micros =
        u64::try_from(media_resumed_at.elapsed().as_micros()).unwrap_or(u64::MAX);

    Ok(ResolutionStallEvidence {
        native_dimensions,
        reduced_dimensions,
        stall_sequence: reduced.sequence,
        fresh_duration,
        stall_input_ack_micros: stall_input_ack_micros
            .context("resolution-stall input acknowledgement was not measured")?,
        resume_input_ack_micros,
        resume_media_micros,
        resume_sequence_advance,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.timeout_seconds > 0,
        "--timeout-seconds must be greater than zero"
    );
    ensure!(
        !args.keyframe_smoke || args.frames >= KEYFRAME_SMOKE_MINIMUM_FRAMES,
        "--keyframe-smoke requires at least {KEYFRAME_SMOKE_MINIMUM_FRAMES} accepted frames"
    );
    ensure!(
        args.slow_consumer_ms.is_none() || args.frames >= SLOW_CONSUMER_MINIMUM_FRAMES,
        "--slow-consumer-ms requires at least {SLOW_CONSUMER_MINIMUM_FRAMES} accepted frames"
    );

    let secret = match args.identity.as_deref() {
        Some(path) => identity::load(path).context("loading persistent probe identity")?,
        None => SecretKey::generate(),
    };
    let mut required_grants = InvitationGrants::VIEW;
    if args.pointer_smoke {
        required_grants = required_grants.union(InvitationGrants::POINTER_KEYBOARD);
    }
    if args.gamepad_smoke {
        required_grants = required_grants.union(InvitationGrants::GAMEPAD);
    }
    let invitation = args
        .invitation
        .as_deref()
        .map(|path| load_invitation_file(path, args.node_id, secret.public(), required_grants))
        .transpose()?;
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).context("generating handshake nonce")?;
    let mut endpoint_builder = Endpoint::builder(presets::N0).secret_key(secret);
    if args.relay_only {
        endpoint_builder = endpoint_builder.clear_ip_transports();
    }
    let endpoint = endpoint_builder
        .bind()
        .await
        .context("binding probe endpoint")?;
    let _ = tokio::time::timeout(Duration::from_secs(10), endpoint.online()).await;
    let address = EndpointAddr::new(args.node_id);
    let media_transport = if args.media_v3 {
        MediaTransport::GroupedV3
    } else {
        MediaTransport::UpstreamMoq
    };

    let media_connection = endpoint
        .connect(address.clone(), media_transport.alpn())
        .await
        .context("connecting media protocol")?;
    let (mut media_send, mut media_recv) = media_connection
        .open_bi()
        .await
        .context("opening media stream")?;
    let handshake_name = if media_transport == MediaTransport::UpstreamMoq {
        "control"
    } else {
        "media"
    };
    let media_negotiation = negotiate(
        &mut media_send,
        &mut media_recv,
        nonce,
        vec![Capability::VideoH264],
        Capability::VideoH264,
        handshake_name,
        invitation.as_deref(),
    )
    .await?;
    let session_id = media_negotiation.session_id;
    // Sigil finishes its response half after HostHello. Keep our send half
    // alive as the bounded keyframe-control stream.
    drop(media_recv);

    let mut moq_receiver = if media_transport == MediaTransport::UpstreamMoq {
        Some(
            MoqProbeReceiver::connect(
                &endpoint,
                address.clone(),
                session_id,
                Duration::from_secs(args.timeout_seconds),
            )
            .await?,
        )
    } else {
        None
    };

    let input_connection = endpoint
        .connect(address.clone(), INPUT_ALPN_V1)
        .await
        .context("connecting input protocol")?;
    let (mut input_send, mut input_recv) = input_connection
        .open_bi()
        .await
        .context("opening input stream")?;
    let mut input_offers = vec![Capability::InputAck];
    if args.pointer_smoke {
        input_offers.push(Capability::RelativePointer);
    }
    if args.pointer_feedback_smoke {
        input_offers.push(Capability::PointerPositionFeedback);
        input_offers.push(Capability::PointerVisibilityFeedback);
    }
    if args.gamepad_smoke {
        input_offers.push(Capability::Gamepad);
    }
    let input_negotiation = negotiate(
        &mut input_send,
        &mut input_recv,
        nonce,
        input_offers,
        Capability::InputAck,
        "input",
        None,
    )
    .await?;
    ensure!(
        session_id == input_negotiation.session_id,
        "media and input session IDs differ"
    );
    if args.pointer_feedback_smoke {
        ensure!(
            input_negotiation
                .capabilities
                .contains(&Capability::PointerPositionFeedback)
                && input_negotiation
                    .capabilities
                    .contains(&Capability::PointerVisibilityFeedback),
            "host did not accept the required pointer feedback capabilities"
        );
        let initial_feedback =
            read_expected_input_ack(&mut input_recv, args.timeout_seconds, 0).await?;
        ensure!(
            initial_feedback.pointer_position.is_some(),
            "host pointer feedback is unavailable"
        );
        ensure!(
            initial_feedback.pointer_visible.is_some(),
            "host omitted negotiated pointer visibility feedback"
        );
    }
    let input_started = Instant::now();
    write_input_event(&mut input_send, &InputEvent::Probe)
        .await
        .context("writing input probe")?;
    read_expected_input_ack(&mut input_recv, args.timeout_seconds, 1).await?;
    let input_ack_micros = input_started.elapsed().as_micros();
    let mut expected_ack = 1_u64;
    let mut pointer_sync_position = None;
    let mut pointer_motion_position = None;

    if args.pointer_smoke {
        ensure!(
            input_negotiation
                .capabilities
                .contains(&Capability::RelativePointer),
            "host did not accept the required relative pointer capability"
        );
        let pointer_dimensions = media_negotiation.pointer_surface_dimensions.context(
            "host did not advertise pointer surface dimensions required by --pointer-smoke",
        )?;
        let [position_sync, relative_motion, click] =
            pointer_smoke_events(Some(pointer_dimensions))?;
        let [expected_sync_position, expected_motion_position] =
            pointer_smoke_expected_positions(pointer_dimensions)?;
        write_input_event(&mut input_send, &position_sync)
            .await
            .context("writing pointer position synchronization")?;
        expected_ack += 1;
        let sync_ack =
            read_expected_input_ack(&mut input_recv, args.timeout_seconds, expected_ack).await?;
        if args.pointer_feedback_smoke {
            pointer_sync_position = Some(
                confirm_pointer_position(
                    &mut input_send,
                    &mut input_recv,
                    &mut expected_ack,
                    sync_ack,
                    expected_sync_position,
                    Duration::from_secs(args.timeout_seconds.min(5)),
                )
                .await
                .context("verifying compositor position after pointer synchronization")?,
            );
        }
        write_input_event(&mut input_send, &relative_motion)
            .await
            .context("writing relative pointer smoke motion")?;
        expected_ack += 1;
        let motion_ack =
            read_expected_input_ack(&mut input_recv, args.timeout_seconds, expected_ack).await?;
        if args.pointer_feedback_smoke {
            pointer_motion_position = Some(
                confirm_pointer_position(
                    &mut input_send,
                    &mut input_recv,
                    &mut expected_ack,
                    motion_ack,
                    expected_motion_position,
                    Duration::from_secs(args.timeout_seconds.min(5)),
                )
                .await
                .context("verifying compositor position after relative pointer motion")?,
            );
        }
        write_input_event(&mut input_send, &click)
            .await
            .context("writing pointer smoke click")?;
        expected_ack += 1;
        read_expected_input_ack(&mut input_recv, args.timeout_seconds, expected_ack).await?;
    }

    if args.gamepad_smoke {
        ensure!(
            input_negotiation
                .capabilities
                .contains(&Capability::Gamepad),
            "host did not accept the required gamepad capability"
        );
        write_input_event(
            &mut input_send,
            &InputEvent::Gamepad {
                state: GamepadState {
                    a: true,
                    right_shoulder: true,
                    dpad_right: true,
                    left_x: GAMEPAD_AXIS_MAX,
                    right_y: GAMEPAD_AXIS_MIN,
                    left_trigger: GAMEPAD_TRIGGER_MAX,
                    right_trigger: GAMEPAD_TRIGGER_MAX,
                    ..GamepadState::default()
                },
            },
        )
        .await
        .context("writing non-neutral gamepad smoke snapshot")?;
        expected_ack += 1;
        read_expected_input_ack(&mut input_recv, args.timeout_seconds, expected_ack).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        write_input_event(
            &mut input_send,
            &InputEvent::Gamepad {
                state: GamepadState::default(),
            },
        )
        .await
        .context("writing neutral gamepad smoke snapshot")?;
        expected_ack += 1;
        read_expected_input_ack(&mut input_recv, args.timeout_seconds, expected_ack).await?;
    }

    if let Some(gate_directory) = args.resolution_stall_smoke.as_deref() {
        let feedback_connection = endpoint
            .connect(address.clone(), MEDIA_FEEDBACK_ALPN_V1)
            .await
            .context("connecting media feedback protocol")?;
        let (mut feedback_send, mut feedback_recv) = feedback_connection
            .open_bi()
            .await
            .context("opening media feedback stream")?;
        let feedback_negotiation = negotiate(
            &mut feedback_send,
            &mut feedback_recv,
            nonce,
            vec![Capability::VideoH264],
            Capability::VideoH264,
            "media feedback",
            None,
        )
        .await?;
        ensure!(
            session_id == feedback_negotiation.session_id,
            "media and feedback session IDs differ"
        );

        let evidence = run_resolution_stall_smoke(
            gate_directory,
            moq_receiver
                .as_mut()
                .expect("resolution-stall smoke requires upstream MoQ"),
            &mut feedback_send,
            &mut feedback_recv,
            &mut input_send,
            &mut input_recv,
            &mut expected_ack,
            args.expect_size,
            Duration::from_secs(args.timeout_seconds),
        )
        .await?;

        feedback_send
            .finish()
            .context("finishing media feedback stream")?;
        input_send.finish().context("finishing input stream")?;
        media_send
            .finish()
            .context("finishing media request stream")?;
        feedback_connection.close(0_u32.into(), b"probe complete");
        input_connection.close(0_u32.into(), b"probe complete");
        moq_receiver
            .as_ref()
            .expect("resolution-stall smoke requires upstream MoQ")
            .connection()
            .close(0_u32.into(), b"probe complete");
        media_connection.close(0_u32.into(), b"probe complete");
        tokio::time::sleep(Duration::from_millis(50)).await;
        endpoint.close().await;

        println!("probe=ok");
        println!("session_id={session_id}");
        println!("resolution_stall_smoke=ok");
        println!("resolution_stall_session_id={session_id}");
        println!(
            "resolution_stall_native_dimensions={}x{}",
            evidence.native_dimensions.0, evidence.native_dimensions.1
        );
        println!(
            "resolution_stall_reduced_dimensions={}x{}",
            evidence.reduced_dimensions.0, evidence.reduced_dimensions.1
        );
        println!("resolution_stall_sequence={}", evidence.stall_sequence);
        println!("resolution_stall_fresh_reports={RESOLUTION_STALL_FRESH_REPORTS}");
        println!(
            "resolution_stall_fresh_duration_ms={}",
            evidence.fresh_duration.as_millis()
        );
        println!(
            "resolution_stall_input_ack_micros={}",
            evidence.stall_input_ack_micros
        );
        println!(
            "resolution_resume_input_ack_micros={}",
            evidence.resume_input_ack_micros
        );
        println!(
            "resolution_resume_media_micros={}",
            evidence.resume_media_micros
        );
        println!(
            "resolution_resume_sequence_advance={}",
            evidence.resume_sequence_advance
        );
        return Ok(());
    }

    let started = Instant::now();
    let mut received = 0_u32;
    let mut bytes = 0_u64;
    let mut keyframes = 0_u32;
    let mut gaps = 0_u64;
    let mut media_objects_dropped = 0_u64;
    let mut media_objects_late = 0_u64;
    let mut last_sequence: Option<u64> = None;
    let mut dimensions = None;
    let mut first_accepted_at = None;
    let mut last_accepted_at = None;
    let mut object_receiver_v3 = (media_transport == MediaTransport::GroupedV3).then(|| {
        MediaObjectReceiverV3::new(
            media_connection.clone(),
            Duration::from_secs(args.timeout_seconds),
        )
    });
    let mut object_sequence_v3 = MediaObjectSequenceV3::new();
    let mut keyframe_request_sent = false;
    let mut keyframe_recovery_verified = false;
    let mut keyframe_request_sent_at: Option<Instant> = None;
    let mut keyframe_recovery_micros = None;
    let mut keyframe_request_group_id = None;
    let mut keyframe_request_last_sequence = None;
    let mut slow_consumer_stalled = false;
    let mut slow_consumer_pending_recovery = false;
    let mut slow_consumer_verified = false;
    let mut slow_consumer_group_id = None;
    let mut slow_consumer_last_sequence = None;
    let mut slow_consumer_capture_timestamp_us = None;
    let mut slow_consumer_cancelled_groups_before = None;
    let mut slow_consumer_resumed_at: Option<Instant> = None;
    let mut slow_consumer_recovery_micros = None;
    let mut slow_consumer_cancellation_delta = None;
    let mut slow_consumer_group_advance = None;
    let mut slow_consumer_sequence_advance = None;
    let mut slow_consumer_capture_advance_micros = None;
    let mut slow_consumer_input_ack_micros = None;

    while received < args.frames {
        let (frame, recovery_frame): (AcceptedMedia, bool) = match media_transport {
            MediaTransport::UpstreamMoq => loop {
                let media_wait = if slow_consumer_pending_recovery {
                    SLOW_CONSUMER_RECOVERY_TIMEOUT
                } else {
                    Duration::from_secs(args.timeout_seconds).saturating_add(Duration::from_secs(1))
                };
                let outcome = tokio::time::timeout(
                    media_wait,
                    moq_receiver
                        .as_mut()
                        .expect("upstream MoQ receiver is present")
                        .next(),
                )
                .await
                .context("timed out waiting for an upstream MoQ media object")??
                .context("host closed the upstream MoQ video track")?;
                match outcome {
                    MoqProbeOutcome::Dropped => {
                        media_objects_dropped = media_objects_dropped.saturating_add(1);
                    }
                    MoqProbeOutcome::Frame {
                        frame,
                        group_sequence,
                        recovery,
                    } => {
                        let requested_keyframe_recovery =
                            keyframe_request_sent && !keyframe_recovery_verified;
                        if requested_keyframe_recovery {
                            ensure!(
                                recovery
                                    && frame.header.flags.contains(FrameFlags::KEYFRAME)
                                    && frame.header.flags.contains(FrameFlags::CODEC_CONFIG),
                                "MoQ keyframe request recovery did not begin at a configured IDR barrier"
                            );
                            ensure!(
                                keyframe_request_group_id
                                    .is_some_and(|prior| group_sequence > prior),
                                "MoQ keyframe request recovery did not advance the native group"
                            );
                            ensure!(
                                keyframe_request_last_sequence
                                    .is_some_and(|prior| frame.header.sequence > prior),
                                "MoQ keyframe request recovery did not advance media sequence"
                            );
                            keyframe_recovery_micros = keyframe_request_sent_at.map(|sent_at| {
                                u64::try_from(sent_at.elapsed().as_micros()).unwrap_or(u64::MAX)
                            });
                            keyframe_recovery_verified = true;
                        }
                        let slow_consumer_recovery = slow_consumer_pending_recovery;
                        if slow_consumer_recovery {
                            ensure!(
                                recovery
                                    && frame.header.flags.contains(FrameFlags::KEYFRAME)
                                    && frame.header.flags.contains(FrameFlags::CODEC_CONFIG),
                                "MoQ slow-consumer recovery did not begin at a configured IDR barrier"
                            );
                            let prior_group = slow_consumer_group_id
                                .context("slow-consumer group was not recorded")?;
                            let group_advance = group_sequence.checked_sub(prior_group).context(
                                "MoQ slow-consumer recovery did not advance the native group",
                            )?;
                            ensure!(
                                group_advance > 0,
                                "MoQ slow-consumer recovery did not advance the native group"
                            );
                            let prior_sequence = slow_consumer_last_sequence
                                .context("slow-consumer sequence was not recorded")?;
                            let sequence_advance =
                                frame.header.sequence.checked_sub(prior_sequence).context(
                                    "MoQ slow-consumer recovery did not advance media sequence",
                                )?;
                            ensure!(
                                sequence_advance > 0,
                                "MoQ slow-consumer recovery did not advance media sequence"
                            );
                            let prior_capture_timestamp = slow_consumer_capture_timestamp_us
                                .context("slow-consumer capture timestamp was not recorded")?;
                            let capture_advance = frame
                                .header
                                .capture_timestamp_us
                                .checked_sub(prior_capture_timestamp)
                                .context(
                                    "MoQ slow-consumer recovery capture timestamp moved backward",
                                )?;
                            let minimum_capture_advance = args
                                .slow_consumer_ms
                                .expect("slow-consumer duration is configured")
                                .saturating_mul(1_000)
                                / 2;
                            ensure!(
                                capture_advance >= minimum_capture_advance,
                                "MoQ slow-consumer recovery advanced capture time by only {capture_advance}us; required at least {minimum_capture_advance}us"
                            );
                            let cancelled_before = slow_consumer_cancelled_groups_before
                                .context("slow-consumer cancellation count was not recorded")?;
                            let cancelled_after = moq_receiver
                                .as_ref()
                                .expect("upstream MoQ receiver is present")
                                .cancelled_groups;
                            let cancellation_delta = cancelled_after
                                .checked_sub(cancelled_before)
                                .context("MoQ slow-consumer cancellation count moved backward")?;
                            ensure!(
                                cancellation_delta > 0,
                                "MoQ slow-consumer recovery did not observe a cancelled stale group"
                            );
                            ensure!(
                                moq_receiver
                                    .as_ref()
                                    .expect("upstream MoQ receiver is present")
                                    .historical_suffix_frames
                                    == 0,
                                "MoQ slow-consumer recovery accepted historical delta suffix frames"
                            );
                            let recovery_micros = slow_consumer_resumed_at
                                .context("slow-consumer resume instant was not recorded")?
                                .elapsed()
                                .as_micros();
                            ensure!(
                                recovery_micros <= SLOW_CONSUMER_RECOVERY_TIMEOUT.as_micros(),
                                "MoQ slow-consumer recovery exceeded {}us",
                                SLOW_CONSUMER_RECOVERY_TIMEOUT.as_micros()
                            );
                            slow_consumer_recovery_micros =
                                Some(u64::try_from(recovery_micros).unwrap_or(u64::MAX));
                            slow_consumer_cancellation_delta = Some(cancellation_delta);
                            slow_consumer_group_advance = Some(group_advance);
                            slow_consumer_sequence_advance = Some(sequence_advance);
                            slow_consumer_capture_advance_micros = Some(capture_advance);
                            slow_consumer_pending_recovery = false;
                            slow_consumer_verified = true;
                        }
                        break (
                            AcceptedMedia {
                                flags: frame.header.flags,
                                width: frame.header.width,
                                height: frame.header.height,
                                sequence: frame.header.sequence,
                                capture_timestamp_us: frame.header.capture_timestamp_us,
                                payload_len: frame.payload.len(),
                                v3_group_id: Some(group_sequence),
                            },
                            recovery,
                        );
                    }
                }
            },
            MediaTransport::GroupedV3 => loop {
                let outcome = tokio::time::timeout(
                    Duration::from_secs(args.timeout_seconds)
                        .saturating_add(Duration::from_secs(1)),
                    object_receiver_v3
                        .as_mut()
                        .expect("v3 media object receiver is present")
                        .next(),
                )
                .await
                .context("timed out waiting for media v3 object")??
                .context("host closed the media v3 object connection")?;
                match outcome {
                    MediaObjectOutcomeV3::Dropped { .. } => {
                        if object_sequence_v3.note_drop() {
                            media_objects_dropped = media_objects_dropped.saturating_add(1);
                        } else {
                            media_objects_late = media_objects_late.saturating_add(1);
                        }
                    }
                    MediaObjectOutcomeV3::Malformed {
                        accept_index,
                        error,
                    } => {
                        bail!("media v3 object {accept_index} is malformed: {error}");
                    }
                    MediaObjectOutcomeV3::Object { object, .. } => {
                        match object_sequence_v3.classify(&object) {
                            MediaObjectDecisionV3::Deliver { discontinuity } => {
                                let recovery = keyframe_request_sent && !keyframe_recovery_verified;
                                if recovery {
                                    ensure!(
                                        discontinuity
                                            && object.header.object_id == 0
                                            && object.header.flags.contains(FrameFlags::KEYFRAME)
                                            && object
                                                .header
                                                .flags
                                                .contains(FrameFlags::CODEC_CONFIG)
                                            && object
                                                .header
                                                .flags
                                                .contains(FrameFlags::DISCONTINUITY),
                                        "keyframe request recovery did not begin at a configured discontinuity object zero"
                                    );
                                    ensure!(
                                        keyframe_request_group_id
                                            .is_some_and(
                                                |group_id| object.header.group_id > group_id
                                            ),
                                        "keyframe request recovery did not advance the media group"
                                    );
                                    ensure!(
                                        keyframe_request_last_sequence.is_some_and(|sequence| {
                                            object.header.sequence > sequence
                                        }),
                                        "keyframe request recovery did not advance media sequence"
                                    );
                                    keyframe_recovery_micros =
                                        keyframe_request_sent_at.map(|sent_at| {
                                            u64::try_from(sent_at.elapsed().as_micros())
                                                .unwrap_or(u64::MAX)
                                        });
                                    keyframe_recovery_verified = true;
                                }
                                break (object.into(), recovery);
                            }
                            MediaObjectDecisionV3::DropLate => {
                                media_objects_late = media_objects_late.saturating_add(1);
                            }
                            MediaObjectDecisionV3::DropUntilKeyframe => {
                                media_objects_dropped = media_objects_dropped.saturating_add(1);
                            }
                        }
                    }
                }
            },
        };

        let accepted_at = Instant::now();
        first_accepted_at.get_or_insert(accepted_at);
        last_accepted_at = Some(accepted_at);

        if received == 0 {
            ensure!(
                frame.flags.contains(FrameFlags::KEYFRAME)
                    && frame.flags.contains(FrameFlags::CODEC_CONFIG),
                "first media frame is not a decodable keyframe with codec configuration"
            );
        }

        let frame_dimensions = (frame.width, frame.height);
        if let Some(expected) = dimensions {
            ensure!(
                frame_dimensions == expected,
                "media dimensions changed during probe"
            );
        } else {
            dimensions = Some(frame_dimensions);
        }
        if let Some(previous) = last_sequence
            && !recovery_frame
        {
            gaps += sequence_gap(previous, frame.sequence)?;
        }
        last_sequence = Some(frame.sequence);
        keyframes += u32::from(frame.flags.contains(FrameFlags::KEYFRAME));
        bytes = bytes.saturating_add(frame.payload_len as u64);
        received += 1;

        if let Some(stall_ms) = args.slow_consumer_ms
            && !slow_consumer_stalled
        {
            slow_consumer_group_id = frame.v3_group_id;
            slow_consumer_last_sequence = Some(frame.sequence);
            slow_consumer_capture_timestamp_us = Some(frame.capture_timestamp_us);
            slow_consumer_cancelled_groups_before = Some(
                moq_receiver
                    .as_ref()
                    .expect("upstream MoQ receiver is present")
                    .cancelled_groups,
            );
            let first_half = Duration::from_millis(stall_ms / 2);
            let second_half = Duration::from_millis(stall_ms).saturating_sub(first_half);
            tokio::time::sleep(first_half).await;
            let input_probe_started = Instant::now();
            write_input_event(&mut input_send, &InputEvent::Probe)
                .await
                .context("writing input probe during slow-consumer media stall")?;
            expected_ack = expected_ack.saturating_add(1);
            tokio::time::timeout(
                SLOW_CONSUMER_RECOVERY_TIMEOUT,
                read_expected_input_ack(&mut input_recv, args.timeout_seconds, expected_ack),
            )
            .await
            .context("input acknowledgement was blocked by the slow media consumer")??;
            slow_consumer_input_ack_micros =
                Some(u64::try_from(input_probe_started.elapsed().as_micros()).unwrap_or(u64::MAX));
            tokio::time::sleep(second_half).await;
            slow_consumer_resumed_at = Some(Instant::now());
            slow_consumer_pending_recovery = true;
            slow_consumer_stalled = true;
        }

        if args.keyframe_smoke
            && !keyframe_request_sent
            && received == KEYFRAME_SMOKE_REQUEST_AFTER_FRAMES
        {
            let request = MediaControlRequestV3::request_keyframe(
                args.keyframe_request_id,
                Some(frame.sequence),
                KeyframeRequestReasonV3::DecoderReset,
            );
            tokio::time::timeout(
                Duration::from_secs(1),
                write_media_control_request_v3(&mut media_send, &request),
            )
            .await
            .context("timed out writing v3 keyframe request")??;
            keyframe_request_group_id = frame.v3_group_id;
            keyframe_request_last_sequence = Some(frame.sequence);
            keyframe_request_sent_at = Some(Instant::now());
            keyframe_request_sent = true;
            match media_transport {
                MediaTransport::UpstreamMoq => {
                    let cancelled_group = moq_receiver
                        .as_mut()
                        .expect("upstream MoQ receiver is present")
                        .request_recovery();
                    ensure!(
                        cancelled_group == keyframe_request_group_id,
                        "MoQ recovery did not cancel the currently accepted native group"
                    );
                }
                MediaTransport::GroupedV3 => object_sequence_v3.request_recovery(),
            }
        }
    }

    let Some((width, height)) = dimensions else {
        bail!("probe received no frames");
    };
    if let Some((expected_width, expected_height)) = args.expect_size {
        ensure!(
            (width, height) == (expected_width, expected_height),
            "expected {expected_width}x{expected_height} but received {width}x{height}"
        );
    }
    ensure!(keyframes > 0, "probe received no H.264 keyframe");
    ensure!(gaps == 0, "probe observed {gaps} media sequence gaps");
    ensure!(
        !args.keyframe_smoke || keyframe_recovery_verified,
        "probe did not verify requested keyframe recovery"
    );
    ensure!(
        args.slow_consumer_ms.is_none() || slow_consumer_verified,
        "probe did not verify bounded slow-consumer recovery"
    );
    let accepted_span = first_accepted_at
        .zip(last_accepted_at)
        .map_or(Duration::ZERO, |(first, last)| {
            last.saturating_duration_since(first)
        });
    let accepted_fps = accepted_frame_rate(received, accepted_span);
    if let Some(minimum_fps) = args.minimum_fps {
        let accepted_fps = accepted_fps.context(
            "--minimum-fps requires at least two accepted frames with measurable elapsed time",
        )?;
        ensure!(
            accepted_fps >= minimum_fps,
            "probe sustained only {accepted_fps:.3} accepted fps; required at least {minimum_fps:.3} fps"
        );
    }
    let feedback_evidence = if args.feedback_smoke {
        Some(
            run_feedback_smoke(
                &endpoint,
                address.clone(),
                nonce,
                session_id,
                last_sequence.context("feedback smoke requires an accepted media sequence")?,
            )
            .await?,
        )
    } else {
        None
    };
    let media_diagnostics_connection = moq_receiver
        .as_ref()
        .map_or(&media_connection, MoqProbeReceiver::connection);
    let (media_path_mode, media_path_rtt_ms) =
        selected_path_diagnostics(media_diagnostics_connection);
    let (control_path_mode, control_path_rtt_ms) = if media_transport == MediaTransport::UpstreamMoq
    {
        selected_path_diagnostics(&media_connection)
    } else {
        ("not-used", None)
    };
    let (input_path_mode, input_path_rtt_ms) = selected_path_diagnostics(&input_connection);
    let feedback_path_mode = feedback_evidence
        .as_ref()
        .map_or("not-used", |(_, path_mode, _)| *path_mode);
    if args.relay_only {
        ensure!(
            control_path_mode == "relay"
                && media_path_mode == "relay"
                && input_path_mode == "relay"
                && (feedback_evidence.is_none() || feedback_path_mode == "relay"),
            "relay-only probe did not keep every transport on relay: control={control_path_mode}, media={media_path_mode}, input={input_path_mode}, feedback={feedback_path_mode}"
        );
    }
    let (
        moq_cancelled_groups,
        moq_group_gaps,
        moq_maximum_group_objects,
        moq_maximum_group_bytes,
        moq_historical_suffix_frames,
    ) = moq_receiver.as_ref().map_or((0, 0, 0, 0, 0), |receiver| {
        (
            receiver.cancelled_groups,
            receiver.group_gaps,
            receiver.maximum_group_objects,
            receiver.maximum_group_bytes,
            receiver.historical_suffix_frames,
        )
    });
    input_send.finish().context("finishing input stream")?;
    media_send
        .finish()
        .context("finishing media request stream")?;
    // Close both protocol connections explicitly. Relying only on endpoint
    // teardown can leave a peer waiting for QUIC idle timeout during repeated
    // short-lived probes, which obscures whether the host released its
    // one-client lease deterministically.
    input_connection.close(0_u32.into(), b"probe complete");
    if let Some(receiver) = moq_receiver.as_ref() {
        receiver.connection().close(0_u32.into(), b"probe complete");
    }
    media_connection.close(0_u32.into(), b"probe complete");
    tokio::time::sleep(Duration::from_millis(50)).await;
    endpoint.close().await;

    println!("probe=ok");
    println!("session_id={session_id}");
    println!("frames={received}");
    println!("dimensions={width}x{height}");
    println!("keyframes={keyframes}");
    println!("sequence_gaps={gaps}");
    println!("media_objects_dropped={media_objects_dropped}");
    println!("media_objects_late={media_objects_late}");
    println!("transport={}", media_transport.label());
    println!(
        "control_alpn={}",
        if media_transport == MediaTransport::UpstreamMoq {
            "sigil/control/1"
        } else {
            "not-used"
        }
    );
    println!("transport_alpn={}", media_transport.media_alpn_label());
    println!("first_configured_idr=ok");
    println!("frame_sequence=monotonic");
    println!(
        "group_sequence={}",
        if media_transport == MediaTransport::UpstreamMoq {
            "monotonic"
        } else {
            "not-applicable"
        }
    );
    if media_transport == MediaTransport::UpstreamMoq {
        println!(
            "moq_catalog={}",
            moq_receiver
                .as_ref()
                .expect("upstream MoQ receiver is present")
                .catalog_mode
                .label()
        );
        println!("moq_group_capacity=1");
        println!("moq_cancelled_groups={moq_cancelled_groups}");
        println!("moq_group_gaps={moq_group_gaps}");
        println!("moq_unrecovered_group_gaps=0");
        println!("moq_maximum_group_objects={moq_maximum_group_objects}");
        println!("moq_maximum_group_bytes={moq_maximum_group_bytes}");
        println!("moq_historical_suffix_frames={moq_historical_suffix_frames}");
    } else {
        println!("moq_catalog=not-applicable");
        for field in [
            "moq_group_capacity",
            "moq_cancelled_groups",
            "moq_group_gaps",
            "moq_unrecovered_group_gaps",
            "moq_maximum_group_objects",
            "moq_maximum_group_bytes",
            "moq_historical_suffix_frames",
        ] {
            println!("{field}=not-applicable");
        }
    }
    println!(
        "recovery_barrier={}",
        if args.keyframe_smoke {
            "configured-idr"
        } else {
            "not-requested"
        }
    );
    println!(
        "keyframe_recovery={}",
        if args.keyframe_smoke {
            "ok"
        } else {
            "not-requested"
        }
    );
    if args.keyframe_smoke {
        println!("keyframe_request_id={}", args.keyframe_request_id);
        println!(
            "keyframe_recovery_micros={}",
            keyframe_recovery_micros.context("keyframe recovery latency was not measured")?
        );
    } else {
        println!("keyframe_request_id=not-requested");
        println!("keyframe_recovery_micros=not-requested");
    }
    if let Some(stall_ms) = args.slow_consumer_ms {
        println!("slow_consumer=ok");
        println!("slow_consumer_stall_ms={stall_ms}");
        println!("slow_consumer_first_post_stall=configured-idr");
        println!("slow_consumer_historical_suffix_frames=0");
        println!(
            "slow_consumer_recovery_micros={}",
            slow_consumer_recovery_micros
                .context("slow-consumer recovery latency was not measured")?
        );
        println!(
            "slow_consumer_cancellation_delta={}",
            slow_consumer_cancellation_delta
                .context("slow-consumer cancellation delta was not measured")?
        );
        println!(
            "slow_consumer_group_advance={}",
            slow_consumer_group_advance.context("slow-consumer group advance was not measured")?
        );
        println!(
            "slow_consumer_sequence_advance={}",
            slow_consumer_sequence_advance
                .context("slow-consumer sequence advance was not measured")?
        );
        println!(
            "slow_consumer_capture_advance_micros={}",
            slow_consumer_capture_advance_micros
                .context("slow-consumer capture advance was not measured")?
        );
        println!(
            "slow_consumer_input_ack_micros={}",
            slow_consumer_input_ack_micros
                .context("slow-consumer input acknowledgement was not measured")?
        );
    } else {
        for field in [
            "slow_consumer",
            "slow_consumer_stall_ms",
            "slow_consumer_first_post_stall",
            "slow_consumer_historical_suffix_frames",
            "slow_consumer_recovery_micros",
            "slow_consumer_cancellation_delta",
            "slow_consumer_group_advance",
            "slow_consumer_sequence_advance",
            "slow_consumer_capture_advance_micros",
            "slow_consumer_input_ack_micros",
        ] {
            println!("{field}=not-requested");
        }
    }
    match accepted_fps {
        Some(fps) => println!("accepted_fps={fps:.3}"),
        None => println!("accepted_fps=unknown"),
    }
    println!("input_ack_micros={input_ack_micros}");
    println!(
        "pointer_smoke={}",
        if args.pointer_smoke {
            "ok"
        } else {
            "not-requested"
        }
    );
    println!(
        "pointer_feedback_smoke={}",
        if args.pointer_feedback_smoke {
            "ok"
        } else {
            "not-requested"
        }
    );
    match pointer_sync_position {
        Some(position) => println!("pointer_sync_position={},{}", position.x, position.y),
        None => println!("pointer_sync_position=not-requested"),
    }
    match pointer_motion_position {
        Some(position) => println!("pointer_motion_position={},{}", position.x, position.y),
        None => println!("pointer_motion_position=not-requested"),
    }
    println!(
        "gamepad_smoke={}",
        if args.gamepad_smoke {
            "ok"
        } else {
            "not-requested"
        }
    );
    if let Some((decision, path_mode, path_rtt_ms)) = &feedback_evidence {
        println!("feedback_smoke=ok");
        println!("feedback_report_id={}", decision.report_id);
        println!("feedback_state={:?}", decision.state);
        println!("feedback_target_kbps={}", decision.target_kbps);
        println!("feedback_floor_kbps={}", decision.floor_kbps);
        println!("feedback_ceiling_kbps={}", decision.ceiling_kbps);
        println!("feedback_applied={}", decision.applied);
        println!("feedback_path_mode={path_mode}");
        match path_rtt_ms {
            Some(rtt) => println!("feedback_path_rtt_ms={rtt:.3}"),
            None => println!("feedback_path_rtt_ms=unknown"),
        }
    } else {
        for field in [
            "feedback_smoke",
            "feedback_report_id",
            "feedback_state",
            "feedback_target_kbps",
            "feedback_floor_kbps",
            "feedback_ceiling_kbps",
            "feedback_applied",
            "feedback_path_mode",
            "feedback_path_rtt_ms",
        ] {
            println!("{field}=not-requested");
        }
    }
    println!("path_mode={media_path_mode}");
    match media_path_rtt_ms {
        Some(rtt) => println!("path_rtt_ms={rtt:.3}"),
        None => println!("path_rtt_ms=unknown"),
    }
    println!("control_path_mode={control_path_mode}");
    match control_path_rtt_ms {
        Some(rtt) => println!("control_path_rtt_ms={rtt:.3}"),
        None => println!("control_path_rtt_ms=unknown"),
    }
    println!("media_path_mode={media_path_mode}");
    match media_path_rtt_ms {
        Some(rtt) => println!("media_path_rtt_ms={rtt:.3}"),
        None => println!("media_path_rtt_ms=unknown"),
    }
    println!("input_path_mode={input_path_mode}");
    match input_path_rtt_ms {
        Some(rtt) => println!("input_path_rtt_ms={rtt:.3}"),
        None => println!("input_path_rtt_ms=unknown"),
    }
    println!(
        "forced_relay={}",
        if args.relay_only {
            "ok"
        } else {
            "not-requested"
        }
    );
    println!("encoded_bytes={bytes}");
    println!("elapsed_ms={}", started.elapsed().as_millis());
    Ok(())
}

fn pointer_smoke_events(
    pointer_surface_dimensions: Option<PointerSurfaceDimensions>,
) -> Result<[InputEvent; 3]> {
    let dimensions = pointer_surface_dimensions
        .context("host did not advertise pointer surface dimensions required by --pointer-smoke")?;
    let [sync_position, _motion_position] = pointer_smoke_expected_positions(dimensions)?;
    Ok([
        InputEvent::MousePositionSync {
            x: sync_position.x,
            y: sync_position.y,
        },
        InputEvent::MouseMoveRelative { dx: 32, dy: 16 },
        InputEvent::MouseClick { b: 1 },
    ])
}

fn pointer_smoke_expected_positions(
    dimensions: PointerSurfaceDimensions,
) -> Result<[PointerPosition; 2]> {
    let sync_position = PointerPosition::new(
        i32::from(dimensions.width / 2),
        i32::from(dimensions.height / 2),
    )?;
    let motion_position = PointerPosition::new(
        sync_position
            .x
            .checked_add(32)
            .context("pointer smoke X position overflowed")?,
        sync_position
            .y
            .checked_add(16)
            .context("pointer smoke Y position overflowed")?,
    )?;
    Ok([sync_position, motion_position])
}

fn sequence_gap(previous: u64, current: u64) -> Result<u64> {
    let Some(expected) = previous.checked_add(1) else {
        bail!("media sequence overflowed after {previous}");
    };
    ensure!(
        current >= expected,
        "non-monotonic media sequence: previous={previous}, current={current}"
    );
    Ok(current - expected)
}

fn accepted_frame_rate(frame_count: u32, elapsed: Duration) -> Option<f64> {
    if frame_count < 2 || elapsed.is_zero() {
        return None;
    }
    Some(f64::from(frame_count - 1) / elapsed.as_secs_f64())
}

fn selected_path_diagnostics(
    connection: &iroh::endpoint::Connection,
) -> (&'static str, Option<f64>) {
    let paths = connection.paths();
    let Some(path) = paths.iter().find(|path| path.is_selected()) else {
        return ("unknown", None);
    };
    let mode = if path.is_ip() {
        "direct"
    } else if path.is_relay() {
        "relay"
    } else {
        "custom"
    };
    (mode, Some(path.rtt().as_secs_f64() * 1000.0))
}

fn parse_size(value: &str) -> std::result::Result<(u16, u16), String> {
    let (width, height) = value
        .split_once('x')
        .ok_or_else(|| "size must be WIDTHxHEIGHT".to_owned())?;
    let width = width.parse().map_err(|_| "invalid width".to_owned())?;
    let height = height.parse().map_err(|_| "invalid height".to_owned())?;
    if width == 0 || height == 0 {
        return Err("dimensions must be non-zero".to_owned());
    }
    Ok((width, height))
}

fn parse_minimum_fps(value: &str) -> std::result::Result<f64, String> {
    let minimum_fps = value
        .parse::<f64>()
        .map_err(|_| "minimum fps must be a number".to_owned())?;
    if !minimum_fps.is_finite() || minimum_fps <= 0.0 || minimum_fps > 240.0 {
        return Err("minimum fps must be finite and within (0, 240]".to_owned());
    }
    Ok(minimum_fps)
}

fn load_invitation_file(
    path: &Path,
    expected_host: EndpointId,
    expected_peer: EndpointId,
    required_grants: InvitationGrants,
) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs();
    load_invitation_file_at(path, expected_host, expected_peer, required_grants, now)
}

fn load_invitation_file_at(
    path: &Path,
    expected_host: EndpointId,
    expected_peer: EndpointId,
    required_grants: InvitationGrants,
    now: u64,
) -> Result<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(path).with_context(|| {
        format!(
            "opening invitation {} without following symlinks",
            path.display()
        )
    })?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspecting invitation {}", path.display()))?;
    ensure!(metadata.is_file(), "invitation must be a regular file");
    ensure!(
        metadata.len() <= MAX_INVITATION_FILE_BYTES,
        "invitation {} exceeds the bounded file size",
        path.display()
    );
    #[cfg(unix)]
    {
        ensure!(
            metadata.mode() & 0o077 == 0,
            "invitation {} must not be readable or writable by group or other users",
            path.display()
        );
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "invitation {} is not owned by the current user",
            path.display()
        );
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_INVITATION_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading invitation {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_INVITATION_FILE_BYTES,
        "invitation {} grew beyond the bounded file size while being read",
        path.display()
    );
    let token_bytes = bytes
        .strip_suffix(b"\r\n")
        .or_else(|| bytes.strip_suffix(b"\n"))
        .unwrap_or(&bytes);
    ensure!(
        !token_bytes.is_empty() && token_bytes.len() <= MAX_INVITATION_TOKEN_LEN,
        "invitation token has an invalid length"
    );
    ensure!(
        !token_bytes.contains(&b'\n') && !token_bytes.contains(&b'\r'),
        "invitation file contains unexpected line breaks"
    );
    let token = std::str::from_utf8(token_bytes).context("invitation file is not UTF-8")?;
    let invitation =
        SignedInvitation::decode(token).context("decoding and verifying invitation file")?;
    ensure!(
        invitation.claims.host_node_id == *expected_host.as_bytes(),
        "invitation belongs to a different Sigil host"
    );
    ensure!(
        invitation.claims.intended_peer_id == *expected_peer.as_bytes(),
        "invitation is bound to a different probe identity"
    );
    ensure!(
        invitation.claims.grants.contains(required_grants),
        "invitation does not grant every requested probe capability"
    );
    ensure!(
        invitation
            .claims
            .grants
            .contains(InvitationGrants::POINTER_KEYBOARD)
            || invitation.claims.grants.contains(InvitationGrants::GAMEPAD),
        "invitation must grant pointer/keyboard or gamepad for the probe input acknowledgment"
    );
    ensure!(
        now >= invitation
            .claims
            .issued_at_unix
            .saturating_sub(INVITATION_CLOCK_SKEW_SECS),
        "invitation was issued too far in the future"
    );
    ensure!(
        now <= invitation.claims.expires_at_unix,
        "invitation has expired"
    );
    Ok(token.to_owned())
}

fn probe_client_hello(
    nonce: [u8; 16],
    capabilities: Vec<Capability>,
    invitation: Option<&str>,
) -> ClientHello {
    let hello = ClientHello::new("sigil-probe/0.1.0", nonce, capabilities);
    match invitation {
        Some(invitation) => hello.with_invitation(invitation),
        None => hello,
    }
}

async fn negotiate(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    nonce: [u8; 16],
    capabilities: Vec<Capability>,
    required: Capability,
    name: &str,
    invitation: Option<&str>,
) -> Result<Negotiated> {
    write_client_hello(send, &probe_client_hello(nonce, capabilities, invitation))
        .await
        .with_context(|| format!("writing {name} hello"))?;
    let response = tokio::time::timeout(Duration::from_secs(10), read_host_hello(recv))
        .await
        .with_context(|| format!("timed out waiting for {name} hello"))??
        .with_context(|| format!("host closed during {name} hello"))?;
    if !response.accepted {
        bail!(
            "host rejected {name} stream: {}",
            response.message.as_deref().unwrap_or("unspecified reason")
        );
    }
    ensure!(
        response.capabilities.contains(&required),
        "host accepted {name} without required capability {required:?}"
    );
    let session_id = response
        .session_id
        .with_context(|| format!("host omitted {name} session ID"))?;
    Ok(Negotiated {
        session_id,
        capabilities: response.capabilities,
        pointer_surface_dimensions: response.pointer_surface_dimensions,
    })
}

struct Negotiated {
    session_id: u64,
    capabilities: Vec<Capability>,
    pointer_surface_dimensions: Option<PointerSurfaceDimensions>,
}

async fn read_expected_input_ack<R>(
    recv: &mut R,
    timeout_seconds: u64,
    expected_sequence: u64,
) -> Result<InputAck>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds.min(5));
    read_expected_input_ack_before(recv, deadline, expected_sequence).await
}

async fn read_expected_input_ack_before<R>(
    recv: &mut R,
    deadline: tokio::time::Instant,
    expected_sequence: u64,
) -> Result<InputAck>
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let input_ack = tokio::time::timeout_at(deadline, read_input_ack(recv))
            .await
            .context("timed out waiting for input acknowledgment")??
            .context("host closed before input acknowledgment")?;
        ensure!(
            input_ack.sequence <= expected_sequence,
            "unexpected input acknowledgment sequence {}; expected at most {expected_sequence}",
            input_ack.sequence
        );
        if input_ack.sequence == expected_sequence {
            return Ok(input_ack);
        }
    }
}

async fn confirm_pointer_position<S, R>(
    send: &mut S,
    recv: &mut R,
    expected_sequence: &mut u64,
    mut input_ack: InputAck,
    expected_position: PointerPosition,
    timeout: Duration,
) -> Result<PointerPosition>
where
    S: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    ensure!(
        !timeout.is_zero(),
        "pointer feedback confirmation timeout must be nonzero"
    );
    let deadline = tokio::time::Instant::now() + timeout.min(Duration::from_secs(5));
    loop {
        if input_ack.pointer_position == Some(expected_position) {
            return Ok(expected_position);
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            bail!(
                "compositor pointer position did not converge to {},{}; last observed position was {:?}",
                expected_position.x,
                expected_position.y,
                input_ack.pointer_position
            );
        }
        tokio::time::sleep_until(std::cmp::min(now + Duration::from_millis(16), deadline)).await;
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "compositor pointer position did not converge to {},{}; last observed position was {:?}",
                expected_position.x,
                expected_position.y,
                input_ack.pointer_position
            );
        }
        write_input_event(send, &InputEvent::Probe)
            .await
            .context("writing pointer feedback convergence probe")?;
        *expected_sequence = expected_sequence
            .checked_add(1)
            .context("input acknowledgment sequence overflowed")?;
        input_ack = match read_expected_input_ack_before(recv, deadline, *expected_sequence).await {
            Ok(input_ack) => input_ack,
            Err(error) if tokio::time::Instant::now() >= deadline => {
                bail!(
                    "compositor pointer position did not converge to {},{}; last observed position was {:?}: {error:#}",
                    expected_position.x,
                    expected_position.y,
                    input_ack.pointer_position
                );
            }
            Err(error) => return Err(error),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn expected_input_ack_skips_older_feedback_under_one_deadline() {
        let (mut sender, mut receiver) = tokio::io::duplex(512);
        for sequence in [0, 0, 1] {
            sigil_protocol::write_input_ack(
                &mut sender,
                &sigil_protocol::InputAck {
                    sequence,
                    pointer_position: None,
                    pointer_visible: Some(false),
                },
            )
            .await
            .unwrap();
        }

        let input_ack = read_expected_input_ack(&mut receiver, 1, 1).await.unwrap();

        assert_eq!(input_ack.sequence, 1);
    }

    #[tokio::test]
    async fn expected_input_ack_rejects_future_feedback() {
        let (mut sender, mut receiver) = tokio::io::duplex(256);
        sigil_protocol::write_input_ack(
            &mut sender,
            &sigil_protocol::InputAck {
                sequence: 2,
                pointer_position: None,
                pointer_visible: Some(false),
            },
        )
        .await
        .unwrap();

        let error = read_expected_input_ack(&mut receiver, 1, 1)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("expected at most 1"));
    }

    #[tokio::test]
    async fn pointer_confirmation_polls_until_the_tracker_reports_the_exact_position() {
        let target = PointerPosition::new(1_280, 800).unwrap();
        let initial_ack = InputAck {
            sequence: 2,
            pointer_position: Some(PointerPosition::new(10, 20).unwrap()),
            pointer_visible: Some(false),
        };
        let (client, server) = tokio::io::duplex(512);
        let (mut client_recv, mut client_send) = tokio::io::split(client);
        let (mut server_recv, mut server_send) = tokio::io::split(server);
        let server_task = tokio::spawn(async move {
            let event = sigil_protocol::read_input_event(&mut server_recv)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(event, InputEvent::Probe);
            sigil_protocol::write_input_ack(
                &mut server_send,
                &InputAck {
                    sequence: 3,
                    pointer_position: Some(target),
                    pointer_visible: Some(false),
                },
            )
            .await
            .unwrap();
        });
        let mut expected_sequence = 2;

        let observed = confirm_pointer_position(
            &mut client_send,
            &mut client_recv,
            &mut expected_sequence,
            initial_ack,
            target,
            Duration::from_secs(1),
        )
        .await
        .unwrap();

        assert_eq!(observed, target);
        assert_eq!(expected_sequence, 3);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn pointer_confirmation_fails_when_the_tracker_never_converges() {
        let target = PointerPosition::new(1_280, 800).unwrap();
        let initial_ack = InputAck {
            sequence: 2,
            pointer_position: Some(PointerPosition::new(10, 20).unwrap()),
            pointer_visible: Some(false),
        };
        let (client, server_guard) = tokio::io::duplex(512);
        let (mut client_recv, mut client_send) = tokio::io::split(client);
        let mut expected_sequence = 2;

        let error = confirm_pointer_position(
            &mut client_send,
            &mut client_recv,
            &mut expected_sequence,
            initial_ack,
            target,
            Duration::from_millis(25),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("did not converge to 1280,800"));
        drop(server_guard);
    }

    #[test]
    fn feedback_report_is_complete_and_keeps_the_observed_sequence() {
        let report = complete_feedback_report(7, MediaFeedbackFlags::NONE, 42);
        report.validate().unwrap();
        assert_eq!(report.report_id, 7);
        assert_eq!(report.interval_ms, 1_000);
        assert_eq!(report.last_sequence, Some(42));
        assert_eq!(report.frontend_queue_depth, 0);
        assert_eq!(report.decode_queue_depth, 0);
        assert_eq!(report.presenter_queue_depth, 0);
        assert_eq!(report.transport_delivery_p95_ms, Some(10));
        assert_eq!(report.decode_p95_ms, Some(3));
        assert_eq!(report.presentation_p95_ms, Some(5));

        let later = complete_feedback_report(8, MediaFeedbackFlags::NONE, 42);
        assert_eq!(later.last_sequence, report.last_sequence);
    }

    #[test]
    fn resolution_stall_reduced_dimensions_are_exact_even_and_same_aspect() {
        assert_eq!(
            resolution_stall_reduced_dimensions((2_560, 1_600)).unwrap(),
            (1_920, 1_200)
        );
        assert_eq!(
            resolution_stall_reduced_dimensions((1_280, 800)).unwrap(),
            (960, 600)
        );
        assert!(resolution_stall_reduced_dimensions((1_278, 800)).is_err());
        assert!(resolution_stall_reduced_dimensions((80, 64)).is_err());
    }

    #[test]
    fn sequence_checks_reject_duplicates_regressions_and_overflow() {
        assert_eq!(sequence_gap(41, 42).unwrap(), 0);
        assert_eq!(sequence_gap(41, 45).unwrap(), 3);
        assert!(sequence_gap(41, 41).is_err());
        assert!(sequence_gap(41, 40).is_err());
        assert!(sequence_gap(u64::MAX, 0).is_err());
    }

    #[test]
    fn pointer_smoke_uses_negotiated_native_surface_center_and_order() {
        let dimensions = PointerSurfaceDimensions::new(2_560, 1_600).unwrap();

        assert_eq!(
            pointer_smoke_events(Some(dimensions)).unwrap(),
            [
                InputEvent::MousePositionSync { x: 1_280, y: 800 },
                InputEvent::MouseMoveRelative { dx: 32, dy: 16 },
                InputEvent::MouseClick { b: 1 },
            ]
        );
        assert_eq!(
            pointer_smoke_expected_positions(dimensions).unwrap(),
            [
                PointerPosition::new(1_280, 800).unwrap(),
                PointerPosition::new(1_312, 816).unwrap(),
            ]
        );
    }

    #[test]
    fn pointer_smoke_requires_negotiated_native_surface_dimensions() {
        let error = pointer_smoke_events(None).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("host did not advertise pointer surface dimensions")
        );
    }

    fn media_frame(sequence: u64, flags: FrameFlags) -> MediaFrame {
        let payload = vec![0x65, 0x88, 0x84];
        MediaFrame::new(
            sigil_protocol::MediaFrameHeader::h264(
                1_280,
                800,
                payload.len(),
                sequence,
                sequence * 1_000,
                i64::try_from(sequence * 1_000).unwrap(),
                flags,
            )
            .unwrap(),
            payload,
        )
        .unwrap()
    }

    fn media_object_v3(
        group_id: u64,
        object_id: u32,
        sequence: u64,
        flags: FrameFlags,
    ) -> MediaObjectV3 {
        let payload = vec![0x65, 0x88, 0x84];
        let header = sigil_protocol::MediaObjectHeaderV3::h264(
            1_280,
            800,
            payload.len(),
            if object_id == 0 { 0 } else { 128 },
            flags,
            object_id,
            group_id,
            sequence,
            sequence * 1_000,
            i64::try_from(sequence * 1_000).unwrap(),
            100,
        )
        .unwrap();
        MediaObjectV3::new(header, payload).unwrap()
    }

    fn media_outcome_v3(
        accept_index: u64,
        group_id: u64,
        object_id: u32,
        sequence: u64,
        flags: FrameFlags,
    ) -> MediaObjectOutcomeV3 {
        MediaObjectOutcomeV3::Object {
            accept_index,
            object: media_object_v3(group_id, object_id, sequence, flags),
        }
    }

    #[test]
    fn media_v3_discontinuity_object_zero_fast_forwards_accept_order() {
        let keyframe = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let barrier = keyframe.union(FrameFlags::DISCONTINUITY);
        let mut reorder = MediaObjectReorderV3::new(0);
        let mut sequence = MediaObjectSequenceV3::new();

        assert!(
            reorder
                .push(media_outcome_v3(1, 10, 1, 11, FrameFlags::NONE))
                .unwrap()
                .is_none()
        );
        let recovered = reorder
            .push(media_outcome_v3(2, 20, 0, 20, barrier))
            .unwrap()
            .unwrap();
        assert_eq!(recovered.accept_index(), 2);
        let MediaObjectOutcomeV3::Object { object, .. } = recovered else {
            panic!("recovery barrier must remain an object");
        };
        assert_eq!(
            sequence.classify(&object),
            MediaObjectDecisionV3::Deliver {
                discontinuity: true
            }
        );
        assert_eq!(reorder.pending_len(), 0);
        assert!(
            reorder
                .push(MediaObjectOutcomeV3::Dropped { accept_index: 0 })
                .unwrap()
                .is_none()
        );
        let next = reorder
            .push(media_outcome_v3(3, 20, 1, 21, FrameFlags::NONE))
            .unwrap()
            .unwrap();
        let MediaObjectOutcomeV3::Object { object, .. } = next else {
            panic!("recovered-group delta must remain an object");
        };
        assert_eq!(
            sequence.classify(&object),
            MediaObjectDecisionV3::Deliver {
                discontinuity: false
            }
        );
    }

    #[test]
    fn minimum_fps_parser_and_first_to_last_evaluation_are_bounded() {
        assert_eq!(parse_minimum_fps("55").unwrap(), 55.0);
        assert_eq!(parse_minimum_fps("240").unwrap(), 240.0);
        for invalid in ["0", "-1", "240.1", "NaN", "inf", "not-a-number"] {
            assert!(parse_minimum_fps(invalid).is_err(), "{invalid}");
        }

        assert_eq!(accepted_frame_rate(56, Duration::from_secs(1)), Some(55.0));
        assert_eq!(accepted_frame_rate(1, Duration::from_secs(1)), None);
        assert_eq!(accepted_frame_rate(2, Duration::ZERO), None);
        assert!(accepted_frame_rate(56, Duration::from_secs(1)).unwrap() >= 55.0);
        assert!(accepted_frame_rate(55, Duration::from_secs(1)).unwrap() < 55.0);
    }

    #[test]
    fn media_v3_enforces_wire_group_object_and_sequence_continuity() {
        let keyframe = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let barrier = keyframe.union(FrameFlags::DISCONTINUITY);
        let mut sequence = MediaObjectSequenceV3::new();

        assert_eq!(
            sequence.classify(&media_object_v3(10, 0, 10, keyframe)),
            MediaObjectDecisionV3::Deliver {
                discontinuity: true
            }
        );
        assert_eq!(
            sequence.classify(&media_object_v3(10, 1, 11, FrameFlags::NONE)),
            MediaObjectDecisionV3::Deliver {
                discontinuity: false
            }
        );
        assert_eq!(
            sequence.classify(&media_object_v3(10, 3, 13, FrameFlags::NONE)),
            MediaObjectDecisionV3::DropUntilKeyframe
        );
        assert_eq!(
            sequence.classify(&media_object_v3(10, 2, 12, FrameFlags::NONE)),
            MediaObjectDecisionV3::DropUntilKeyframe
        );
        assert_eq!(
            sequence.classify(&media_object_v3(20, 0, 20, barrier)),
            MediaObjectDecisionV3::Deliver {
                discontinuity: true
            }
        );
        assert_eq!(
            sequence.classify(&media_object_v3(10, 2, 12, FrameFlags::NONE)),
            MediaObjectDecisionV3::DropLate
        );
    }

    #[test]
    fn explicit_v3_recovery_never_delivers_same_group_delta_history() {
        let keyframe = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let barrier = keyframe.union(FrameFlags::DISCONTINUITY);
        let mut sequence = MediaObjectSequenceV3::new();

        assert!(matches!(
            sequence.classify(&media_object_v3(10, 0, 10, keyframe)),
            MediaObjectDecisionV3::Deliver { .. }
        ));
        assert!(matches!(
            sequence.classify(&media_object_v3(10, 1, 11, FrameFlags::NONE)),
            MediaObjectDecisionV3::Deliver { .. }
        ));
        sequence.request_recovery();
        assert_eq!(
            sequence.classify(&media_object_v3(10, 2, 12, FrameFlags::NONE)),
            MediaObjectDecisionV3::DropUntilKeyframe
        );
        assert_eq!(
            sequence.classify(&media_object_v3(20, 0, 20, keyframe)),
            MediaObjectDecisionV3::DropUntilKeyframe,
            "recovery after skipped history must be explicitly discontinuous"
        );
        assert_eq!(
            sequence.classify(&media_object_v3(30, 0, 30, barrier)),
            MediaObjectDecisionV3::Deliver {
                discontinuity: true
            }
        );
    }

    #[test]
    fn media_v3_group_payload_is_bounded_by_the_shared_protocol_limit() {
        let keyframe = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let mut sequence = MediaObjectSequenceV3::new();
        assert!(matches!(
            sequence.classify(&media_object_v3(1, 0, 1, keyframe)),
            MediaObjectDecisionV3::Deliver { .. }
        ));
        sequence.group_payload_bytes = MAX_MEDIA_GROUP_BYTES_V3;
        assert_eq!(
            sequence.classify(&media_object_v3(1, 1, 2, FrameFlags::NONE)),
            MediaObjectDecisionV3::DropUntilKeyframe
        );
    }

    #[test]
    fn upstream_moq_group_and_object_limits_fail_closed() {
        assert!(!classify_moq_probe_group(None, 8).unwrap());
        assert!(!classify_moq_probe_group(Some(8), 9).unwrap());
        assert!(classify_moq_probe_group(Some(8), 10).unwrap());
        assert!(classify_moq_probe_group(Some(8), 8).is_err());
        assert!(classify_moq_probe_group(Some(8), 7).is_err());

        assert_eq!(validate_moq_probe_object_bounds(4, 0, 0, 42).unwrap(), 42);
        assert!(
            validate_moq_probe_object_bounds(4, MAX_MEDIA_OBJECT_ID_V3 as usize + 1, 0, 1,)
                .is_err()
        );
        assert!(validate_moq_probe_object_bounds(4, 0, MAX_MEDIA_GROUP_BYTES_V3, 1).is_err());
        assert!(validate_moq_probe_object_bounds(4, 0, usize::MAX, 1).is_err());
    }

    #[test]
    fn upstream_moq_requires_configured_group_zero_and_idr_recovery() {
        let keyframe = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let recovery = keyframe.union(FrameFlags::DISCONTINUITY);
        let delta = media_frame(11, FrameFlags::NONE);
        let configured = media_frame(10, keyframe);
        let configured_next = media_frame(12, keyframe);
        let recovered = media_frame(20, recovery);

        assert!(validate_moq_probe_frame(3, true, None, &delta).is_err());
        assert!(validate_moq_probe_frame(3, true, None, &configured).is_ok());
        assert!(validate_moq_probe_frame(4, true, Some(11), &configured).is_err());
        assert!(validate_moq_probe_frame(4, true, Some(11), &configured_next).is_ok());
        assert!(validate_moq_probe_frame(4, true, Some(11), &recovered).is_ok());
        assert!(validate_moq_probe_frame(4, false, Some(10), &delta).is_ok());
        assert!(validate_moq_probe_frame(4, false, Some(9), &delta).is_err());
    }

    #[test]
    fn upstream_moq_transport_reports_the_pinned_native_alpn() {
        assert_eq!(MediaTransport::UpstreamMoq.label(), "iroh-moq");
        assert_eq!(MediaTransport::UpstreamMoq.alpn(), CONTROL_ALPN_V1);
        assert_eq!(
            MediaTransport::UpstreamMoq.media_alpn_label(),
            "moq-lite-04"
        );
        assert_eq!(
            MediaTransport::UpstreamMoq.media_alpn_label().as_bytes(),
            iroh_moq::ALPN
        );
    }

    fn test_invitation(
        host: &SecretKey,
        peer: &SecretKey,
        grants: InvitationGrants,
        issued_at_unix: u64,
        expires_at_unix: u64,
    ) -> String {
        let claims = sigil_protocol::InvitationClaims::new(
            *host.public().as_bytes(),
            *peer.public().as_bytes(),
            issued_at_unix,
            expires_at_unix,
            1,
            [0x55; 32],
            grants,
        )
        .unwrap();
        SignedInvitation::issue(claims, &host.to_bytes())
            .unwrap()
            .encode()
    }

    fn write_private_invitation(path: &Path, token: &str, ending: &[u8]) {
        let mut bytes = token.as_bytes().to_vec();
        bytes.extend_from_slice(ending);
        std::fs::write(path, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn persistent_probe_credentials_are_explicit_and_paired() {
        let node_id = SecretKey::from_bytes(&[0x2a; 32]).public().to_string();
        assert!(
            Args::try_parse_from([
                "sigil-probe",
                "--node-id",
                &node_id,
                "--invitation",
                "probe.goq-invite",
            ])
            .is_err()
        );
        let persistent = Args::try_parse_from([
            "sigil-probe",
            "--node-id",
            &node_id,
            "--identity",
            "probe.key",
            "--invitation",
            "probe.goq-invite",
        ])
        .unwrap();
        assert_eq!(persistent.identity, Some(PathBuf::from("probe.key")));
        assert_eq!(
            persistent.invitation,
            Some(PathBuf::from("probe.goq-invite"))
        );
    }

    #[test]
    fn private_invitation_preflight_accepts_only_the_bound_current_grants() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("probe.goq-invite");
        let host = SecretKey::from_bytes(&[0x31; 32]);
        let peer = SecretKey::from_bytes(&[0x32; 32]);
        let other = SecretKey::from_bytes(&[0x33; 32]);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let token = test_invitation(
            &host,
            &peer,
            InvitationGrants::VIEW.union(InvitationGrants::GAMEPAD),
            now.saturating_sub(1),
            now + 600,
        );

        write_private_invitation(&path, &token, b"\n");
        assert_eq!(
            load_invitation_file_at(
                &path,
                host.public(),
                peer.public(),
                InvitationGrants::VIEW.union(InvitationGrants::GAMEPAD),
                now,
            )
            .unwrap(),
            token
        );
        write_private_invitation(&path, &token, b"\r\n");
        assert!(
            load_invitation_file_at(
                &path,
                host.public(),
                peer.public(),
                InvitationGrants::VIEW,
                now,
            )
            .is_ok()
        );
        assert!(
            load_invitation_file_at(
                &path,
                other.public(),
                peer.public(),
                InvitationGrants::VIEW,
                now,
            )
            .is_err()
        );
        assert!(
            load_invitation_file_at(
                &path,
                host.public(),
                other.public(),
                InvitationGrants::VIEW,
                now,
            )
            .is_err()
        );
        assert!(
            load_invitation_file_at(
                &path,
                host.public(),
                peer.public(),
                InvitationGrants::POINTER_KEYBOARD,
                now,
            )
            .is_err()
        );

        let view_only = test_invitation(
            &host,
            &peer,
            InvitationGrants::VIEW,
            now.saturating_sub(1),
            now + 600,
        );
        write_private_invitation(&path, &view_only, b"\n");
        assert!(
            load_invitation_file_at(
                &path,
                host.public(),
                peer.public(),
                InvitationGrants::VIEW,
                now,
            )
            .unwrap_err()
            .to_string()
            .contains("input acknowledgment")
        );

        let expired = test_invitation(
            &host,
            &peer,
            InvitationGrants::VIEW.union(InvitationGrants::GAMEPAD),
            now.saturating_sub(120),
            now.saturating_sub(60),
        );
        write_private_invitation(&path, &expired, b"\n");
        assert!(
            load_invitation_file_at(
                &path,
                host.public(),
                peer.public(),
                InvitationGrants::VIEW,
                now,
            )
            .unwrap_err()
            .to_string()
            .contains("expired")
        );
        write_private_invitation(&path, &token, b"\n\n");
        assert!(
            load_invitation_file_at(
                &path,
                host.public(),
                peer.public(),
                InvitationGrants::VIEW,
                now,
            )
            .is_err()
        );
        let future = test_invitation(
            &host,
            &peer,
            InvitationGrants::VIEW.union(InvitationGrants::GAMEPAD),
            now + INVITATION_CLOCK_SKEW_SECS + 1,
            now + INVITATION_CLOCK_SKEW_SECS + 61,
        );
        write_private_invitation(&path, &future, b"\n");
        assert!(
            load_invitation_file_at(
                &path,
                host.public(),
                peer.public(),
                InvitationGrants::VIEW,
                now,
            )
            .unwrap_err()
            .to_string()
            .contains("future")
        );
    }

    #[cfg(unix)]
    #[test]
    fn invitation_reader_rejects_unsafe_files() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("probe.goq-invite");
        let link = temp.path().join("linked.goq-invite");
        let host = SecretKey::from_bytes(&[0x41; 32]);
        let peer = SecretKey::from_bytes(&[0x42; 32]);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let token = test_invitation(
            &host,
            &peer,
            InvitationGrants::VIEW,
            now.saturating_sub(1),
            now + 60,
        );
        write_private_invitation(&path, &token, b"\n");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            load_invitation_file(&path, host.public(), peer.public(), InvitationGrants::VIEW,)
                .is_err()
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&path, &link).unwrap();
        assert!(
            load_invitation_file(&link, host.public(), peer.public(), InvitationGrants::VIEW,)
                .is_err()
        );
        std::fs::write(&path, vec![b'x'; MAX_INVITATION_FILE_BYTES as usize + 1]).unwrap();
        assert!(
            load_invitation_file(&path, host.public(), peer.public(), InvitationGrants::VIEW,)
                .is_err()
        );
    }

    #[test]
    fn invitation_is_attached_only_when_the_caller_selects_the_media_hello() {
        let host = SecretKey::from_bytes(&[0x51; 32]);
        let peer = SecretKey::from_bytes(&[0x52; 32]);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let token = test_invitation(
            &host,
            &peer,
            InvitationGrants::VIEW,
            now.saturating_sub(1),
            now + 60,
        );
        let media = probe_client_hello([7; 16], vec![Capability::VideoH264], Some(&token));
        let input = probe_client_hello([7; 16], vec![Capability::InputAck], None);
        assert_eq!(media.invitation.as_deref(), Some(token.as_str()));
        assert_eq!(input.invitation, None);
    }

    #[test]
    fn media_compatibility_flag_defaults_and_conflicts_are_enforced() {
        let node_id = SecretKey::from_bytes(&[0x2a; 32]).public().to_string();
        let default = Args::try_parse_from(["sigil-probe", "--node-id", &node_id]).unwrap();
        assert!(!default.media_v3);
        assert_eq!(default.identity, None);
        assert_eq!(default.invitation, None);
        assert_eq!(default.expect_size, None);
        assert!(!default.feedback_smoke);
        let strict = Args::try_parse_from([
            "sigil-probe",
            "--node-id",
            &node_id,
            "--expect-size",
            "2560x1600",
        ])
        .unwrap();
        assert_eq!(strict.expect_size, Some((2_560, 1_600)));
        let feedback =
            Args::try_parse_from(["sigil-probe", "--node-id", &node_id, "--feedback-smoke"])
                .unwrap();
        assert!(feedback.feedback_smoke);
        assert!(
            Args::try_parse_from([
                "sigil-probe",
                "--node-id",
                &node_id,
                "--feedback-smoke",
                "--media-v3",
            ])
            .is_err()
        );
    }
}
