use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey, endpoint::presets};
use iroh_moq::{Moq, MoqSession};
use moq_net::{BroadcastConsumer, GroupConsumer, Track, TrackConsumer};
use sigil_protocol::{
    CONTROL_ALPN_V1, Capability, ClientHello, FrameFlags, GAMEPAD_AXIS_MAX, GAMEPAD_AXIS_MIN,
    GAMEPAD_TRIGGER_MAX, GamepadState, INPUT_ALPN_V1, InputEvent, KeyframeRequestReasonV3,
    MAX_MEDIA_GROUP_BYTES_V3, MAX_MEDIA_OBJECT_ID_V3, MEDIA_ALPN_V1, MEDIA_ALPN_V2, MEDIA_ALPN_V3,
    MOQ_VIDEO_H264_TRACK, MediaCodec, MediaControlRequestV3, MediaFrame, MediaObjectV3,
    PointerSurfaceDimensions, ProtocolError, decode_media_frame_object, media_moq_broadcast_name,
    read_host_hello, read_input_ack, read_media_frame, read_media_object, read_media_object_v3,
    write_client_hello, write_input_event, write_media_control_request_v3,
};

const MEDIA_OBJECT_CAPACITY: usize = 4;

#[derive(Debug, Parser)]
#[command(name = "sigil-probe", version, about = "Bounded Sigil transport probe")]
struct Args {
    #[arg(long)]
    node_id: EndpointId,
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
    #[arg(long, conflicts_with_all = ["media_v2", "media_v1"])]
    media_v3: bool,
    /// Exercise independent v2 media objects instead of upstream MoQ.
    /// Intended only for compatibility validation.
    #[arg(long, conflicts_with_all = ["media_v3", "media_v1"])]
    media_v2: bool,
    /// Exercise the reliable ordered v1 media stream instead of upstream MoQ.
    /// Intended only for compatibility validation.
    #[arg(long, conflicts_with_all = ["media_v3", "media_v2"])]
    media_v1: bool,
    /// Request a configured recovery keyframe after three accepted frames,
    /// then prove no delta history is delivered before the recovery barrier.
    #[arg(long)]
    keyframe_smoke: bool,
    /// Correlation identifier for `--keyframe-smoke` host evidence.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..))]
    keyframe_request_id: u64,
    /// Require gamepad negotiation and emit one bounded non-neutral snapshot
    /// followed by neutral. Intended for evtest-backed uinput proof.
    #[arg(long)]
    gamepad_smoke: bool,
    /// Require relative-pointer negotiation and emit bounded motion plus one
    /// complete left-click. Intended for libinput/Gamescope-backed proof.
    #[arg(long)]
    pointer_smoke: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaTransport {
    UpstreamMoq,
    GroupedV3,
    IndependentV2,
    ReliableV1,
}

impl MediaTransport {
    fn alpn(self) -> &'static [u8] {
        match self {
            Self::UpstreamMoq => CONTROL_ALPN_V1,
            Self::GroupedV3 => MEDIA_ALPN_V3,
            Self::IndependentV2 => MEDIA_ALPN_V2,
            Self::ReliableV1 => MEDIA_ALPN_V1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::UpstreamMoq => "iroh-moq",
            Self::GroupedV3 => "grouped-v3",
            Self::IndependentV2 => "independent-v2",
            Self::ReliableV1 => "reliable-v1",
        }
    }

    fn media_alpn_label(self) -> &'static str {
        match self {
            Self::UpstreamMoq => std::str::from_utf8(iroh_moq::ALPN)
                .expect("pinned iroh-moq ALPN must be printable UTF-8"),
            Self::GroupedV3 => "sigil/media/3",
            Self::IndependentV2 => "sigil/media/2",
            Self::ReliableV1 => "sigil/media/1",
        }
    }
}

const KEYFRAME_SMOKE_REQUEST_AFTER_FRAMES: u32 = 3;
const KEYFRAME_SMOKE_MINIMUM_FRAMES: u32 = KEYFRAME_SMOKE_REQUEST_AFTER_FRAMES + 1;

#[derive(Debug)]
enum MediaObjectOutcome {
    Frame { index: u64, frame: MediaFrame },
    Dropped { index: u64 },
    Malformed { index: u64, error: ProtocolError },
}

impl MediaObjectOutcome {
    fn index(&self) -> u64 {
        match self {
            Self::Frame { index, .. } | Self::Dropped { index } | Self::Malformed { index, .. } => {
                *index
            }
        }
    }

    fn is_fast_forward_barrier(&self) -> bool {
        let Self::Frame { frame, .. } = self else {
            return false;
        };
        frame.header.flags.contains(FrameFlags::KEYFRAME)
            && frame.header.flags.contains(FrameFlags::CODEC_CONFIG)
            && frame.header.flags.contains(FrameFlags::DISCONTINUITY)
    }
}

#[derive(Debug)]
struct MediaObjectReorder {
    next_index: u64,
    completed: BTreeMap<u64, MediaObjectOutcome>,
}

impl MediaObjectReorder {
    fn new(first_index: u64) -> Self {
        Self {
            next_index: first_index,
            completed: BTreeMap::new(),
        }
    }

    fn pending_len(&self) -> usize {
        self.completed.len()
    }

    fn push(&mut self, outcome: MediaObjectOutcome) -> Result<Option<MediaObjectOutcome>> {
        if matches!(outcome, MediaObjectOutcome::Malformed { .. }) {
            // Malformed objects remain terminal as soon as their read completes.
            return Ok(Some(outcome));
        }
        let index = outcome.index();
        if index < self.next_index {
            return Ok(Some(outcome));
        }
        if outcome.is_fast_forward_barrier() {
            self.completed
                .retain(|completed_index, _| *completed_index > index);
            self.next_index = index
                .checked_add(1)
                .context("media object reorder overflowed")?;
            return Ok(Some(outcome));
        }
        ensure!(
            self.completed.insert(index, outcome).is_none(),
            "media object {index} completed more than once"
        );
        self.take_next()
    }

    fn take_next(&mut self) -> Result<Option<MediaObjectOutcome>> {
        let Some(outcome) = self.completed.remove(&self.next_index) else {
            return Ok(None);
        };
        self.next_index = self
            .next_index
            .checked_add(1)
            .context("media object reorder overflowed")?;
        Ok(Some(outcome))
    }
}

struct MediaObjectReceiver {
    connection: iroh::endpoint::Connection,
    reads: tokio::task::JoinSet<MediaObjectOutcome>,
    reorder: MediaObjectReorder,
    next_index: u64,
    accepting: bool,
    read_timeout: Duration,
}

impl MediaObjectReceiver {
    fn new(connection: iroh::endpoint::Connection, read_timeout: Duration) -> Self {
        Self {
            connection,
            reads: tokio::task::JoinSet::new(),
            reorder: MediaObjectReorder::new(0),
            next_index: 0,
            accepting: true,
            read_timeout,
        }
    }

    async fn next(&mut self) -> Result<Option<MediaObjectOutcome>> {
        loop {
            if let Some(completed) = self.reorder.take_next()? {
                return Ok(Some(completed));
            }
            if !self.accepting && self.reads.is_empty() {
                ensure!(
                    self.reorder.pending_len() == 0,
                    "media connection closed with an incomplete object order"
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
                            let index = self.next_index;
                            self.next_index = self
                                .next_index
                                .checked_add(1)
                                .context("media object index overflowed")?;
                            let read_timeout = self.read_timeout;
                            self.reads.spawn(async move {
                                match tokio::time::timeout(read_timeout, read_media_object(&mut stream)).await {
                                    Ok(Ok(frame)) => MediaObjectOutcome::Frame { index, frame },
                                    Ok(Err(ProtocolError::Io(_))) => MediaObjectOutcome::Dropped { index },
                                    Ok(Err(error)) => MediaObjectOutcome::Malformed { index, error },
                                    Err(_) => MediaObjectOutcome::Dropped { index },
                                }
                            });
                        }
                        Err(_) => self.accepting = false,
                    }
                }
                completed = self.reads.join_next(), if !self.reads.is_empty() => {
                    match completed.expect("guarded by non-empty media object task set") {
                        Ok(outcome) => {
                            if let Some(completed) = self.reorder.push(outcome)? {
                                return Ok(Some(completed));
                            }
                        }
                        Err(error) if error.is_cancelled() => continue,
                        Err(error) => return Err(error).context("media object read task failed"),
                    }
                }
            }
        }
    }
}

impl Drop for MediaObjectReceiver {
    fn drop(&mut self) {
        self.reads.abort_all();
    }
}

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
enum MediaObjectDecision {
    Deliver,
    DropLate,
    DropUntilKeyframe,
}

#[derive(Debug, Default)]
struct MediaObjectSequence {
    completion_watermark: Option<u64>,
    last_sequence: Option<u64>,
    waiting_for_keyframe: bool,
}

impl MediaObjectSequence {
    fn new() -> Self {
        Self {
            waiting_for_keyframe: true,
            ..Self::default()
        }
    }

    fn note_drop(&mut self, index: u64) -> bool {
        if self
            .completion_watermark
            .is_some_and(|watermark| index <= watermark)
        {
            return false;
        }
        self.completion_watermark = Some(index);
        self.waiting_for_keyframe = true;
        true
    }

    fn classify(&mut self, index: u64, frame: &MediaFrame) -> MediaObjectDecision {
        if self
            .completion_watermark
            .is_some_and(|watermark| index <= watermark)
        {
            return MediaObjectDecision::DropLate;
        }
        self.completion_watermark = Some(index);

        let independently_decodable = frame.header.flags.contains(FrameFlags::KEYFRAME)
            && frame.header.flags.contains(FrameFlags::CODEC_CONFIG);
        let sequence_contiguous = self
            .last_sequence
            .is_none_or(|last| last.checked_add(1) == Some(frame.header.sequence));
        let sequence_monotonic = self
            .last_sequence
            .is_none_or(|last| frame.header.sequence > last);

        let resync_required =
            !sequence_monotonic || self.waiting_for_keyframe || !sequence_contiguous;
        if resync_required && (!independently_decodable || !sequence_monotonic) {
            self.waiting_for_keyframe = true;
            return MediaObjectDecision::DropUntilKeyframe;
        }

        self.last_sequence = Some(frame.header.sequence);
        self.waiting_for_keyframe = false;
        MediaObjectDecision::Deliver
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
        let track = broadcast
            .subscribe_track(&Track::new(MOQ_VIDEO_H264_TRACK))
            .with_context(|| format!("subscribing to MoQ track {MOQ_VIDEO_H264_TRACK}"))?;
        Ok(Self {
            lifetime: MoqProbeLifetime {
                _moq: moq,
                session,
                _broadcast: broadcast,
            },
            track,
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
    payload_len: usize,
    v3_group_id: Option<u64>,
}

impl From<MediaFrame> for AcceptedMedia {
    fn from(frame: MediaFrame) -> Self {
        Self {
            flags: frame.header.flags,
            width: frame.header.width,
            height: frame.header.height,
            sequence: frame.header.sequence,
            payload_len: frame.payload.len(),
            v3_group_id: None,
        }
    }
}

impl From<MediaObjectV3> for AcceptedMedia {
    fn from(object: MediaObjectV3) -> Self {
        Self {
            flags: object.header.flags,
            width: object.header.width,
            height: object.header.height,
            sequence: object.header.sequence,
            payload_len: object.payload.len(),
            v3_group_id: Some(object.header.group_id),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.timeout_seconds > 0,
        "--timeout-seconds must be greater than zero"
    );
    ensure!(
        !args.keyframe_smoke || (!args.media_v1 && !args.media_v2),
        "--keyframe-smoke requires upstream MoQ or grouped v3 media"
    );
    ensure!(
        !args.keyframe_smoke || args.frames >= KEYFRAME_SMOKE_MINIMUM_FRAMES,
        "--keyframe-smoke requires at least {KEYFRAME_SMOKE_MINIMUM_FRAMES} accepted frames"
    );

    let secret = SecretKey::generate();
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).context("generating handshake nonce")?;
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .bind()
        .await
        .context("binding probe endpoint")?;
    let _ = tokio::time::timeout(Duration::from_secs(10), endpoint.online()).await;
    let address = EndpointAddr::new(args.node_id);
    let media_transport = if args.media_v1 {
        MediaTransport::ReliableV1
    } else if args.media_v2 {
        MediaTransport::IndependentV2
    } else if args.media_v3 {
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
    )
    .await?;
    let session_id = media_negotiation.session_id;
    let mut media_send = Some(media_send);
    let mut media_recv = Some(media_recv);
    match media_transport {
        MediaTransport::UpstreamMoq | MediaTransport::GroupedV3 => {
            // Sigil finishes its response half after HostHello. Keep our send
            // half alive as the bounded keyframe-control stream.
            drop(media_recv.take());
        }
        MediaTransport::IndependentV2 => {
            media_send
                .take()
                .expect("media handshake stream is present")
                .finish()
                .context("finishing media v2 handshake request")?;
            drop(media_recv.take());
        }
        MediaTransport::ReliableV1 => {}
    }

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
    )
    .await?;
    ensure!(
        session_id == input_negotiation.session_id,
        "media and input session IDs differ"
    );
    let input_started = Instant::now();
    write_input_event(&mut input_send, &InputEvent::Probe)
        .await
        .context("writing input probe")?;
    read_expected_input_ack(&mut input_recv, args.timeout_seconds, 1).await?;
    let input_ack_micros = input_started.elapsed().as_micros();
    let mut expected_ack = 1_u64;

    if args.pointer_smoke {
        ensure!(
            input_negotiation
                .capabilities
                .contains(&Capability::RelativePointer),
            "host did not accept the required relative pointer capability"
        );
        let [position_sync, relative_motion, click] =
            pointer_smoke_events(media_negotiation.pointer_surface_dimensions)?;
        write_input_event(&mut input_send, &position_sync)
            .await
            .context("writing pointer position synchronization")?;
        expected_ack += 1;
        read_expected_input_ack(&mut input_recv, args.timeout_seconds, expected_ack).await?;
        write_input_event(&mut input_send, &relative_motion)
            .await
            .context("writing relative pointer smoke motion")?;
        expected_ack += 1;
        read_expected_input_ack(&mut input_recv, args.timeout_seconds, expected_ack).await?;
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
    let mut object_receiver = (media_transport == MediaTransport::IndependentV2).then(|| {
        MediaObjectReceiver::new(
            media_connection.clone(),
            Duration::from_secs(args.timeout_seconds),
        )
    });
    let mut object_sequence = MediaObjectSequence::new();
    let mut object_receiver_v3 = (media_transport == MediaTransport::GroupedV3).then(|| {
        MediaObjectReceiverV3::new(
            media_connection.clone(),
            Duration::from_secs(args.timeout_seconds),
        )
    });
    let mut object_sequence_v3 = MediaObjectSequenceV3::new();
    let mut keyframe_request_sent = false;
    let mut keyframe_recovery_verified = false;
    let mut keyframe_request_group_id = None;
    let mut keyframe_request_last_sequence = None;

    while received < args.frames {
        let (frame, recovery_frame): (AcceptedMedia, bool) = match media_transport {
            MediaTransport::UpstreamMoq => loop {
                let outcome = tokio::time::timeout(
                    Duration::from_secs(args.timeout_seconds)
                        .saturating_add(Duration::from_secs(1)),
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
                        let requested_recovery =
                            keyframe_request_sent && !keyframe_recovery_verified;
                        if requested_recovery {
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
                            keyframe_recovery_verified = true;
                        }
                        break (
                            AcceptedMedia {
                                flags: frame.header.flags,
                                width: frame.header.width,
                                height: frame.header.height,
                                sequence: frame.header.sequence,
                                payload_len: frame.payload.len(),
                                v3_group_id: Some(group_sequence),
                            },
                            requested_recovery,
                        );
                    }
                }
            },
            MediaTransport::ReliableV1 => (
                tokio::time::timeout(
                    Duration::from_secs(args.timeout_seconds),
                    read_media_frame(
                        media_recv
                            .as_mut()
                            .expect("v1 media receive stream is present"),
                    ),
                )
                .await
                .context("timed out waiting for media frame")??
                .context("host closed the media stream")?
                .into(),
                false,
            ),
            MediaTransport::IndependentV2 => loop {
                let outcome = tokio::time::timeout(
                    Duration::from_secs(args.timeout_seconds)
                        .saturating_add(Duration::from_secs(1)),
                    object_receiver
                        .as_mut()
                        .expect("v2 media object receiver is present")
                        .next(),
                )
                .await
                .context("timed out waiting for media object")??
                .context("host closed the media object connection")?;
                match outcome {
                    MediaObjectOutcome::Dropped { index } => {
                        if object_sequence.note_drop(index) {
                            media_objects_dropped = media_objects_dropped.saturating_add(1);
                        } else {
                            media_objects_late = media_objects_late.saturating_add(1);
                        }
                    }
                    MediaObjectOutcome::Malformed { index, error } => {
                        bail!("media object {index} is malformed: {error}");
                    }
                    MediaObjectOutcome::Frame { index, frame } => {
                        match object_sequence.classify(index, &frame) {
                            MediaObjectDecision::Deliver => break (frame.into(), false),
                            MediaObjectDecision::DropLate => {
                                media_objects_late = media_objects_late.saturating_add(1);
                            }
                            MediaObjectDecision::DropUntilKeyframe => {
                                media_objects_dropped = media_objects_dropped.saturating_add(1);
                            }
                        }
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
                write_media_control_request_v3(
                    media_send
                        .as_mut()
                        .expect("v3 media control stream is present"),
                    &request,
                ),
            )
            .await
            .context("timed out writing v3 keyframe request")??;
            keyframe_request_group_id = frame.v3_group_id;
            keyframe_request_last_sequence = Some(frame.sequence);
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
                MediaTransport::IndependentV2 | MediaTransport::ReliableV1 => {
                    unreachable!("keyframe smoke excludes v2 and v1")
                }
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
    let diagnostics_connection = moq_receiver
        .as_ref()
        .map_or(&media_connection, MoqProbeReceiver::connection);
    let (path_mode, path_rtt_ms) = selected_path_diagnostics(diagnostics_connection);
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
    if let Some(mut media_send) = media_send {
        media_send
            .finish()
            .context("finishing media request stream")?;
    }
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
        println!("moq_group_capacity=1");
        println!("moq_cancelled_groups={moq_cancelled_groups}");
        println!("moq_group_gaps={moq_group_gaps}");
        println!("moq_unrecovered_group_gaps=0");
        println!("moq_maximum_group_objects={moq_maximum_group_objects}");
        println!("moq_maximum_group_bytes={moq_maximum_group_bytes}");
        println!("moq_historical_suffix_frames={moq_historical_suffix_frames}");
    } else {
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
    } else {
        println!("keyframe_request_id=not-requested");
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
        "gamepad_smoke={}",
        if args.gamepad_smoke {
            "ok"
        } else {
            "not-requested"
        }
    );
    println!("path_mode={path_mode}");
    match path_rtt_ms {
        Some(rtt) => println!("path_rtt_ms={rtt:.3}"),
        None => println!("path_rtt_ms=unknown"),
    }
    println!("encoded_bytes={bytes}");
    println!("elapsed_ms={}", started.elapsed().as_millis());
    Ok(())
}

fn pointer_smoke_events(
    pointer_surface_dimensions: Option<PointerSurfaceDimensions>,
) -> Result<[InputEvent; 3]> {
    let dimensions = pointer_surface_dimensions
        .context("host did not advertise pointer surface dimensions required by --pointer-smoke")?;
    Ok([
        InputEvent::MousePositionSync {
            x: i32::from(dimensions.width / 2),
            y: i32::from(dimensions.height / 2),
        },
        InputEvent::MouseMoveRelative { dx: 32, dy: 16 },
        InputEvent::MouseClick { b: 1 },
    ])
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

async fn negotiate(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    nonce: [u8; 16],
    capabilities: Vec<Capability>,
    required: Capability,
    name: &str,
) -> Result<Negotiated> {
    write_client_hello(
        send,
        &ClientHello::new("sigil-probe/0.1.0", nonce, capabilities),
    )
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

async fn read_expected_input_ack(
    recv: &mut iroh::endpoint::RecvStream,
    timeout_seconds: u64,
    expected_sequence: u64,
) -> Result<()> {
    let input_ack = tokio::time::timeout(
        Duration::from_secs(timeout_seconds.min(5)),
        read_input_ack(recv),
    )
    .await
    .context("timed out waiting for input acknowledgment")??
    .context("host closed before input acknowledgment")?;
    ensure!(
        input_ack.sequence == expected_sequence,
        "unexpected input acknowledgment sequence {}; expected {expected_sequence}",
        input_ack.sequence
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn media_outcome(index: u64, sequence: u64, flags: FrameFlags) -> MediaObjectOutcome {
        MediaObjectOutcome::Frame {
            index,
            frame: media_frame(sequence, flags),
        }
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
    fn media_object_reorder_restores_accept_order_before_sequence_checks() {
        let keyframe = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let mut reorder = MediaObjectReorder::new(0);

        assert!(
            reorder
                .push(media_outcome(1, 11, FrameFlags::NONE))
                .unwrap()
                .is_none()
        );
        assert_eq!(reorder.pending_len(), 1);
        assert_eq!(
            reorder
                .push(media_outcome(0, 10, keyframe))
                .unwrap()
                .unwrap()
                .index(),
            0
        );
        assert_eq!(reorder.take_next().unwrap().unwrap().index(), 1);
        assert_eq!(reorder.pending_len(), 0);
    }

    #[test]
    fn discontinuity_keyframe_is_an_explicit_latest_frame_barrier() {
        let barrier = FrameFlags::KEYFRAME
            .union(FrameFlags::CODEC_CONFIG)
            .union(FrameFlags::DISCONTINUITY);
        let mut reorder = MediaObjectReorder::new(0);

        assert!(
            reorder
                .push(media_outcome(1, 11, FrameFlags::NONE))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            reorder
                .push(media_outcome(2, 20, barrier))
                .unwrap()
                .unwrap()
                .index(),
            2
        );
        assert_eq!(reorder.pending_len(), 0);
        assert_eq!(
            reorder
                .push(media_outcome(0, 10, FrameFlags::NONE))
                .unwrap()
                .unwrap()
                .index(),
            0
        );
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
    fn media_objects_wait_for_a_decodable_keyframe_then_deliver_contiguous_frames() {
        let mut sequence = MediaObjectSequence::new();
        let delta = media_frame(1, FrameFlags::NONE);
        let keyframe = media_frame(2, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        let next_delta = media_frame(3, FrameFlags::NONE);

        assert_eq!(
            sequence.classify(0, &delta),
            MediaObjectDecision::DropUntilKeyframe
        );
        assert_eq!(
            sequence.classify(1, &keyframe),
            MediaObjectDecision::Deliver
        );
        assert_eq!(
            sequence.classify(2, &next_delta),
            MediaObjectDecision::Deliver
        );
    }

    #[test]
    fn dropped_objects_force_resync_and_late_completions_cannot_rewind() {
        let mut sequence = MediaObjectSequence::new();
        let first_keyframe = media_frame(10, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        let stale_delta = media_frame(11, FrameFlags::NONE);
        let replacement_keyframe =
            media_frame(20, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));

        assert_eq!(
            sequence.classify(0, &first_keyframe),
            MediaObjectDecision::Deliver
        );
        assert!(sequence.note_drop(1));
        assert_eq!(
            sequence.classify(2, &replacement_keyframe),
            MediaObjectDecision::Deliver
        );
        assert_eq!(
            sequence.classify(1, &stale_delta),
            MediaObjectDecision::DropLate
        );
        assert!(!sequence.note_drop(1));

        let next_delta = media_frame(21, FrameFlags::NONE);
        assert_eq!(
            sequence.classify(3, &next_delta),
            MediaObjectDecision::Deliver
        );
    }

    #[test]
    fn media_sequence_gap_drops_deltas_until_replacement_keyframe() {
        let mut sequence = MediaObjectSequence::new();
        let keyframe = media_frame(40, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        let gap_delta = media_frame(42, FrameFlags::NONE);
        let replacement_keyframe =
            media_frame(50, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));

        assert_eq!(
            sequence.classify(0, &keyframe),
            MediaObjectDecision::Deliver
        );
        assert_eq!(
            sequence.classify(1, &gap_delta),
            MediaObjectDecision::DropUntilKeyframe
        );
        assert_eq!(
            sequence.classify(2, &replacement_keyframe),
            MediaObjectDecision::Deliver
        );
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

    #[test]
    fn media_compatibility_flags_are_mutually_exclusive() {
        let node_id = SecretKey::from_bytes(&[0x2a; 32]).public().to_string();
        let default = Args::try_parse_from(["sigil-probe", "--node-id", &node_id]).unwrap();
        assert!(!default.media_v1);
        assert!(!default.media_v2);
        assert!(!default.media_v3);
        assert_eq!(default.expect_size, None);
        let strict = Args::try_parse_from([
            "sigil-probe",
            "--node-id",
            &node_id,
            "--expect-size",
            "2560x1600",
        ])
        .unwrap();
        assert_eq!(strict.expect_size, Some((2_560, 1_600)));
        for flags in [
            ["--media-v1", "--media-v2"],
            ["--media-v1", "--media-v3"],
            ["--media-v2", "--media-v3"],
        ] {
            assert!(
                Args::try_parse_from(["sigil-probe", "--node-id", &node_id, flags[0], flags[1],])
                    .is_err()
            );
        }
    }
}
