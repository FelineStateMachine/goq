use super::auth::derive_iroh_secret_from_key;
use super::state::{
    AUDIO_DELIVERY_CAPACITY, AppState, AudioDeliveryState, CLIENT_INPUT_QUEUE_CAPACITY, FRAME_ALPN,
    INPUT_ALPN, development_direct_node_available,
};
use base64::Engine;
use iroh::{Endpoint, SecretKey, endpoint::presets};
use openh264::{formats::YUVSource, nal_units};
use serde::Serialize;
use sigil_protocol::{
    AUDIO_ALPN_V1, AudioFlags, AudioPacket, AudioPacketHeader, Capability, ClientHello, FrameFlags,
    INPUT_ALPN_V1, InputEvent, MAX_MEDIA_PAYLOAD_LEN, MAX_VIDEO_DIMENSION, MAX_VIDEO_PIXELS,
    MEDIA_ALPN_V1, MEDIA_ALPN_V2, MediaCodec, MediaFrame, PointerPosition,
    PointerSurfaceDimensions, ProtocolError, RELATIVE_POINTER_DELTA_MAX,
    RELATIVE_POINTER_DELTA_MIN, read_host_hello, read_input_ack, read_media_frame,
    read_media_object, write_client_hello, write_input_event,
};
use std::collections::{BTreeMap, VecDeque};
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard};
use std::time::{Duration, Instant};
use tauri::{
    AppHandle, Emitter, State,
    ipc::{Channel, Response},
};

// ─── Client commands ──────────────────────────────────────────────────────────

fn byte_to_codec(value: u8) -> &'static str {
    match value {
        1 => "h264",
        2 => "h265",
        3 => "av1",
        _ => "h264",
    }
}

#[derive(Serialize, Clone)]
pub struct FramePayload {
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub data: String,
    pub keyframe: bool,
    pub codec: String,
    pub capture_timestamp_micros: Option<u64>,
    pub pts_micros: Option<i64>,
    pub discontinuity: bool,
}

#[derive(Serialize, Clone)]
struct FrameErrorPayload {
    generation: u64,
    error: String,
}

fn emit_frame_error(app: &AppHandle, generation: u64, error: impl Into<String>) {
    let _ = app.emit(
        "frame-error",
        FrameErrorPayload {
            generation,
            error: error.into(),
        },
    );
}

const LEGACY_MEDIA_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const CLIENT_MEDIA_OBJECT_CAPACITY: usize = 4;
const CLIENT_MEDIA_OBJECT_READ_TIMEOUT: Duration = Duration::from_secs(1);
// Absorb brief webview→Rust acknowledgment jitter without allowing Tauri IPC
// to grow without bound. Four 60 fps frames cap this handoff at about 67 ms;
// WebCodecs has a separate, stricter decode-queue bound in the frontend.
const CLIENT_FRAME_CHANNEL_CAPACITY: usize = 4;
// Three 20 ms Opus packets cap the Rust→webview handoff at 60 ms. The
// AudioWorklet owns a separate fixed ring and never feeds back into transport.
const CLIENT_FRAME_STATS_INTERVAL: Duration = Duration::from_millis(250);
const CLIENT_ENDPOINT_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const CLIENT_FRAME_RATE_WINDOW: Duration = Duration::from_secs(1);
const CLIENT_FRAME_TIMING_WINDOW: Duration = Duration::from_secs(5);
// Host configuration permits at most 240 fps. Leave a little headroom for
// timer-boundary samples while keeping every rate window strictly bounded.
const CLIENT_FRAME_RATE_SAMPLE_CAPACITY: usize = 256;
const CLIENT_FRAME_TIMING_SAMPLE_CAPACITY: usize = 512;
const FRAME_CHANNEL_MAGIC: [u8; 4] = *b"SGFR";
const FRAME_CHANNEL_VERSION: u8 = 1;
const FRAME_CHANNEL_HEADER_LEN: usize = 40;
const FRAME_CHANNEL_FLAG_KEYFRAME: u8 = 1 << 0;
const FRAME_CHANNEL_FLAG_DISCONTINUITY: u8 = 1 << 1;
const FRAME_CHANNEL_OPTIONAL_U64_NONE: u64 = u64::MAX;
const FRAME_CHANNEL_OPTIONAL_I64_NONE: i64 = i64::MIN;
const AUDIO_CHANNEL_MAGIC: [u8; 4] = *b"SGAC";
const AUDIO_CHANNEL_VERSION: u16 = 1;
const AUDIO_CHANNEL_HEADER_LEN: usize = 24;

#[derive(Debug, Default)]
struct RollingFrameRate {
    samples: VecDeque<Duration>,
}

impl RollingFrameRate {
    fn record(&mut self, elapsed: Duration) {
        self.prune(elapsed);
        if self.samples.len() == CLIENT_FRAME_RATE_SAMPLE_CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back(elapsed);
    }

    fn rate(&mut self, elapsed: Duration) -> f64 {
        self.prune(elapsed);
        let observed = elapsed.min(CLIENT_FRAME_RATE_WINDOW).as_secs_f64();
        if observed <= f64::EPSILON {
            return 0.0;
        }
        self.samples.len() as f64 / observed
    }

    fn prune(&mut self, elapsed: Duration) {
        let cutoff = elapsed.saturating_sub(CLIENT_FRAME_RATE_WINDOW);
        while self
            .samples
            .front()
            .is_some_and(|sample| *sample <= cutoff && elapsed >= CLIENT_FRAME_RATE_WINDOW)
        {
            self.samples.pop_front();
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct TimedDurationSample {
    observed_at: Duration,
    duration: Duration,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct DurationWindowSummary {
    sample_count: usize,
    p50_ms: Option<f64>,
    p95_ms: Option<f64>,
    max_ms: Option<f64>,
}

#[derive(Debug, Default)]
struct RollingDurationWindow {
    samples: VecDeque<TimedDurationSample>,
}

impl RollingDurationWindow {
    fn record(&mut self, observed_at: Duration, duration: Duration) {
        self.prune(observed_at);
        if self.samples.len() == CLIENT_FRAME_TIMING_SAMPLE_CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back(TimedDurationSample {
            observed_at,
            duration,
        });
    }

    fn summary(&mut self, elapsed: Duration) -> DurationWindowSummary {
        self.prune(elapsed);
        if self.samples.is_empty() {
            return DurationWindowSummary::default();
        }

        let mut durations = self
            .samples
            .iter()
            .map(|sample| sample.duration)
            .collect::<Vec<_>>();
        durations.sort_unstable();
        DurationWindowSummary {
            sample_count: durations.len(),
            p50_ms: Some(duration_ms(nearest_rank(&durations, 50))),
            p95_ms: Some(duration_ms(nearest_rank(&durations, 95))),
            max_ms: durations.last().copied().map(duration_ms),
        }
    }

    fn prune(&mut self, elapsed: Duration) {
        let cutoff = elapsed.saturating_sub(CLIENT_FRAME_TIMING_WINDOW);
        while self.samples.front().is_some_and(|sample| {
            sample.observed_at <= cutoff && elapsed >= CLIENT_FRAME_TIMING_WINDOW
        }) {
            self.samples.pop_front();
        }
    }
}

fn nearest_rank(values: &[Duration], percentile: usize) -> Duration {
    debug_assert!(!values.is_empty());
    debug_assert!((1..=100).contains(&percentile));
    let rank = values.len().saturating_mul(percentile).div_ceil(100);
    values[rank.saturating_sub(1).min(values.len() - 1)]
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

#[derive(Debug, Default)]
struct ClientMediaMetrics {
    transport_rate: RollingFrameRate,
    frontend_send_rate: RollingFrameRate,
    transport_intervals: RollingDurationWindow,
    frontend_ipc_send_durations: RollingDurationWindow,
    transport_received_total: u64,
    frontend_sent_total: u64,
    sequence_dropped_total: u64,
    transport_object_dropped_total: u64,
    transport_late_object_dropped_total: u64,
    frontend_queue_dropped_total: u64,
    frontend_resync_dropped_total: u64,
    frontend_queue_peak: usize,
    frontend_resync_episode_total: u64,
    frontend_resync_started_at: Option<Duration>,
    frontend_resync_completed_duration: Duration,
    frontend_resync_max_duration: Duration,
    last_transport_received_at: Option<Duration>,
    last_sequence: Option<u64>,
    last_keyframe: bool,
}

impl ClientMediaMetrics {
    fn observe_transport_receive(
        &mut self,
        elapsed: Duration,
        sequence: Option<u64>,
        keyframe: bool,
    ) {
        if let Some(previous) = self.last_transport_received_at {
            let interval = elapsed.saturating_sub(previous);
            // Gamescope capture is damage-driven. A gap longer than the
            // rolling diagnostics window is idle, not one giant hitch; start
            // a fresh cadence anchor while retaining the exact boundary.
            if interval <= CLIENT_FRAME_TIMING_WINDOW {
                self.transport_intervals.record(elapsed, interval);
            }
        }
        self.last_transport_received_at = Some(elapsed);
        self.transport_received_total = self.transport_received_total.saturating_add(1);
        self.transport_rate.record(elapsed);
        self.last_sequence = sequence;
        self.last_keyframe = keyframe;
    }

    fn observe_frontend_send(&mut self, elapsed: Duration) {
        self.frontend_sent_total = self.frontend_sent_total.saturating_add(1);
        self.frontend_send_rate.record(elapsed);
    }

    fn observe_sequence_drop(&mut self, count: u64) {
        self.sequence_dropped_total = self.sequence_dropped_total.saturating_add(count);
    }

    fn observe_transport_object_drop(&mut self, late: bool) {
        self.transport_object_dropped_total = self.transport_object_dropped_total.saturating_add(1);
        if late {
            self.transport_late_object_dropped_total =
                self.transport_late_object_dropped_total.saturating_add(1);
        }
    }

    fn observe_frontend_queue_drop(&mut self) {
        self.frontend_queue_dropped_total = self.frontend_queue_dropped_total.saturating_add(1);
    }

    fn observe_frontend_resync_drop(&mut self) {
        self.frontend_resync_dropped_total = self.frontend_resync_dropped_total.saturating_add(1);
    }

    fn begin_frontend_resync(&mut self, elapsed: Duration) {
        if self.frontend_resync_started_at.is_none() {
            self.frontend_resync_episode_total =
                self.frontend_resync_episode_total.saturating_add(1);
            self.frontend_resync_started_at = Some(elapsed);
        }
    }

    fn finish_frontend_resync(&mut self, elapsed: Duration) {
        let Some(started_at) = self.frontend_resync_started_at.take() else {
            return;
        };
        let duration = elapsed.saturating_sub(started_at);
        self.frontend_resync_completed_duration = self
            .frontend_resync_completed_duration
            .saturating_add(duration);
        self.frontend_resync_max_duration = self.frontend_resync_max_duration.max(duration);
    }

    fn observe_frontend_ipc_send_duration(&mut self, elapsed: Duration, duration: Duration) {
        self.frontend_ipc_send_durations.record(elapsed, duration);
    }

    fn observe_frontend_queue_depth(&mut self, depth: usize) {
        self.frontend_queue_peak = self.frontend_queue_peak.max(depth);
    }

    fn snapshot(
        &mut self,
        elapsed: Duration,
        frontend_queue_depth: usize,
        path_mode: &'static str,
        path_rtt_ms: Option<f64>,
        generation: u64,
    ) -> FrameStatsPayload {
        let transport_receive_fps = self.transport_rate.rate(elapsed);
        let frontend_send_fps = self.frontend_send_rate.rate(elapsed);
        let frontend_dropped_total = self
            .frontend_queue_dropped_total
            .saturating_add(self.frontend_resync_dropped_total);
        let transport_intervals = self.transport_intervals.summary(elapsed);
        let frontend_ipc_send_durations = self.frontend_ipc_send_durations.summary(elapsed);
        let frontend_resync_current_duration = self
            .frontend_resync_started_at
            .map(|started_at| elapsed.saturating_sub(started_at));
        let frontend_resync_duration = self
            .frontend_resync_completed_duration
            .saturating_add(frontend_resync_current_duration.unwrap_or_default());
        let frontend_resync_max_duration = self
            .frontend_resync_max_duration
            .max(frontend_resync_current_duration.unwrap_or_default());
        FrameStatsPayload {
            generation,
            stats_version: 3,
            transport_receive_fps,
            frontend_send_fps,
            transport_received_total: self.transport_received_total,
            frontend_sent_total: self.frontend_sent_total,
            sequence_dropped_total: self.sequence_dropped_total,
            transport_object_dropped_total: self.transport_object_dropped_total,
            transport_late_object_dropped_total: self.transport_late_object_dropped_total,
            frontend_queue_dropped_total: self.frontend_queue_dropped_total,
            frontend_resync_dropped_total: self.frontend_resync_dropped_total,
            frontend_dropped_total,
            frontend_queue_depth,
            frontend_queue_peak: self.frontend_queue_peak,
            frontend_queue_capacity: CLIENT_FRAME_CHANNEL_CAPACITY,
            frontend_resync_episode_total: self.frontend_resync_episode_total,
            frontend_resync_active: self.frontend_resync_started_at.is_some(),
            frontend_resync_duration_ms_total: duration_ms(frontend_resync_duration),
            frontend_resync_duration_ms_current: frontend_resync_current_duration.map(duration_ms),
            frontend_resync_duration_ms_max: duration_ms(frontend_resync_max_duration),
            timing_window_ms: duration_ms(CLIENT_FRAME_TIMING_WINDOW),
            timing_sample_capacity: CLIENT_FRAME_TIMING_SAMPLE_CAPACITY,
            transport_interval_sample_count: transport_intervals.sample_count,
            transport_interval_p50_ms: transport_intervals.p50_ms,
            transport_interval_p95_ms: transport_intervals.p95_ms,
            transport_interval_max_ms: transport_intervals.max_ms,
            frontend_ipc_send_duration_sample_count: frontend_ipc_send_durations.sample_count,
            frontend_ipc_send_duration_p50_ms: frontend_ipc_send_durations.p50_ms,
            frontend_ipc_send_duration_p95_ms: frontend_ipc_send_durations.p95_ms,
            frontend_ipc_send_duration_max_ms: frontend_ipc_send_durations.max_ms,
            path_mode,
            path_rtt_ms,
            sequence: self.last_sequence,
            keyframe: self.last_keyframe,
            // Compatibility aliases for the currently bundled frontend. New
            // diagnostics should consume the explicitly named fields above.
            fps: frontend_send_fps,
            transport_fps: transport_receive_fps,
            frontend_fps: frontend_send_fps,
            count: self.frontend_sent_total,
            host_dropped_frames: self.sequence_dropped_total,
            frontend_dropped_frames: frontend_dropped_total,
        }
    }
}

fn lock_client_media_metrics(
    metrics: &StdMutex<ClientMediaMetrics>,
) -> StdMutexGuard<'_, ClientMediaMetrics> {
    metrics
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn close_generation_connection<T>(
    connection: Option<(u64, T)>,
    close: impl FnOnce(T),
) -> Option<u64> {
    connection.map(|(generation, connection)| {
        close(connection);
        generation
    })
}

#[derive(Clone, Debug, Serialize)]
struct FrameStatsPayload {
    generation: u64,
    stats_version: u8,
    transport_receive_fps: f64,
    frontend_send_fps: f64,
    transport_received_total: u64,
    frontend_sent_total: u64,
    sequence_dropped_total: u64,
    transport_object_dropped_total: u64,
    transport_late_object_dropped_total: u64,
    frontend_queue_dropped_total: u64,
    frontend_resync_dropped_total: u64,
    frontend_dropped_total: u64,
    frontend_queue_depth: usize,
    frontend_queue_peak: usize,
    frontend_queue_capacity: usize,
    frontend_resync_episode_total: u64,
    frontend_resync_active: bool,
    frontend_resync_duration_ms_total: f64,
    frontend_resync_duration_ms_current: Option<f64>,
    frontend_resync_duration_ms_max: f64,
    timing_window_ms: f64,
    timing_sample_capacity: usize,
    transport_interval_sample_count: usize,
    transport_interval_p50_ms: Option<f64>,
    transport_interval_p95_ms: Option<f64>,
    transport_interval_max_ms: Option<f64>,
    frontend_ipc_send_duration_sample_count: usize,
    frontend_ipc_send_duration_p50_ms: Option<f64>,
    frontend_ipc_send_duration_p95_ms: Option<f64>,
    frontend_ipc_send_duration_max_ms: Option<f64>,
    path_mode: &'static str,
    path_rtt_ms: Option<f64>,
    sequence: Option<u64>,
    keyframe: bool,
    fps: f64,
    transport_fps: f64,
    frontend_fps: f64,
    count: u64,
    host_dropped_frames: u64,
    frontend_dropped_frames: u64,
}

struct FrameEnvelopeMetadata<'a> {
    width: u32,
    height: u32,
    codec: &'a str,
    keyframe: bool,
    discontinuity: bool,
    sequence: Option<u64>,
    capture_timestamp_micros: Option<u64>,
    pts_micros: Option<i64>,
}

fn encode_frame_envelope(
    metadata: FrameEnvelopeMetadata<'_>,
    payload: &[u8],
) -> Result<Vec<u8>, String> {
    validate_legacy_media_header(metadata.width, metadata.height, payload.len())?;
    if metadata.sequence == Some(FRAME_CHANNEL_OPTIONAL_U64_NONE) {
        return Err("Frame sequence collides with the channel sentinel".to_string());
    }
    if metadata.capture_timestamp_micros == Some(FRAME_CHANNEL_OPTIONAL_U64_NONE) {
        return Err("Capture timestamp collides with the channel sentinel".to_string());
    }
    if metadata.pts_micros == Some(FRAME_CHANNEL_OPTIONAL_I64_NONE) {
        return Err("Frame PTS collides with the channel sentinel".to_string());
    }
    let width = u16::try_from(metadata.width).map_err(|_| {
        format!(
            "Frame width does not fit channel envelope: {}",
            metadata.width
        )
    })?;
    let height = u16::try_from(metadata.height).map_err(|_| {
        format!(
            "Frame height does not fit channel envelope: {}",
            metadata.height
        )
    })?;
    let payload_len = u32::try_from(payload.len()).map_err(|_| {
        format!(
            "Frame payload does not fit channel envelope: {}",
            payload.len()
        )
    })?;
    let codec = match metadata.codec {
        "h264" => 1,
        "h265" => 2,
        "av1" => 3,
        other => return Err(format!("Unsupported frame channel codec: {other}")),
    };
    let mut flags = 0_u8;
    if metadata.keyframe {
        flags |= FRAME_CHANNEL_FLAG_KEYFRAME;
    }
    if metadata.discontinuity {
        flags |= FRAME_CHANNEL_FLAG_DISCONTINUITY;
    }

    let mut envelope = Vec::with_capacity(FRAME_CHANNEL_HEADER_LEN + payload.len());
    envelope.extend_from_slice(&FRAME_CHANNEL_MAGIC);
    envelope.push(FRAME_CHANNEL_VERSION);
    envelope.push(codec);
    envelope.push(flags);
    envelope.push(0); // Reserved; the parser rejects non-zero values.
    envelope.extend_from_slice(&width.to_be_bytes());
    envelope.extend_from_slice(&height.to_be_bytes());
    envelope.extend_from_slice(&payload_len.to_be_bytes());
    envelope.extend_from_slice(
        &metadata
            .sequence
            .unwrap_or(FRAME_CHANNEL_OPTIONAL_U64_NONE)
            .to_be_bytes(),
    );
    envelope.extend_from_slice(
        &metadata
            .capture_timestamp_micros
            .unwrap_or(FRAME_CHANNEL_OPTIONAL_U64_NONE)
            .to_be_bytes(),
    );
    envelope.extend_from_slice(
        &metadata
            .pts_micros
            .unwrap_or(FRAME_CHANNEL_OPTIONAL_I64_NONE)
            .to_be_bytes(),
    );
    envelope.extend_from_slice(payload);
    debug_assert_eq!(envelope.len(), FRAME_CHANNEL_HEADER_LEN + payload.len());
    Ok(envelope)
}

fn try_reserve_frame_channel_slot(in_flight: &AtomicUsize) -> bool {
    in_flight
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            (current < CLIENT_FRAME_CHANNEL_CAPACITY).then_some(current + 1)
        })
        .is_ok()
}

fn release_frame_channel_slot(in_flight: &AtomicUsize) {
    let _ = in_flight.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
        current.checked_sub(1)
    });
}

fn release_frame_channel_slot_for_generation(
    in_flight: &AtomicUsize,
    current_generation: u64,
    generation: u64,
) -> bool {
    if generation == 0 || current_generation != generation {
        return false;
    }
    release_frame_channel_slot(in_flight);
    true
}

fn lock_audio_deliveries(
    deliveries: &StdMutex<AudioDeliveryState>,
) -> StdMutexGuard<'_, AudioDeliveryState> {
    deliveries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn next_audio_generation(counter: &AtomicU64) -> Result<u64, String> {
    counter
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            current.checked_add(1)
        })
        .map(|previous| previous + 1)
        .map_err(|_| "Audio connection generation overflowed".to_string())
}

fn next_media_generation(counter: &AtomicU64) -> Result<u64, String> {
    counter
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            current.checked_add(1)
        })
        .map(|previous| previous + 1)
        .map_err(|_| "Media connection generation overflowed".to_string())
}

fn audio_generation_is_current(counter: &AtomicU64, generation: u64) -> bool {
    counter.load(Ordering::SeqCst) == generation
}

fn emit_audio_event_if_current(
    app: &AppHandle,
    event: &str,
    generation_counter: &AtomicU64,
    deliveries: &StdMutex<AudioDeliveryState>,
    generation: u64,
    payload: impl FnOnce(&AudioDeliveryState) -> serde_json::Value,
) -> bool {
    let deliveries = lock_audio_deliveries(deliveries);
    if generation_counter.load(Ordering::SeqCst) != generation
        || deliveries.generation() != Some(generation)
    {
        return false;
    }
    let _ = app.emit(event, payload(&deliveries));
    true
}

struct ClientConnectGuard {
    active: Arc<AtomicBool>,
    committed: bool,
}

impl ClientConnectGuard {
    fn acquire(active: Arc<AtomicBool>) -> Result<Self, String> {
        active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| "A client connection is already active or in progress".to_string())?;
        Ok(Self {
            active,
            committed: false,
        })
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for ClientConnectGuard {
    fn drop(&mut self) {
        if !self.committed {
            self.active.store(false, Ordering::SeqCst);
        }
    }
}

#[derive(Serialize)]
pub struct ConnectResult {
    pub connected: bool,
    pub host_node_id: Option<String>,
    pub development_mode: bool,
    pub media_transport: &'static str,
    pub pointer_surface_dimensions: Option<PointerSurfaceDimensions>,
    pub relative_pointer_available: bool,
    pub pointer_position_feedback_available: bool,
    pub absolute_pointer_available: bool,
    pub keyboard_available: bool,
    pub text_available: bool,
    pub gamepad_available: bool,
    pub control_available: bool,
    pub audio_available: bool,
    pub audio_generation: Option<u64>,
    pub audio_error: Option<String>,
    pub media_generation: u64,
    pub error: Option<String>,
}

struct NegotiatedV1Stream {
    session_id: u64,
    capabilities: Vec<Capability>,
    pointer_surface_dimensions: Option<PointerSurfaceDimensions>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaTransport {
    LegacyV0,
    ReliableStreamV1,
    IndependentObjectsV2,
}

impl MediaTransport {
    const fn diagnostic_name(self) -> &'static str {
        match self {
            Self::LegacyV0 => "reliable-v0",
            Self::ReliableStreamV1 => "reliable-v1",
            Self::IndependentObjectsV2 => "independent-v2",
        }
    }
}

#[derive(Debug)]
enum MediaObjectReadOutcome {
    Frame {
        object_index: u64,
        frame: MediaFrame,
    },
    Dropped {
        object_index: u64,
    },
    Malformed(String),
}

impl MediaObjectReadOutcome {
    fn object_index(&self) -> Option<u64> {
        match self {
            Self::Frame { object_index, .. } | Self::Dropped { object_index } => {
                Some(*object_index)
            }
            Self::Malformed(_) => None,
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
    next_object_index: u64,
    completed: BTreeMap<u64, MediaObjectReadOutcome>,
}

impl MediaObjectReorder {
    fn new(first_object_index: u64) -> Self {
        Self {
            next_object_index: first_object_index,
            completed: BTreeMap::new(),
        }
    }

    fn pending_len(&self) -> usize {
        self.completed.len()
    }

    fn push(
        &mut self,
        outcome: MediaObjectReadOutcome,
    ) -> Result<Option<MediaObjectReadOutcome>, String> {
        let Some(object_index) = outcome.object_index() else {
            // Malformed objects remain terminal as soon as their read completes.
            return Ok(Some(outcome));
        };
        if object_index < self.next_object_index {
            return Ok(Some(outcome));
        }
        if outcome.is_fast_forward_barrier() {
            self.completed
                .retain(|completed_index, _| *completed_index > object_index);
            self.next_object_index = object_index
                .checked_add(1)
                .ok_or_else(|| "Media object reorder index overflowed".to_string())?;
            return Ok(Some(outcome));
        }
        if self.completed.insert(object_index, outcome).is_some() {
            return Err(format!(
                "Media object {object_index} completed more than once"
            ));
        }
        self.take_next()
    }

    fn take_next(&mut self) -> Result<Option<MediaObjectReadOutcome>, String> {
        let Some(outcome) = self.completed.remove(&self.next_object_index) else {
            return Ok(None);
        };
        self.next_object_index = self
            .next_object_index
            .checked_add(1)
            .ok_or_else(|| "Media object reorder index overflowed".to_string())?;
        Ok(Some(outcome))
    }
}

struct MediaObjectReceiver {
    connection: iroh::endpoint::Connection,
    reads: tokio::task::JoinSet<MediaObjectReadOutcome>,
    reorder: MediaObjectReorder,
    next_object_index: u64,
    connection_closed: bool,
}

impl MediaObjectReceiver {
    fn new(connection: iroh::endpoint::Connection) -> Self {
        Self {
            connection,
            reads: tokio::task::JoinSet::new(),
            reorder: MediaObjectReorder::new(1),
            next_object_index: 0,
            connection_closed: false,
        }
    }

    async fn next(&mut self) -> Result<Option<MediaObjectReadOutcome>, String> {
        loop {
            if let Some(completed) = self.reorder.take_next()? {
                return Ok(Some(completed));
            }
            if self.connection_closed && self.reads.is_empty() {
                if self.reorder.pending_len() != 0 {
                    return Err("Media connection closed with an incomplete object order".into());
                }
                return Ok(None);
            }

            tokio::select! {
                biased;
                completed = self.reads.join_next(), if !self.reads.is_empty() => {
                    let completed = completed
                        .ok_or_else(|| "Media object reader ended unexpectedly".to_string())?
                        .map_err(|error| format!("Media object reader task failed: {error}"))?;
                    if let Some(completed) = self.reorder.push(completed)? {
                        return Ok(Some(completed));
                    }
                }
                accepted = self.connection.accept_uni(), if !self.connection_closed
                    && self.reads.len() + self.reorder.pending_len()
                        < CLIENT_MEDIA_OBJECT_CAPACITY => {
                    let mut stream = match accepted {
                        Ok(stream) => stream,
                        Err(_) => {
                            self.connection_closed = true;
                            continue;
                        }
                    };
                    self.next_object_index = self.next_object_index.checked_add(1)
                        .ok_or_else(|| "Media object index overflowed".to_string())?;
                    let object_index = self.next_object_index;
                    self.reads.spawn(async move {
                        match tokio::time::timeout(
                            CLIENT_MEDIA_OBJECT_READ_TIMEOUT,
                            read_media_object(&mut stream),
                        )
                        .await
                        {
                            Err(_) => MediaObjectReadOutcome::Dropped { object_index },
                            Ok(Err(ProtocolError::Io(_))) => {
                                MediaObjectReadOutcome::Dropped { object_index }
                            }
                            Ok(Err(error)) => {
                                MediaObjectReadOutcome::Malformed(format!(
                                    "Invalid media object: {error}"
                                ))
                            }
                            Ok(Ok(frame)) => MediaObjectReadOutcome::Frame {
                                object_index,
                                frame,
                            },
                        }
                    });
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaObjectSequenceDecision {
    Deliver { discontinuity: bool },
    DropLate,
    DropUntilKeyframe,
}

#[derive(Debug, Default)]
struct MediaObjectSequence {
    last_sequence: Option<u64>,
    last_object_index: u64,
    waiting_for_keyframe: bool,
}

impl MediaObjectSequence {
    fn new() -> Self {
        Self {
            waiting_for_keyframe: true,
            ..Self::default()
        }
    }

    fn note_dropped_object(&mut self, object_index: u64) -> bool {
        if object_index <= self.last_object_index {
            return false;
        }
        self.waiting_for_keyframe = true;
        true
    }

    fn classify(&mut self, object_index: u64, frame: &MediaFrame) -> MediaObjectSequenceDecision {
        if object_index <= self.last_object_index
            || self
                .last_sequence
                .is_some_and(|sequence| frame.header.sequence <= sequence)
        {
            return MediaObjectSequenceDecision::DropLate;
        }

        let keyframe = frame.header.flags.contains(FrameFlags::KEYFRAME)
            && frame.header.flags.contains(FrameFlags::CODEC_CONFIG);
        let sequence_contiguous = self
            .last_sequence
            .is_none_or(|sequence| sequence.checked_add(1) == Some(frame.header.sequence));
        if !keyframe && (self.waiting_for_keyframe || !sequence_contiguous) {
            self.waiting_for_keyframe = true;
            return MediaObjectSequenceDecision::DropUntilKeyframe;
        }

        let discontinuity = frame.header.flags.contains(FrameFlags::DISCONTINUITY)
            || self.waiting_for_keyframe
            || !sequence_contiguous;
        self.last_sequence = Some(frame.header.sequence);
        self.last_object_index = object_index;
        self.waiting_for_keyframe = false;
        MediaObjectSequenceDecision::Deliver { discontinuity }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InputAvailability {
    relative_pointer: bool,
    pointer_position_feedback: bool,
    absolute_pointer: bool,
    keyboard: bool,
    text: bool,
    gamepad: bool,
    control: bool,
}

impl InputAvailability {
    fn from_capabilities(capabilities: &[Capability]) -> Self {
        let relative_pointer = capabilities.contains(&Capability::RelativePointer);
        let pointer_position_feedback = capabilities.contains(&Capability::PointerPositionFeedback);
        let absolute_pointer = capabilities.contains(&Capability::AbsolutePointer);
        let keyboard = capabilities.contains(&Capability::Keyboard);
        let text = capabilities.contains(&Capability::Text);
        let gamepad = capabilities.contains(&Capability::Gamepad);
        Self {
            relative_pointer,
            pointer_position_feedback,
            absolute_pointer,
            keyboard,
            text,
            gamepad,
            control: relative_pointer || absolute_pointer || keyboard || text || gamepad,
        }
    }
}

#[derive(Serialize, Clone, Copy)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PointerFeedbackPayload {
    Position {
        sequence: u64,
        position: Option<PointerPosition>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pointer_visible: Option<bool>,
    },
    Terminal {
        reason: PointerFeedbackTerminalReason,
    },
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum PointerFeedbackTerminalReason {
    Eof,
    Malformed,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RelativePointerAccumulator {
    dx: i64,
    dy: i64,
}

impl RelativePointerAccumulator {
    fn push(&mut self, dx: i32, dy: i32) {
        self.dx = self.dx.saturating_add(i64::from(dx));
        self.dy = self.dy.saturating_add(i64::from(dy));
    }

    fn take(&mut self) -> Option<InputEvent> {
        if self.dx == 0 && self.dy == 0 {
            return None;
        }
        let dx = self.dx.clamp(
            i64::from(RELATIVE_POINTER_DELTA_MIN),
            i64::from(RELATIVE_POINTER_DELTA_MAX),
        ) as i32;
        let dy = self.dy.clamp(
            i64::from(RELATIVE_POINTER_DELTA_MIN),
            i64::from(RELATIVE_POINTER_DELTA_MAX),
        ) as i32;
        let event = InputEvent::MouseMoveRelative { dx, dy };
        self.dx -= i64::from(dx);
        self.dy -= i64::from(dy);
        Some(event)
    }

    fn is_pending(&self) -> bool {
        self.dx != 0 || self.dy != 0
    }
}

fn stage_relative_input(
    pending: &mut RelativePointerAccumulator,
    event: InputEvent,
) -> Option<InputEvent> {
    match event {
        InputEvent::MouseMoveRelative { dx, dy } => {
            pending.push(dx, dy);
            None
        }
        event => Some(event),
    }
}

async fn negotiate_v1(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    nonce: [u8; 16],
    capabilities: Vec<Capability>,
    required: Option<Capability>,
    stream_name: &str,
) -> Result<NegotiatedV1Stream, String> {
    let hello = ClientHello::new("sigil-spark/0.1.0", nonce, capabilities.clone());
    write_client_hello(send, &hello)
        .await
        .map_err(|e| format!("Failed to send {stream_name} handshake: {e}"))?;
    let response = tokio::time::timeout(Duration::from_secs(10), read_host_hello(recv))
        .await
        .map_err(|_| format!("Timed out waiting for {stream_name} handshake"))?
        .map_err(|e| format!("Invalid {stream_name} handshake: {e}"))?
        .ok_or_else(|| format!("Host closed during {stream_name} handshake"))?;
    if !response.accepted {
        return Err(format!(
            "Host rejected {stream_name} stream: {}",
            response.message.as_deref().unwrap_or("unspecified reason")
        ));
    }
    if let Some(required) = required
        && !response.capabilities.contains(&required)
    {
        return Err(format!(
            "Host accepted {stream_name} without required capability {required:?}"
        ));
    }
    if let Some(unoffered) = response
        .capabilities
        .iter()
        .find(|capability| !capabilities.contains(capability))
    {
        return Err(format!(
            "Host accepted unoffered {stream_name} capability {unoffered:?}"
        ));
    }
    let session_id = response
        .session_id
        .ok_or_else(|| format!("Host omitted {stream_name} session ID"))?;
    Ok(NegotiatedV1Stream {
        session_id,
        capabilities: response.capabilities,
        pointer_surface_dimensions: response.pointer_surface_dimensions,
    })
}

async fn open_negotiated_input_stream(
    endpoint: &Endpoint,
    address: &iroh::EndpointAddr,
    nonce: [u8; 16],
    capabilities: Vec<Capability>,
) -> Result<
    (
        iroh::endpoint::SendStream,
        iroh::endpoint::RecvStream,
        NegotiatedV1Stream,
    ),
    String,
> {
    let connection = endpoint
        .connect(address.clone(), INPUT_ALPN_V1)
        .await
        .map_err(|error| format!("Failed to connect input stream: {error}"))?;
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|error| format!("Failed to open input stream: {error}"))?;
    let negotiation =
        negotiate_v1(&mut send, &mut recv, nonce, capabilities, None, "input").await?;
    Ok((send, recv, negotiation))
}

async fn open_negotiated_media_stream(
    endpoint: &Endpoint,
    address: &iroh::EndpointAddr,
    nonce: [u8; 16],
) -> Result<
    (
        iroh::endpoint::Connection,
        iroh::endpoint::RecvStream,
        NegotiatedV1Stream,
        MediaTransport,
    ),
    String,
> {
    match endpoint.connect(address.clone(), MEDIA_ALPN_V2).await {
        Ok(connection) => {
            let (mut send, mut recv) = connection
                .open_bi()
                .await
                .map_err(|error| format!("Failed to open media v2 handshake: {error}"))?;
            let negotiation = negotiate_v1(
                &mut send,
                &mut recv,
                nonce,
                vec![Capability::VideoH264],
                Some(Capability::VideoH264),
                "media v2",
            )
            .await?;
            send.finish()
                .map_err(|error| format!("Failed to finish media v2 handshake: {error}"))?;
            Ok((
                connection,
                recv,
                negotiation,
                MediaTransport::IndependentObjectsV2,
            ))
        }
        Err(v2_error) => {
            let connection = endpoint
                .connect(address.clone(), MEDIA_ALPN_V1)
                .await
                .map_err(|v1_error| {
                    format!(
                        "Failed to connect media v2 ({v2_error}); v1 compatibility connection also failed ({v1_error})"
                    )
                })?;
            let (mut send, mut recv) = connection
                .open_bi()
                .await
                .map_err(|error| format!("Failed to open media v1 stream: {error}"))?;
            let negotiation = negotiate_v1(
                &mut send,
                &mut recv,
                nonce,
                vec![Capability::VideoH264],
                Some(Capability::VideoH264),
                "media v1",
            )
            .await?;
            send.finish()
                .map_err(|error| format!("Failed to finish media v1 handshake: {error}"))?;
            Ok((
                connection,
                recv,
                negotiation,
                MediaTransport::ReliableStreamV1,
            ))
        }
    }
}

fn input_capability_offers() -> [Vec<Capability>; 4] {
    [
        vec![
            Capability::RelativePointer,
            Capability::PointerPositionFeedback,
            Capability::PointerVisibilityFeedback,
            Capability::AbsolutePointer,
            Capability::Keyboard,
            Capability::Text,
            Capability::Gamepad,
        ],
        vec![
            Capability::RelativePointer,
            Capability::PointerPositionFeedback,
            Capability::AbsolutePointer,
            Capability::Keyboard,
            Capability::Text,
            Capability::Gamepad,
        ],
        vec![
            Capability::RelativePointer,
            Capability::AbsolutePointer,
            Capability::Keyboard,
            Capability::Text,
            Capability::Gamepad,
        ],
        vec![
            Capability::AbsolutePointer,
            Capability::Keyboard,
            Capability::Text,
            Capability::Gamepad,
        ],
    ]
}

fn input_event_allowed(capabilities: &[Capability], event: &InputEvent) -> bool {
    match event {
        InputEvent::Probe => capabilities.contains(&Capability::InputAck),
        InputEvent::MouseMove { .. } => capabilities.contains(&Capability::AbsolutePointer),
        InputEvent::MouseMoveRelative { .. } => capabilities.contains(&Capability::RelativePointer),
        InputEvent::MousePositionSync { .. } => capabilities.contains(&Capability::RelativePointer),
        InputEvent::MouseClick { .. }
        | InputEvent::MouseDown { .. }
        | InputEvent::MouseUp { .. }
        | InputEvent::MouseScroll { .. } => {
            capabilities.contains(&Capability::RelativePointer)
                || capabilities.contains(&Capability::AbsolutePointer)
        }
        InputEvent::KeyDown { .. } | InputEvent::KeyUp { .. } | InputEvent::KeyClick { .. } => {
            capabilities.contains(&Capability::Keyboard)
        }
        InputEvent::Text { .. } => capabilities.contains(&Capability::Text),
        InputEvent::Gamepad { .. } => capabilities.contains(&Capability::Gamepad),
    }
}

async fn write_client_input_event(
    stream: &mut iroh::endpoint::SendStream,
    event: &InputEvent,
    use_v1: bool,
) -> Result<(), String> {
    if use_v1 {
        write_input_event(stream, event)
            .await
            .map_err(|error| error.to_string())
    } else {
        let json = serde_json::to_string(event).map_err(|error| error.to_string())?;
        stream
            .write_all(format!("{json}\n").as_bytes())
            .await
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, Default)]
struct AudioReorderBuffer {
    expected_sequence: Option<u64>,
    packets: BTreeMap<u64, AudioPacket>,
}

#[derive(Debug)]
struct OrderedAudioPacket {
    packet: AudioPacket,
    discontinuity: bool,
}

impl AudioReorderBuffer {
    const CAPACITY: usize = 3;

    fn insert(&mut self, packet: AudioPacket) -> Result<(Vec<OrderedAudioPacket>, u64), String> {
        let sequence = packet.header.sequence;
        let expected = self.expected_sequence.get_or_insert(sequence);
        if sequence < *expected || self.packets.contains_key(&sequence) {
            return Ok((Vec::new(), 0));
        }
        self.packets.insert(sequence, packet);

        let mut dropped = 0_u64;
        let mut discontinuity = false;
        if !self.packets.contains_key(expected) && self.packets.len() >= Self::CAPACITY {
            let next = *self
                .packets
                .first_key_value()
                .expect("capacity check guarantees one packet")
                .0;
            dropped = next.saturating_sub(*expected);
            *expected = next;
            discontinuity = true;
        }

        let mut ordered = Vec::with_capacity(Self::CAPACITY);
        while let Some(packet) = self.packets.remove(expected) {
            ordered.push(OrderedAudioPacket {
                packet,
                discontinuity,
            });
            discontinuity = false;
            *expected = expected
                .checked_add(1)
                .ok_or_else(|| "Audio sequence overflowed".to_string())?;
        }
        Ok((ordered, dropped))
    }
}

fn encode_audio_channel_packet(
    generation: u64,
    delivery_id: u64,
    packet: AudioPacket,
    force_discontinuity: bool,
) -> Result<Vec<u8>, String> {
    let protocol_packet = if force_discontinuity {
        let header = AudioPacketHeader::opus(
            packet.payload.len(),
            packet.header.sequence,
            packet.header.capture_timestamp_us,
            packet.header.pts_us,
            AudioFlags::DISCONTINUITY,
        )
        .map_err(|error| error.to_string())?;
        AudioPacket::new(header, packet.payload).map_err(|error| error.to_string())?
    } else {
        packet
    };
    let datagram = protocol_packet
        .encode_datagram()
        .map_err(|error| error.to_string())?;
    let mut envelope = Vec::with_capacity(AUDIO_CHANNEL_HEADER_LEN + datagram.len());
    envelope.extend_from_slice(&AUDIO_CHANNEL_MAGIC);
    envelope.extend_from_slice(&AUDIO_CHANNEL_VERSION.to_be_bytes());
    envelope.extend_from_slice(&(AUDIO_CHANNEL_HEADER_LEN as u16).to_be_bytes());
    envelope.extend_from_slice(&generation.to_be_bytes());
    envelope.extend_from_slice(&delivery_id.to_be_bytes());
    envelope.extend_from_slice(&datagram);
    debug_assert_eq!(envelope.len(), AUDIO_CHANNEL_HEADER_LEN + datagram.len());
    Ok(envelope)
}

struct AudioStartRequest {
    address: iroh::EndpointAddr,
    handshake_nonce: [u8; 16],
    media_session_id: Option<u64>,
    audio_supported: bool,
    audio_channel: Channel<Response>,
    audio_deliveries: Arc<StdMutex<AudioDeliveryState>>,
    connection_generation: Arc<AtomicU64>,
    generation: u64,
}

async fn try_start_audio(
    app: AppHandle,
    endpoint: &Endpoint,
    request: AudioStartRequest,
) -> Result<iroh::endpoint::Connection, String> {
    if !request.audio_supported {
        return Err("WebCodecs Opus AudioDecoder is unavailable".to_string());
    }
    let media_session_id = request
        .media_session_id
        .ok_or_else(|| "The connected host protocol does not negotiate audio".to_string())?;
    let audio_connection = tokio::time::timeout(
        Duration::from_secs(3),
        endpoint.connect(request.address, AUDIO_ALPN_V1),
    )
    .await
    .map_err(|_| "Timed out connecting optional audio".to_string())?
    .map_err(|error| format!("Audio connection unavailable: {error}"))?;
    let (mut send, mut recv) = audio_connection
        .open_bi()
        .await
        .map_err(|error| format!("Failed to open audio handshake: {error}"))?;
    let negotiation = negotiate_v1(
        &mut send,
        &mut recv,
        request.handshake_nonce,
        vec![Capability::AudioOpus],
        Some(Capability::AudioOpus),
        "audio",
    )
    .await?;
    if negotiation.session_id != media_session_id {
        audio_connection.close(1_u32.into(), b"audio session mismatch");
        return Err("Host returned mismatched media and audio sessions".to_string());
    }
    send.finish()
        .map_err(|error| format!("Failed to finish audio handshake: {error}"))?;

    let task_audio_connection = audio_connection.clone();
    tokio::spawn(async move {
        let audio_channel = request.audio_channel;
        let audio_deliveries = request.audio_deliveries;
        let connection_generation = request.connection_generation;
        let generation = request.generation;
        let mut reorder = AudioReorderBuffer::default();
        let mut transport_received_total = 0_u64;
        let mut sequence_dropped_total = 0_u64;
        let mut frontend_dropped_total = 0_u64;
        let mut frontend_sent_total = 0_u64;
        let mut pending_discontinuity = false;
        let mut last_stats = Instant::now();

        loop {
            let datagram = match task_audio_connection.read_datagram().await {
                Ok(datagram) => datagram,
                Err(error) => {
                    emit_audio_event_if_current(
                        &app,
                        "audio-state",
                        &connection_generation,
                        &audio_deliveries,
                        generation,
                        |_| {
                            serde_json::json!({
                                "generation": generation,
                                "available": false,
                                "error": format!("Audio connection ended: {error}")
                            })
                        },
                    );
                    break;
                }
            };
            if !audio_generation_is_current(&connection_generation, generation) {
                break;
            }
            let packet = match AudioPacket::decode_datagram(&datagram) {
                Ok(packet) => packet,
                Err(error) => {
                    emit_audio_event_if_current(
                        &app,
                        "audio-state",
                        &connection_generation,
                        &audio_deliveries,
                        generation,
                        |_| {
                            serde_json::json!({
                                "generation": generation,
                                "available": false,
                                "error": format!("Invalid audio packet: {error}")
                            })
                        },
                    );
                    break;
                }
            };
            transport_received_total = transport_received_total.saturating_add(1);
            let (packets, dropped) = match reorder.insert(packet) {
                Ok(result) => result,
                Err(error) => {
                    emit_audio_event_if_current(
                        &app,
                        "audio-state",
                        &connection_generation,
                        &audio_deliveries,
                        generation,
                        |_| {
                            serde_json::json!({
                                "generation": generation,
                                "available": false,
                                "error": error
                            })
                        },
                    );
                    break;
                }
            };
            sequence_dropped_total = sequence_dropped_total.saturating_add(dropped);
            if dropped > 0 {
                pending_discontinuity = true;
            }

            for ordered in packets {
                let delivery_id = match lock_audio_deliveries(&audio_deliveries).reserve(generation)
                {
                    Ok(Some(delivery_id)) => delivery_id,
                    Ok(None) => {
                        frontend_dropped_total = frontend_dropped_total.saturating_add(1);
                        pending_discontinuity = true;
                        continue;
                    }
                    Err(_) => return,
                };
                let envelope = match encode_audio_channel_packet(
                    generation,
                    delivery_id,
                    ordered.packet,
                    ordered.discontinuity || pending_discontinuity,
                ) {
                    Ok(envelope) => envelope,
                    Err(error) => {
                        lock_audio_deliveries(&audio_deliveries)
                            .release_failed_delivery(generation, delivery_id);
                        emit_audio_event_if_current(
                            &app,
                            "audio-state",
                            &connection_generation,
                            &audio_deliveries,
                            generation,
                            |_| {
                                serde_json::json!({
                                    "generation": generation,
                                    "available": false,
                                    "error": error
                                })
                            },
                        );
                        return;
                    }
                };
                if !audio_generation_is_current(&connection_generation, generation) {
                    lock_audio_deliveries(&audio_deliveries)
                        .release_failed_delivery(generation, delivery_id);
                    return;
                }
                if audio_channel.send(Response::new(envelope)).is_err() {
                    lock_audio_deliveries(&audio_deliveries)
                        .release_failed_delivery(generation, delivery_id);
                    emit_audio_event_if_current(
                        &app,
                        "audio-state",
                        &connection_generation,
                        &audio_deliveries,
                        generation,
                        |_| {
                            serde_json::json!({
                                "generation": generation,
                                "available": false,
                                "error": "Audio webview channel closed"
                            })
                        },
                    );
                    return;
                }
                frontend_sent_total = frontend_sent_total.saturating_add(1);
                pending_discontinuity = false;
            }

            if last_stats.elapsed() >= Duration::from_millis(250) {
                if !emit_audio_event_if_current(
                    &app,
                    "audio-stats",
                    &connection_generation,
                    &audio_deliveries,
                    generation,
                    |deliveries| {
                        serde_json::json!({
                            "generation": generation,
                            "transport_received_total": transport_received_total,
                            "sequence_dropped_total": sequence_dropped_total,
                            "frontend_dropped_total": frontend_dropped_total,
                            "frontend_sent_total": frontend_sent_total,
                            "frontend_queue_depth": deliveries.depth(generation).unwrap_or(0),
                            "frontend_queue_capacity": AUDIO_DELIVERY_CAPACITY,
                        })
                    },
                ) {
                    return;
                }
                last_stats = Instant::now();
            }
        }
    });
    Ok(audio_connection)
}

#[tauri::command]
pub async fn iroh_client_connect(
    app: AppHandle,
    state: State<'_, AppState>,
    pin: String,
    frame_channel: Channel<Response>,
    audio_channel: Channel<Response>,
    pointer_channel: Channel<PointerFeedbackPayload>,
    audio_supported: bool,
) -> Result<ConnectResult, String> {
    let _connection_serial = state.client_connection_serial.try_lock().map_err(|_| {
        "Another client connection or disconnection operation is in progress".to_string()
    })?;
    let connect_guard = ClientConnectGuard::acquire(Arc::clone(&state.client_connection_active))?;

    let (host_node_id, development_mode) = if let Some(node_id) = state.dev_connect_node_id {
        if !development_direct_node_available() {
            return Err(
                "Development direct-node routing requires a debug build or the explicit demo-direct-node feature"
                    .to_string(),
            );
        }
        let _ = app.emit(
            "dev-connect-routing",
            serde_json::json!({
                "host_node_id": node_id.to_string(),
                "warning": "Passkey identity lookup skipped; this is not client authorization."
            }),
        );
        (node_id, true)
    } else {
        // FIDO2 derivation — 30s timeout so a missing/stuck key surfaces quickly.
        let host_secret = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::task::spawn_blocking(move || derive_iroh_secret_from_key(&pin)),
        )
        .await
        .map_err(|_| "Security key timed out (30s). Make sure your key is connected.".to_string())?
        .map_err(|e| format!("Task failed: {}", e))?
        .map_err(|e| format!("FIDO2 error: {:?}", e))?;

        // Key has been tapped — relay connection is next; update the UI overlay.
        let _ = app.emit("fido-done", ());
        (host_secret.public(), false)
    };

    let client_secret = SecretKey::generate();
    let mut handshake_nonce = [0_u8; 16];
    getrandom::fill(&mut handshake_nonce)
        .map_err(|error| format!("Failed to generate handshake nonce: {error}"))?;
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(client_secret)
        .bind()
        .await
        .map_err(|e| format!("Failed to bind endpoint: {}", e))?;

    let _ = tokio::time::timeout(Duration::from_secs(10), endpoint.online()).await;

    // Use just the node ID — the presets::N0 relay map handles geographic
    // routing and fallback across all N0 relays automatically.
    let addr = iroh::EndpointAddr::new(host_node_id);

    let use_v1 = development_mode;
    let input_alpn = if use_v1 { INPUT_ALPN_V1 } else { INPUT_ALPN };

    let (frame_conn, mut frame_recv, media_negotiation, media_transport) = if use_v1 {
        let (connection, recv, negotiation, transport) =
            open_negotiated_media_stream(&endpoint, &addr, handshake_nonce).await?;
        (connection, recv, Some(negotiation), transport)
    } else {
        let connection = endpoint
            .connect(addr.clone(), FRAME_ALPN)
            .await
            .map_err(|e| format!("Failed to connect frame stream: {e}"))?;
        let (mut send, recv) = connection
            .open_bi()
            .await
            .map_err(|e| format!("Failed to open frame stream: {e}"))?;
        send.write_all(&[1u8])
            .await
            .map_err(|e| format!("Failed to send start: {e}"))?;
        send.finish()
            .map_err(|e| format!("Failed to finish frame start stream: {e}"))?;
        (connection, recv, None, MediaTransport::LegacyV0)
    };
    let frame_connection_for_stats = frame_conn.clone();
    let media_session_id = media_negotiation
        .as_ref()
        .map(|negotiation| negotiation.session_id);
    let pointer_surface_dimensions = media_negotiation
        .as_ref()
        .and_then(|negotiation| negotiation.pointer_surface_dimensions);

    let (input_send, input_recv, input_capabilities) = if use_v1 {
        let [
            visibility_capabilities,
            position_capabilities,
            relative_capabilities,
            legacy_capabilities,
        ] = input_capability_offers();
        let feedback_offer = open_negotiated_input_stream(
            &endpoint,
            &addr,
            handshake_nonce,
            visibility_capabilities,
        )
        .await;
        let (send, recv, input_negotiation) = match feedback_offer {
            Ok(result) => result,
            Err(feedback_error) => {
                // Capability variants are a strict enum on older hosts. First
                // retry without visibility so the immediately preceding host
                // retains compositor-position feedback.
                let position_offer = open_negotiated_input_stream(
                    &endpoint,
                    &addr,
                    handshake_nonce,
                    position_capabilities,
                )
                .await;
                match position_offer {
                    Ok(result) => result,
                    Err(position_error) => {
                        let relative_offer = open_negotiated_input_stream(
                            &endpoint,
                            &addr,
                            handshake_nonce,
                            relative_capabilities,
                        )
                        .await;
                        match relative_offer {
                            Ok(result) => result,
                            Err(relative_error) => {
                                // A pre-relative-pointer host needs the exact
                                // inherited absolute-pointer compatibility leg.
                                open_negotiated_input_stream(
                                    &endpoint,
                                    &addr,
                                    handshake_nonce,
                                    legacy_capabilities,
                                )
                                .await
                                .map_err(|legacy_error| {
                                    format!(
                                        "Pointer-visibility negotiation failed ({feedback_error}); pointer-position fallback failed ({position_error}); relative input fallback failed ({relative_error}); inherited input fallback failed ({legacy_error})"
                                    )
                                })?
                            }
                        }
                    }
                }
            }
        };
        if Some(input_negotiation.session_id) != media_session_id {
            return Err("Host returned mismatched media and input sessions".to_string());
        }
        (send, recv, input_negotiation.capabilities)
    } else {
        let input_conn = endpoint
            .connect(addr.clone(), input_alpn)
            .await
            .map_err(|e| format!("Failed to connect input stream: {}", e))?;
        let (mut send, recv) = input_conn
            .open_bi()
            .await
            .map_err(|e| format!("Failed to open input stream: {}", e))?;
        send.write_all(&[1u8])
            .await
            .map_err(|e| format!("Failed to send input start: {}", e))?;
        // The inherited protocol predates negotiation and supports the current
        // absolute-pointer, keyboard, and text event set.
        (
            send,
            recv,
            vec![
                Capability::AbsolutePointer,
                Capability::Keyboard,
                Capability::Text,
            ],
        )
    };

    let input_availability = InputAvailability::from_capabilities(&input_capabilities);

    if input_availability.pointer_position_feedback {
        let mut input_feedback = input_recv;
        tokio::spawn(async move {
            let terminal_reason = loop {
                let response = match read_input_ack(&mut input_feedback).await {
                    Ok(Some(response)) => response,
                    Ok(None) => break PointerFeedbackTerminalReason::Eof,
                    Err(error) => {
                        eprintln!("[client] invalid pointer-position feedback: {error}");
                        break PointerFeedbackTerminalReason::Malformed;
                    }
                };
                if pointer_channel
                    .send(PointerFeedbackPayload::Position {
                        sequence: response.sequence,
                        position: response.pointer_position,
                        pointer_visible: response.pointer_visible,
                    })
                    .is_err()
                {
                    return;
                }
            };
            // The session-owned channel emits at most one terminal message.
            // JavaScript rejects deliveries from superseded channel closures.
            let _ = pointer_channel.send(PointerFeedbackPayload::Terminal {
                reason: terminal_reason,
            });
        });
    } else {
        drop(input_recv);
    }

    let audio_generation = next_audio_generation(&state.audio_connection_generation)?;
    lock_audio_deliveries(&state.audio_deliveries).begin_generation(audio_generation)?;
    let audio_result = try_start_audio(
        app.clone(),
        &endpoint,
        AudioStartRequest {
            address: addr,
            handshake_nonce,
            media_session_id,
            audio_supported,
            audio_channel,
            audio_deliveries: Arc::clone(&state.audio_deliveries),
            connection_generation: Arc::clone(&state.audio_connection_generation),
            generation: audio_generation,
        },
    )
    .await;
    let (audio_available, connected_audio_generation, audio_error) = match audio_result {
        Ok(connection) => {
            *state.audio_connection.lock().await = Some((audio_generation, connection));
            (true, Some(audio_generation), None)
        }
        Err(error) => {
            lock_audio_deliveries(&state.audio_deliveries).cancel_generation(audio_generation);
            (false, None, Some(error))
        }
    };
    let media_generation = next_media_generation(&state.client_media_generation)?;

    let (tx, rx) = tokio::sync::mpsc::channel::<InputEvent>(CLIENT_INPUT_QUEUE_CAPACITY);
    {
        let mut input_send_guard = state.input_send.lock().await;
        *input_send_guard = Some(tx);
    }

    {
        let mut ce = state.client_endpoint.lock().await;
        *ce = Some(endpoint.clone());
    }
    *state.media_connection.lock().await = Some((media_generation, frame_conn.clone()));
    let frame_events_in_flight = Arc::new(AtomicUsize::new(0));
    *state.frame_delivery.lock().await =
        Some((media_generation, Arc::clone(&frame_events_in_flight)));

    // Input forwarder: absolute motion is latest-value state and may be
    // dropped at the 60 Hz boundary. Relative motion is displacement, so it
    // owns a separate accumulator and timer that coalesces rather than drops.
    let mut input_stream = input_send;
    tokio::spawn(async move {
        let mut rx = rx;
        const MOUSE_INTERVAL: Duration = Duration::from_millis(16);
        let started = Instant::now();
        let mut last_absolute_mouse_time = started.checked_sub(MOUSE_INTERVAL).unwrap_or(started);
        let mut last_relative_mouse_time = started.checked_sub(MOUSE_INTERVAL).unwrap_or(started);
        let mut pending_relative = RelativePointerAccumulator::default();
        let mut input_open = true;

        while input_open {
            let event = if pending_relative.is_pending() {
                let wait = MOUSE_INTERVAL.saturating_sub(last_relative_mouse_time.elapsed());
                if wait.is_zero() {
                    let Some(event) = pending_relative.take() else {
                        continue;
                    };
                    if let Err(error) =
                        write_client_input_event(&mut input_stream, &event, use_v1).await
                    {
                        eprintln!("[client] input stream write failed: {error}; disconnecting");
                        break;
                    }
                    last_relative_mouse_time = Instant::now();
                    continue;
                }
                tokio::select! {
                    event = rx.recv() => event,
                    () = tokio::time::sleep(wait) => {
                        let Some(event) = pending_relative.take() else {
                            continue;
                        };
                        if let Err(error) = write_client_input_event(&mut input_stream, &event, use_v1).await {
                            eprintln!("[client] input stream write failed: {error}; disconnecting");
                            break;
                        }
                        last_relative_mouse_time = Instant::now();
                        continue;
                    }
                }
            } else {
                rx.recv().await
            };
            let Some(event) = event else {
                input_open = false;
                continue;
            };
            // The host's accepted capability set is an authorization boundary.
            // Drop unavailable event classes silently so event contents never
            // reach logs or the wire even if a compromised webview invokes the
            // command directly.
            if !input_event_allowed(&input_capabilities, &event) {
                continue;
            }
            let Some(event) = stage_relative_input(&mut pending_relative, event) else {
                continue;
            };
            let mut flushed_relative_barrier = false;
            while let Some(relative_barrier) = pending_relative.take() {
                if let Err(error) =
                    write_client_input_event(&mut input_stream, &relative_barrier, use_v1).await
                {
                    eprintln!("[client] input stream write failed: {error}; disconnecting");
                    input_open = false;
                    break;
                }
                flushed_relative_barrier = true;
            }
            if !input_open {
                break;
            }
            if flushed_relative_barrier {
                last_relative_mouse_time = Instant::now();
            }
            if matches!(event, InputEvent::MouseMove { .. }) {
                let now = Instant::now();
                if now.duration_since(last_absolute_mouse_time) < MOUSE_INTERVAL {
                    continue;
                }
                last_absolute_mouse_time = now;
            }
            if let Err(error) = write_client_input_event(&mut input_stream, &event, use_v1).await {
                eprintln!("[client] input stream write failed: {error}; disconnecting");
                break;
            }
        }
        while let Some(event) = pending_relative.take() {
            if let Err(error) = write_client_input_event(&mut input_stream, &event, use_v1).await {
                eprintln!("[client] final relative input write failed: {error}");
                break;
            }
        }
        let _ = input_stream.finish();
    });

    // Frame reader — dual path: WebCodecs (raw bytes) or software JPEG decode
    let use_webcodecs = state.webcodecs.load(Ordering::SeqCst);
    tokio::spawn(async move {
        let metrics_started = Instant::now();
        let mut initial_metrics = ClientMediaMetrics::default();
        // Joining a running encoder commonly starts in the middle of a GOP.
        // The initial keyframe wait is a real resync episode, just like a wait
        // entered after frontend backpressure, so account for it from t=0.
        initial_metrics.begin_frontend_resync(Duration::ZERO);
        let metrics = Arc::new(StdMutex::new(initial_metrics));
        let mut previous_sequence: Option<u64> = None;
        let mut frontend_waiting_for_keyframe = true;
        let mut media_objects = (media_transport == MediaTransport::IndependentObjectsV2)
            .then(|| MediaObjectReceiver::new(frame_connection_for_stats.clone()));
        let mut media_object_sequence = MediaObjectSequence::new();

        let mut decoder = if use_webcodecs {
            None
        } else {
            match openh264::decoder::Decoder::new() {
                Ok(d) => Some(d),
                Err(e) => {
                    emit_frame_error(&app, media_generation, format!("Decoder init failed: {e}"));
                    return;
                }
            }
        };

        let (stats_stop, mut stats_stop_rx) = tokio::sync::watch::channel(false);
        let stats_app = app.clone();
        let stats_metrics = Arc::clone(&metrics);
        let stats_in_flight = Arc::clone(&frame_events_in_flight);
        let stats_connection = frame_connection_for_stats.clone();
        let stats_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(CLIENT_FRAME_STATS_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Tokio intervals tick immediately once. Consume that tick so the
            // first payload represents a full diagnostics interval.
            interval.tick().await;
            let mut path_mode = "unknown";
            let mut path_rtt_ms = None;
            let mut last_path_sample = Instant::now() - Duration::from_secs(1);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if last_path_sample.elapsed() >= Duration::from_secs(1) {
                            (path_mode, path_rtt_ms) = selected_path_diagnostics(&stats_connection);
                            last_path_sample = Instant::now();
                        }
                        let queue_depth = stats_in_flight.load(Ordering::SeqCst);
                        let payload = lock_client_media_metrics(&stats_metrics).snapshot(
                            metrics_started.elapsed(),
                            queue_depth,
                            path_mode,
                            path_rtt_ms,
                            media_generation,
                        );
                        let _ = stats_app.emit("frame-stats", payload);
                    }
                    changed = stats_stop_rx.changed() => {
                        if changed.is_err() || *stats_stop_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });

        'frames: loop {
            let (
                w,
                h,
                frame_buf,
                is_keyframe,
                codec,
                sequence,
                capture_timestamp_micros,
                pts_micros,
                discontinuity,
            ) = match media_transport {
                MediaTransport::IndependentObjectsV2 => {
                    let receiver = media_objects
                        .as_mut()
                        .expect("media v2 receiver must exist for media v2 transport");
                    loop {
                        let outcome = match receiver.next().await {
                            Ok(Some(outcome)) => outcome,
                            Ok(None) => {
                                emit_frame_error(&app, media_generation, "Connection closed");
                                break 'frames;
                            }
                            Err(error) => {
                                emit_frame_error(&app, media_generation, error);
                                break 'frames;
                            }
                        };
                        match outcome {
                            MediaObjectReadOutcome::Dropped { object_index } => {
                                let begins_resync =
                                    media_object_sequence.note_dropped_object(object_index);
                                let mut metrics = lock_client_media_metrics(&metrics);
                                metrics.observe_transport_object_drop(!begins_resync);
                                if begins_resync {
                                    frontend_waiting_for_keyframe = true;
                                    metrics.begin_frontend_resync(metrics_started.elapsed());
                                }
                            }
                            MediaObjectReadOutcome::Malformed(error) => {
                                emit_frame_error(&app, media_generation, error);
                                break 'frames;
                            }
                            MediaObjectReadOutcome::Frame {
                                object_index,
                                frame,
                            } => {
                                let discontinuity = match media_object_sequence
                                    .classify(object_index, &frame)
                                {
                                    MediaObjectSequenceDecision::Deliver { discontinuity } => {
                                        discontinuity
                                    }
                                    MediaObjectSequenceDecision::DropLate => {
                                        lock_client_media_metrics(&metrics)
                                            .observe_transport_object_drop(true);
                                        continue;
                                    }
                                    MediaObjectSequenceDecision::DropUntilKeyframe => {
                                        frontend_waiting_for_keyframe = true;
                                        let mut metrics = lock_client_media_metrics(&metrics);
                                        metrics.observe_transport_object_drop(false);
                                        metrics.begin_frontend_resync(metrics_started.elapsed());
                                        continue;
                                    }
                                };
                                let codec = match frame.header.codec {
                                    MediaCodec::H264 => "h264".to_string(),
                                };
                                break (
                                    u32::from(frame.header.width),
                                    u32::from(frame.header.height),
                                    frame.payload,
                                    frame.header.flags.contains(FrameFlags::KEYFRAME),
                                    codec,
                                    Some(frame.header.sequence),
                                    Some(frame.header.capture_timestamp_us),
                                    Some(frame.header.pts_us),
                                    discontinuity,
                                );
                            }
                        }
                    }
                }
                MediaTransport::ReliableStreamV1 => {
                    // Gamescope's PipeWire stream is damage-driven: a static
                    // screen can legitimately produce no encoded frame for an
                    // arbitrary period. Connection closure and parser/source
                    // errors remain terminal; frame silence does not.
                    let frame = match read_media_frame(&mut frame_recv).await {
                        Ok(Some(frame)) => frame,
                        Ok(None) => {
                            emit_frame_error(&app, media_generation, "Connection closed");
                            break;
                        }
                        Err(error) => {
                            emit_frame_error(
                                &app,
                                media_generation,
                                format!("Invalid media stream: {error}"),
                            );
                            break;
                        }
                    };
                    let codec = match frame.header.codec {
                        MediaCodec::H264 => "h264".to_string(),
                    };
                    (
                        u32::from(frame.header.width),
                        u32::from(frame.header.height),
                        frame.payload,
                        frame.header.flags.contains(FrameFlags::KEYFRAME),
                        codec,
                        Some(frame.header.sequence),
                        Some(frame.header.capture_timestamp_us),
                        Some(frame.header.pts_us),
                        frame.header.flags.contains(FrameFlags::DISCONTINUITY),
                    )
                }
                MediaTransport::LegacyV0 => {
                    let mut header = [0u8; 14];
                    match tokio::time::timeout(
                        LEGACY_MEDIA_IDLE_TIMEOUT,
                        frame_recv.read_exact(&mut header),
                    )
                    .await
                    {
                        Err(_) => {
                            emit_frame_error(&app, media_generation, "Media stream idle timeout");
                            break;
                        }
                        Ok(Err(_)) => {
                            emit_frame_error(&app, media_generation, "Connection lost");
                            break;
                        }
                        Ok(Ok(_)) => {}
                    }
                    let w = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
                    let h = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
                    let frame_len =
                        u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;
                    if let Err(error) = validate_legacy_media_header(w, h, frame_len) {
                        emit_frame_error(&app, media_generation, error);
                        break;
                    }

                    let is_keyframe = header[12] == 1;
                    let codec = byte_to_codec(header[13]).to_string();

                    let mut frame_buf = vec![0u8; frame_len];
                    match tokio::time::timeout(
                        LEGACY_MEDIA_IDLE_TIMEOUT,
                        frame_recv.read_exact(&mut frame_buf),
                    )
                    .await
                    {
                        Err(_) => {
                            emit_frame_error(&app, media_generation, "Media payload idle timeout");
                            break;
                        }
                        Ok(Err(_)) => {
                            emit_frame_error(&app, media_generation, "Connection lost");
                            break;
                        }
                        Ok(Ok(_)) => {}
                    }
                    (w, h, frame_buf, is_keyframe, codec, None, None, None, false)
                }
            };
            lock_client_media_metrics(&metrics).observe_transport_receive(
                metrics_started.elapsed(),
                sequence,
                is_keyframe,
            );

            let sequence_gap = match sequence.zip(previous_sequence) {
                Some((current, previous)) => match sequence_gap(previous, current) {
                    Ok(gap) => gap,
                    Err(error) => {
                        emit_frame_error(&app, media_generation, error);
                        break;
                    }
                },
                None => 0,
            };
            if sequence.is_some() {
                previous_sequence = sequence;
            }
            lock_client_media_metrics(&metrics).observe_sequence_drop(sequence_gap);

            if frontend_waiting_for_keyframe && !is_keyframe {
                lock_client_media_metrics(&metrics).observe_frontend_resync_drop();
                continue;
            }
            if !try_reserve_frame_channel_slot(&frame_events_in_flight) {
                frontend_waiting_for_keyframe = true;
                let mut metrics = lock_client_media_metrics(&metrics);
                metrics.observe_frontend_queue_drop();
                metrics.begin_frontend_resync(metrics_started.elapsed());
                continue;
            }
            lock_client_media_metrics(&metrics)
                .observe_frontend_queue_depth(frame_events_in_flight.load(Ordering::SeqCst));

            let delivered_to_frontend;
            if use_webcodecs {
                let envelope = match encode_frame_envelope(
                    FrameEnvelopeMetadata {
                        width: w,
                        height: h,
                        codec: &codec,
                        keyframe: is_keyframe,
                        discontinuity,
                        sequence,
                        capture_timestamp_micros,
                        pts_micros,
                    },
                    &frame_buf,
                ) {
                    Ok(envelope) => envelope,
                    Err(error) => {
                        emit_frame_error(&app, media_generation, error);
                        release_frame_channel_slot(&frame_events_in_flight);
                        break;
                    }
                };
                let ipc_send_started = Instant::now();
                let send_result = frame_channel.send(Response::new(envelope));
                lock_client_media_metrics(&metrics).observe_frontend_ipc_send_duration(
                    metrics_started.elapsed(),
                    ipc_send_started.elapsed(),
                );
                if send_result.is_err() {
                    release_frame_channel_slot(&frame_events_in_flight);
                    break;
                }
                delivered_to_frontend = true;
            } else if let Some(ref mut dec) = decoder {
                let mut emitted = false;
                for nal in nal_units(&frame_buf) {
                    if let Ok(Some(yuv)) = dec.decode(nal) {
                        let (yw, yh) = yuv.dimensions();
                        let rgb_len = yuv.rgb8_len();
                        let mut rgb_raw = vec![0u8; rgb_len];
                        yuv.write_rgb8(&mut rgb_raw);

                        let img = match image::RgbImage::from_raw(yw as u32, yh as u32, rgb_raw) {
                            Some(img) => img,
                            None => continue,
                        };
                        let mut jpeg_buf = Vec::with_capacity(30_000);
                        if image::DynamicImage::ImageRgb8(img)
                            .write_to(&mut Cursor::new(&mut jpeg_buf), image::ImageFormat::Jpeg)
                            .is_err()
                        {
                            continue;
                        }

                        let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg_buf);
                        let ipc_send_started = Instant::now();
                        let send_result = app.emit(
                            "frame",
                            FramePayload {
                                generation: media_generation,
                                width: yw as u32,
                                height: yh as u32,
                                data: b64,
                                keyframe: is_keyframe,
                                codec: codec.clone(),
                                capture_timestamp_micros,
                                pts_micros,
                                discontinuity,
                            },
                        );
                        lock_client_media_metrics(&metrics).observe_frontend_ipc_send_duration(
                            metrics_started.elapsed(),
                            ipc_send_started.elapsed(),
                        );
                        if send_result.is_err() {
                            release_frame_channel_slot(&frame_events_in_flight);
                            break 'frames;
                        }
                        emitted = true;
                        break;
                    }
                }
                if !emitted {
                    release_frame_channel_slot(&frame_events_in_flight);
                }
                delivered_to_frontend = emitted;
            } else {
                release_frame_channel_slot(&frame_events_in_flight);
                delivered_to_frontend = false;
            }

            if delivered_to_frontend {
                frontend_waiting_for_keyframe = false;
                let mut metrics = lock_client_media_metrics(&metrics);
                let elapsed = metrics_started.elapsed();
                metrics.observe_frontend_send(elapsed);
                metrics.finish_frontend_resync(elapsed);
            }

            tokio::task::yield_now().await;
        }

        let _ = stats_stop.send(true);
        let _ = stats_task.await;
        drop(endpoint);
    });

    let result = ConnectResult {
        connected: true,
        host_node_id: Some(host_node_id.to_string()),
        development_mode,
        media_transport: media_transport.diagnostic_name(),
        pointer_surface_dimensions,
        relative_pointer_available: input_availability.relative_pointer,
        pointer_position_feedback_available: input_availability.pointer_position_feedback,
        absolute_pointer_available: input_availability.absolute_pointer,
        keyboard_available: input_availability.keyboard,
        text_available: input_availability.text,
        gamepad_available: input_availability.gamepad,
        control_available: input_availability.control,
        audio_available,
        audio_generation: connected_audio_generation,
        audio_error,
        media_generation,
        error: None,
    };
    connect_guard.commit();
    Ok(result)
}

fn validate_legacy_media_header(width: u32, height: u32, payload_len: usize) -> Result<(), String> {
    if width == 0
        || height == 0
        || width > u32::from(MAX_VIDEO_DIMENSION)
        || height > u32::from(MAX_VIDEO_DIMENSION)
        || width.saturating_mul(height) > MAX_VIDEO_PIXELS
    {
        return Err(format!("Invalid legacy media dimensions: {width}x{height}"));
    }
    if payload_len == 0 || payload_len > MAX_MEDIA_PAYLOAD_LEN {
        return Err(format!(
            "Invalid legacy media payload length: {payload_len}"
        ));
    }
    Ok(())
}

fn sequence_gap(previous: u64, current: u64) -> Result<u64, String> {
    let expected = previous
        .checked_add(1)
        .ok_or_else(|| format!("Media sequence overflowed after {previous}"))?;
    if current < expected {
        return Err(format!(
            "Non-monotonic media sequence: previous={previous}, current={current}"
        ));
    }
    Ok(current - expected)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn media_object_frame(sequence: u64, flags: FrameFlags) -> MediaFrame {
        let payload = vec![sequence as u8];
        let header = sigil_protocol::MediaFrameHeader::h264(
            1280,
            800,
            payload.len(),
            sequence,
            sequence * 1_000,
            sequence as i64 * 1_000,
            flags,
        )
        .unwrap();
        MediaFrame::new(header, payload).unwrap()
    }

    fn media_object_outcome(
        object_index: u64,
        sequence: u64,
        flags: FrameFlags,
    ) -> MediaObjectReadOutcome {
        MediaObjectReadOutcome::Frame {
            object_index,
            frame: media_object_frame(sequence, flags),
        }
    }

    fn completed_object_index(outcome: &MediaObjectReadOutcome) -> Option<u64> {
        outcome.object_index()
    }

    #[test]
    fn media_object_reorder_restores_accept_order_without_false_resync() {
        let keyframe = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let mut reorder = MediaObjectReorder::new(1);

        assert!(
            reorder
                .push(media_object_outcome(2, 11, FrameFlags::NONE))
                .unwrap()
                .is_none()
        );
        assert_eq!(reorder.pending_len(), 1);
        let first = reorder
            .push(media_object_outcome(1, 10, keyframe))
            .unwrap()
            .unwrap();
        assert_eq!(completed_object_index(&first), Some(1));
        assert_eq!(
            completed_object_index(&reorder.take_next().unwrap().unwrap()),
            Some(2)
        );
        assert_eq!(reorder.pending_len(), 0);
    }

    #[test]
    fn explicit_discontinuity_keyframe_fast_forwards_bounded_reorder() {
        let barrier = FrameFlags::KEYFRAME
            .union(FrameFlags::CODEC_CONFIG)
            .union(FrameFlags::DISCONTINUITY);
        let mut reorder = MediaObjectReorder::new(1);

        assert!(
            reorder
                .push(media_object_outcome(2, 11, FrameFlags::NONE))
                .unwrap()
                .is_none()
        );
        let recovered = reorder
            .push(media_object_outcome(3, 20, barrier))
            .unwrap()
            .unwrap();
        assert_eq!(completed_object_index(&recovered), Some(3));
        assert_eq!(reorder.pending_len(), 0);
        assert_eq!(
            completed_object_index(
                &reorder
                    .push(media_object_outcome(1, 10, FrameFlags::NONE))
                    .unwrap()
                    .unwrap()
            ),
            Some(1)
        );
    }

    #[test]
    fn malformed_media_object_bypasses_reorder_and_remains_terminal() {
        let mut reorder = MediaObjectReorder::new(1);
        assert!(
            reorder
                .push(media_object_outcome(2, 11, FrameFlags::NONE))
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            reorder
                .push(MediaObjectReadOutcome::Malformed("bad object".into()))
                .unwrap(),
            Some(MediaObjectReadOutcome::Malformed(_))
        ));
        assert_eq!(reorder.pending_len(), 1);
    }

    #[test]
    fn media_objects_begin_on_a_configured_keyframe_then_deliver_contiguously() {
        let keyframe_flags = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let mut sequence = MediaObjectSequence::new();

        assert_eq!(
            sequence.classify(1, &media_object_frame(1, FrameFlags::NONE)),
            MediaObjectSequenceDecision::DropUntilKeyframe
        );
        assert_eq!(
            sequence.classify(2, &media_object_frame(2, keyframe_flags)),
            MediaObjectSequenceDecision::Deliver {
                discontinuity: true
            }
        );
        assert_eq!(
            sequence.classify(3, &media_object_frame(3, FrameFlags::NONE)),
            MediaObjectSequenceDecision::Deliver {
                discontinuity: false
            }
        );
    }

    #[test]
    fn media_object_sequence_gaps_drop_deltas_until_a_new_keyframe() {
        let keyframe_flags = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let mut sequence = MediaObjectSequence::new();

        assert!(matches!(
            sequence.classify(1, &media_object_frame(10, keyframe_flags)),
            MediaObjectSequenceDecision::Deliver { .. }
        ));
        assert_eq!(
            sequence.classify(3, &media_object_frame(12, FrameFlags::NONE)),
            MediaObjectSequenceDecision::DropUntilKeyframe
        );
        assert_eq!(
            sequence.classify(4, &media_object_frame(13, FrameFlags::NONE)),
            MediaObjectSequenceDecision::DropUntilKeyframe
        );
        assert_eq!(
            sequence.classify(5, &media_object_frame(14, keyframe_flags)),
            MediaObjectSequenceDecision::Deliver {
                discontinuity: true
            }
        );
    }

    #[test]
    fn late_media_object_completion_cannot_rewind_a_recovered_stream() {
        let keyframe_flags = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let mut sequence = MediaObjectSequence::new();

        assert!(matches!(
            sequence.classify(10, &media_object_frame(10, keyframe_flags)),
            MediaObjectSequenceDecision::Deliver { .. }
        ));
        assert!(sequence.note_dropped_object(12));
        assert_eq!(
            sequence.classify(13, &media_object_frame(13, keyframe_flags)),
            MediaObjectSequenceDecision::Deliver {
                discontinuity: true
            }
        );
        assert!(!sequence.note_dropped_object(11));
        assert_eq!(
            sequence.classify(12, &media_object_frame(12, FrameFlags::NONE)),
            MediaObjectSequenceDecision::DropLate
        );
        assert_eq!(
            sequence.classify(14, &media_object_frame(14, FrameFlags::NONE)),
            MediaObjectSequenceDecision::Deliver {
                discontinuity: false
            }
        );
    }

    #[test]
    fn binary_frame_envelope_has_exact_stable_layout() {
        let payload = [0, 0, 0, 1, 0x65];
        let envelope = encode_frame_envelope(
            FrameEnvelopeMetadata {
                width: 1280,
                height: 800,
                codec: "h264",
                keyframe: true,
                discontinuity: true,
                sequence: Some(42),
                capture_timestamp_micros: Some(123_456),
                pts_micros: Some(98_765),
            },
            &payload,
        )
        .unwrap();

        assert_eq!(envelope.len(), FRAME_CHANNEL_HEADER_LEN + payload.len());
        assert_eq!(&envelope[0..4], b"SGFR");
        assert_eq!(envelope[4], 1);
        assert_eq!(envelope[5], 1);
        assert_eq!(envelope[6], 0b11);
        assert_eq!(envelope[7], 0);
        assert_eq!(&envelope[8..10], &1280_u16.to_be_bytes());
        assert_eq!(&envelope[10..12], &800_u16.to_be_bytes());
        assert_eq!(&envelope[12..16], &5_u32.to_be_bytes());
        assert_eq!(&envelope[16..24], &42_u64.to_be_bytes());
        assert_eq!(&envelope[24..32], &123_456_u64.to_be_bytes());
        assert_eq!(&envelope[32..40], &98_765_i64.to_be_bytes());
        assert_eq!(&envelope[40..], payload);
    }

    #[test]
    fn binary_frame_envelope_uses_explicit_optional_sentinels() {
        let envelope = encode_frame_envelope(
            FrameEnvelopeMetadata {
                width: 1,
                height: 1,
                codec: "av1",
                keyframe: false,
                discontinuity: false,
                sequence: None,
                capture_timestamp_micros: None,
                pts_micros: None,
            },
            &[1],
        )
        .unwrap();
        assert_eq!(envelope[5], 3);
        assert_eq!(&envelope[16..24], &u64::MAX.to_be_bytes());
        assert_eq!(&envelope[24..32], &u64::MAX.to_be_bytes());
        assert_eq!(&envelope[32..40], &i64::MIN.to_be_bytes());
    }

    #[test]
    fn binary_frame_envelope_rejects_invalid_metadata_before_sending() {
        let metadata = |codec| FrameEnvelopeMetadata {
            width: 1280,
            height: 800,
            codec,
            keyframe: false,
            discontinuity: false,
            sequence: None,
            capture_timestamp_micros: None,
            pts_micros: None,
        };
        assert!(encode_frame_envelope(metadata("vp9"), &[1]).is_err());
        assert!(encode_frame_envelope(metadata("h264"), &[]).is_err());
        assert!(
            encode_frame_envelope(
                FrameEnvelopeMetadata {
                    sequence: Some(u64::MAX),
                    ..metadata("h264")
                },
                &[1]
            )
            .is_err()
        );
        assert!(
            encode_frame_envelope(
                FrameEnvelopeMetadata {
                    pts_micros: Some(i64::MIN),
                    ..metadata("h264")
                },
                &[1]
            )
            .is_err()
        );
        assert!(
            encode_frame_envelope(
                FrameEnvelopeMetadata {
                    width: u32::from(MAX_VIDEO_DIMENSION) + 1,
                    ..metadata("h264")
                },
                &[1]
            )
            .is_err()
        );
    }

    #[test]
    fn frame_channel_slots_are_bounded_and_cannot_underflow() {
        let in_flight = AtomicUsize::new(0);
        release_frame_channel_slot(&in_flight);
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);

        for expected in 1..=CLIENT_FRAME_CHANNEL_CAPACITY {
            assert!(try_reserve_frame_channel_slot(&in_flight));
            assert_eq!(in_flight.load(Ordering::SeqCst), expected);
        }
        assert!(!try_reserve_frame_channel_slot(&in_flight));
        assert_eq!(
            in_flight.load(Ordering::SeqCst),
            CLIENT_FRAME_CHANNEL_CAPACITY
        );

        for expected in (0..CLIENT_FRAME_CHANNEL_CAPACITY).rev() {
            release_frame_channel_slot(&in_flight);
            assert_eq!(in_flight.load(Ordering::SeqCst), expected);
        }
        release_frame_channel_slot(&in_flight);
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn frame_acknowledgments_release_only_the_matching_generation() {
        let in_flight = AtomicUsize::new(2);
        let generation = 9;

        assert!(!release_frame_channel_slot_for_generation(
            &in_flight, generation, 8
        ));
        assert!(!release_frame_channel_slot_for_generation(
            &in_flight, generation, 0
        ));
        assert_eq!(in_flight.load(Ordering::SeqCst), 2);
        assert!(release_frame_channel_slot_for_generation(
            &in_flight, generation, 9
        ));
        assert_eq!(in_flight.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn global_frame_events_serialize_their_media_generation() {
        let frame = serde_json::to_value(FramePayload {
            generation: 17,
            width: 1280,
            height: 800,
            data: "jpeg".to_string(),
            keyframe: true,
            codec: "h264".to_string(),
            capture_timestamp_micros: Some(1),
            pts_micros: Some(2),
            discontinuity: false,
        })
        .unwrap();
        let error = serde_json::to_value(FrameErrorPayload {
            generation: 17,
            error: "closed".to_string(),
        })
        .unwrap();

        assert_eq!(frame["generation"], 17);
        assert_eq!(error["generation"], 17);
        assert_eq!(error["error"], "closed");
    }

    fn audio_packet(sequence: u64) -> AudioPacket {
        let payload = vec![0xf8, 0xff, 0xfe];
        AudioPacket::new(
            AudioPacketHeader::opus(
                payload.len(),
                sequence,
                sequence * 20_000,
                sequence as i64 * 20_000,
                AudioFlags::NONE,
            )
            .unwrap(),
            payload,
        )
        .unwrap()
    }

    #[test]
    fn audio_delivery_tokens_are_bounded_unique_and_acknowledged_exactly_once() {
        let mut deliveries = AudioDeliveryState::default();
        deliveries.begin_generation(7).unwrap();

        assert_eq!(deliveries.reserve(7).unwrap(), Some(1));
        assert_eq!(deliveries.reserve(7).unwrap(), Some(2));
        assert_eq!(deliveries.reserve(7).unwrap(), Some(3));
        assert_eq!(deliveries.depth(7), Some(AUDIO_DELIVERY_CAPACITY));
        assert_eq!(deliveries.reserve(7).unwrap(), None);

        deliveries.acknowledge(7, 2).unwrap();
        assert!(deliveries.acknowledge(7, 2).is_err());
        assert_eq!(deliveries.reserve(7).unwrap(), Some(4));
        assert!(deliveries.acknowledge(6, 1).is_err());
        assert!(deliveries.acknowledge(7, 99).is_err());
        assert_eq!(deliveries.depth(7), Some(AUDIO_DELIVERY_CAPACITY));
    }

    #[test]
    fn audio_generation_cancellation_rejects_stale_and_clears_current_tokens() {
        let counter = AtomicU64::new(11);
        let deliveries = StdMutex::new(AudioDeliveryState::default());
        lock_audio_deliveries(&deliveries)
            .begin_generation(11)
            .unwrap();
        assert_eq!(
            lock_audio_deliveries(&deliveries).reserve(11).unwrap(),
            Some(1)
        );

        assert!(!cancel_audio_generation(&counter, &deliveries, 10).unwrap());
        assert_eq!(counter.load(Ordering::SeqCst), 11);
        assert_eq!(lock_audio_deliveries(&deliveries).depth(11), Some(1));

        assert!(cancel_audio_generation(&counter, &deliveries, 11).unwrap());
        assert_eq!(counter.load(Ordering::SeqCst), 12);
        assert_eq!(lock_audio_deliveries(&deliveries).generation(), None);
        assert!(!cancel_audio_generation(&counter, &deliveries, 11).unwrap());
    }

    #[test]
    fn media_generations_are_nonzero_monotonic_and_checked_for_overflow() {
        let counter = AtomicU64::new(0);
        assert_eq!(next_media_generation(&counter).unwrap(), 1);
        assert_eq!(next_media_generation(&counter).unwrap(), 2);

        let exhausted = AtomicU64::new(u64::MAX);
        assert!(next_media_generation(&exhausted).is_err());
    }

    #[test]
    fn audio_reorder_window_is_bounded_and_marks_skipped_packets() {
        let mut reorder = AudioReorderBuffer::default();
        let (first, dropped) = reorder.insert(audio_packet(10)).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(dropped, 0);

        assert!(reorder.insert(audio_packet(12)).unwrap().0.is_empty());
        assert!(reorder.insert(audio_packet(13)).unwrap().0.is_empty());
        let (ordered, dropped) = reorder.insert(audio_packet(14)).unwrap();
        assert_eq!(dropped, 1);
        assert_eq!(
            ordered
                .iter()
                .map(|packet| packet.packet.header.sequence)
                .collect::<Vec<_>>(),
            vec![12, 13, 14]
        );
        assert!(ordered[0].discontinuity);
        assert!(!ordered[1].discontinuity);
        assert!(reorder.packets.len() <= AudioReorderBuffer::CAPACITY);

        assert!(reorder.insert(audio_packet(16)).unwrap().0.is_empty());
        let (ordered, dropped) = reorder.insert(audio_packet(15)).unwrap();
        assert_eq!(dropped, 0);
        assert_eq!(
            ordered
                .iter()
                .map(|packet| packet.packet.header.sequence)
                .collect::<Vec<_>>(),
            vec![15, 16]
        );
        assert!(reorder.insert(audio_packet(16)).unwrap().0.is_empty());
    }

    #[test]
    fn audio_channel_envelope_is_protocol_strict_and_can_force_discontinuity() {
        let encoded = encode_audio_channel_packet(9, 42, audio_packet(7), true).unwrap();
        assert_eq!(&encoded[0..4], b"SGAC");
        assert_eq!(&encoded[4..6], &1_u16.to_be_bytes());
        assert_eq!(&encoded[6..8], &24_u16.to_be_bytes());
        assert_eq!(&encoded[8..16], &9_u64.to_be_bytes());
        assert_eq!(&encoded[16..24], &42_u64.to_be_bytes());
        let decoded = AudioPacket::decode_datagram(&encoded[AUDIO_CHANNEL_HEADER_LEN..]).unwrap();
        assert_eq!(decoded.header.sequence, 7);
        assert!(decoded.header.flags.contains(AudioFlags::DISCONTINUITY));
        assert_eq!(decoded.payload, vec![0xf8, 0xff, 0xfe]);
    }

    #[test]
    fn client_connect_guard_rejects_overlap_and_resets_only_failed_attempts() {
        let active = Arc::new(AtomicBool::new(false));
        let attempt = ClientConnectGuard::acquire(Arc::clone(&active)).unwrap();
        assert!(ClientConnectGuard::acquire(Arc::clone(&active)).is_err());
        drop(attempt);
        assert!(!active.load(Ordering::SeqCst));

        ClientConnectGuard::acquire(Arc::clone(&active))
            .unwrap()
            .commit();
        assert!(active.load(Ordering::SeqCst));
        assert!(ClientConnectGuard::acquire(Arc::clone(&active)).is_err());
        active.store(false, Ordering::SeqCst);
    }

    #[test]
    fn generation_connection_close_retires_exactly_one_owned_handle() {
        let closes = AtomicUsize::new(0);
        let retired = close_generation_connection(Some((7, "media")), |connection| {
            assert_eq!(connection, "media");
            closes.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(retired, Some(7));
        assert_eq!(closes.load(Ordering::SeqCst), 1);

        let absent: Option<(u64, &str)> = None;
        assert_eq!(
            close_generation_connection(absent, |_| {
                closes.fetch_add(1, Ordering::SeqCst);
            }),
            None
        );
        assert_eq!(closes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn pointer_feedback_channel_has_explicit_bounded_terminal_envelopes() {
        let position = serde_json::to_value(PointerFeedbackPayload::Position {
            sequence: 7,
            position: Some(PointerPosition { x: 1280, y: 800 }),
            pointer_visible: Some(false),
        })
        .unwrap();
        assert_eq!(position["type"], "position");
        assert_eq!(position["sequence"], 7);
        assert_eq!(position["position"]["x"], 1280);
        assert_eq!(position["pointer_visible"], false);

        let legacy = serde_json::to_value(PointerFeedbackPayload::Position {
            sequence: 8,
            position: Some(PointerPosition { x: 640, y: 400 }),
            pointer_visible: None,
        })
        .unwrap();
        assert!(legacy.get("pointer_visible").is_none());

        let eof = serde_json::to_value(PointerFeedbackPayload::Terminal {
            reason: PointerFeedbackTerminalReason::Eof,
        })
        .unwrap();
        assert_eq!(
            eof,
            serde_json::json!({ "type": "terminal", "reason": "eof" })
        );

        let malformed = serde_json::to_value(PointerFeedbackPayload::Terminal {
            reason: PointerFeedbackTerminalReason::Malformed,
        })
        .unwrap();
        assert_eq!(
            malformed,
            serde_json::json!({ "type": "terminal", "reason": "malformed" })
        );
    }

    #[test]
    fn rolling_frame_rate_tracks_recent_cadence_ages_out_and_stays_bounded() {
        let mut rate = RollingFrameRate::default();
        for frame in 1..=60 {
            rate.record(Duration::from_micros(frame * 1_000_000 / 60));
        }
        assert!((rate.rate(Duration::from_secs(1)) - 60.0).abs() < 0.001);

        for frame in 1..=30 {
            rate.record(Duration::from_secs(1) + Duration::from_micros(frame * 1_000_000 / 30));
        }
        assert!((rate.rate(Duration::from_secs(2)) - 30.0).abs() < 0.001);
        assert_eq!(rate.rate(Duration::from_secs(4)), 0.0);

        for frame in 0..10_000_u64 {
            rate.record(Duration::from_secs(5) + Duration::from_micros(frame));
        }
        assert_eq!(rate.samples.len(), CLIENT_FRAME_RATE_SAMPLE_CAPACITY);
    }

    #[test]
    fn rolling_duration_window_reports_nearest_rank_percentiles_and_stays_bounded() {
        let mut window = RollingDurationWindow::default();
        for milliseconds in 1..=100_u64 {
            window.record(
                Duration::from_millis(milliseconds),
                Duration::from_millis(milliseconds),
            );
        }

        let summary = window.summary(Duration::from_millis(100));
        assert_eq!(summary.sample_count, 100);
        assert_eq!(summary.p50_ms, Some(50.0));
        assert_eq!(summary.p95_ms, Some(95.0));
        assert_eq!(summary.max_ms, Some(100.0));

        for sample in 0..10_000_u64 {
            window.record(
                Duration::from_secs(1) + Duration::from_micros(sample),
                Duration::from_micros(sample),
            );
        }
        assert_eq!(window.samples.len(), CLIENT_FRAME_TIMING_SAMPLE_CAPACITY);
        assert_eq!(
            window.summary(Duration::from_secs(10)),
            DurationWindowSummary::default()
        );
    }

    #[test]
    fn media_metrics_separate_drop_causes_and_preserve_compatibility_aliases() {
        let mut metrics = ClientMediaMetrics::default();
        metrics.observe_transport_receive(Duration::from_millis(100), Some(40), false);
        metrics.observe_transport_receive(Duration::from_millis(200), Some(43), true);
        metrics.observe_sequence_drop(2);
        metrics.observe_transport_object_drop(false);
        metrics.observe_transport_object_drop(true);
        metrics.observe_frontend_queue_drop();
        metrics.begin_frontend_resync(Duration::from_millis(200));
        metrics.begin_frontend_resync(Duration::from_millis(210));
        metrics.observe_frontend_resync_drop();
        metrics.observe_frontend_resync_drop();
        metrics.observe_frontend_queue_depth(3);
        metrics.observe_frontend_send(Duration::from_millis(200));
        metrics.observe_frontend_ipc_send_duration(
            Duration::from_millis(200),
            Duration::from_millis(3),
        );
        metrics.finish_frontend_resync(Duration::from_millis(230));

        let payload = metrics.snapshot(Duration::from_millis(250), 2, "direct", Some(7.5), 9);
        let json = serde_json::to_value(payload).unwrap();

        assert_eq!(json["generation"], 9);
        assert_eq!(json["stats_version"], 3);
        assert_eq!(json["transport_received_total"], 2);
        assert_eq!(json["frontend_sent_total"], 1);
        assert_eq!(json["sequence_dropped_total"], 2);
        assert_eq!(json["transport_object_dropped_total"], 2);
        assert_eq!(json["transport_late_object_dropped_total"], 1);
        assert_eq!(json["frontend_queue_dropped_total"], 1);
        assert_eq!(json["frontend_resync_dropped_total"], 2);
        assert_eq!(json["frontend_dropped_total"], 3);
        assert_eq!(json["frontend_queue_depth"], 2);
        assert_eq!(json["frontend_queue_peak"], 3);
        assert_eq!(json["frontend_queue_capacity"], 4);
        assert_eq!(json["frontend_resync_episode_total"], 1);
        assert_eq!(json["frontend_resync_active"], false);
        assert_eq!(json["frontend_resync_duration_ms_total"], 30.0);
        assert_eq!(
            json["frontend_resync_duration_ms_current"],
            serde_json::Value::Null
        );
        assert_eq!(json["frontend_resync_duration_ms_max"], 30.0);
        assert_eq!(json["timing_window_ms"], 5_000.0);
        assert_eq!(json["timing_sample_capacity"], 512);
        assert_eq!(json["transport_interval_sample_count"], 1);
        assert_eq!(json["transport_interval_p50_ms"], 100.0);
        assert_eq!(json["transport_interval_p95_ms"], 100.0);
        assert_eq!(json["transport_interval_max_ms"], 100.0);
        assert_eq!(json["frontend_ipc_send_duration_sample_count"], 1);
        assert_eq!(json["frontend_ipc_send_duration_p50_ms"], 3.0);
        assert_eq!(json["frontend_ipc_send_duration_p95_ms"], 3.0);
        assert_eq!(json["frontend_ipc_send_duration_max_ms"], 3.0);
        assert_eq!(json["sequence"], 43);
        assert_eq!(json["keyframe"], true);
        assert_eq!(json["path_mode"], "direct");
        assert_eq!(json["path_rtt_ms"], 7.5);

        assert_eq!(json["count"], json["frontend_sent_total"]);
        assert_eq!(json["host_dropped_frames"], json["sequence_dropped_total"]);
        assert_eq!(
            json["frontend_dropped_frames"],
            json["frontend_dropped_total"]
        );
        assert_eq!(json["fps"], json["frontend_send_fps"]);
        assert_eq!(json["transport_fps"], json["transport_receive_fps"]);
        assert_eq!(json["frontend_fps"], json["frontend_send_fps"]);
    }

    #[test]
    fn initial_keyframe_wait_is_one_measured_resync_episode() {
        let mut metrics = ClientMediaMetrics::default();
        metrics.begin_frontend_resync(Duration::ZERO);
        metrics.begin_frontend_resync(Duration::from_millis(50));
        metrics.observe_frontend_resync_drop();

        let active = metrics.snapshot(Duration::from_millis(75), 4, "direct", None, 1);
        assert_eq!(active.generation, 1);
        assert_eq!(active.frontend_resync_episode_total, 1);
        assert!(active.frontend_resync_active);
        assert_eq!(active.frontend_resync_duration_ms_total, 75.0);
        assert_eq!(active.frontend_resync_duration_ms_current, Some(75.0));
        assert_eq!(active.frontend_resync_duration_ms_max, 75.0);
        assert_eq!(active.frontend_resync_dropped_total, 1);

        metrics.finish_frontend_resync(Duration::from_millis(100));
        let completed = metrics.snapshot(Duration::from_millis(150), 0, "direct", None, 1);
        assert_eq!(completed.frontend_resync_episode_total, 1);
        assert!(!completed.frontend_resync_active);
        assert_eq!(completed.frontend_resync_duration_ms_total, 100.0);
        assert_eq!(completed.frontend_resync_duration_ms_current, None);
        assert_eq!(completed.frontend_resync_duration_ms_max, 100.0);
    }

    #[test]
    fn media_metrics_fps_decays_during_idle_without_new_frames() {
        let mut metrics = ClientMediaMetrics::default();
        for frame in 1..=60 {
            let elapsed = Duration::from_micros(frame * 1_000_000 / 60);
            metrics.observe_transport_receive(elapsed, Some(frame), frame == 60);
            metrics.observe_frontend_send(elapsed);
        }

        let active = metrics.snapshot(Duration::from_secs(1), 0, "relay", None, 1);
        assert!((active.transport_receive_fps - 60.0).abs() < 0.001);
        assert!((active.frontend_send_fps - 60.0).abs() < 0.001);

        let idle = metrics.snapshot(Duration::from_secs(3), 0, "relay", None, 1);
        assert_eq!(idle.transport_receive_fps, 0.0);
        assert_eq!(idle.frontend_send_fps, 0.0);
        assert_eq!(idle.transport_received_total, 60);
        assert_eq!(idle.frontend_sent_total, 60);
    }

    #[test]
    fn transport_interval_resets_after_damage_idle_but_retains_exact_boundary() {
        let mut metrics = ClientMediaMetrics::default();
        metrics.observe_transport_receive(Duration::ZERO, Some(1), true);
        metrics.observe_transport_receive(CLIENT_FRAME_TIMING_WINDOW, Some(2), false);
        let boundary = metrics
            .transport_intervals
            .summary(CLIENT_FRAME_TIMING_WINDOW);
        assert_eq!(boundary.sample_count, 1);
        assert_eq!(boundary.max_ms, Some(5_000.0));

        let mut resumed_metrics = ClientMediaMetrics::default();
        resumed_metrics.observe_transport_receive(Duration::ZERO, Some(1), true);
        let after_idle = CLIENT_FRAME_TIMING_WINDOW + Duration::from_millis(1);
        resumed_metrics.observe_transport_receive(after_idle, Some(2), false);
        resumed_metrics.observe_transport_receive(
            after_idle + Duration::from_millis(16),
            Some(3),
            false,
        );
        let resumed = resumed_metrics
            .transport_intervals
            .summary(after_idle + Duration::from_millis(16));
        assert_eq!(resumed.sample_count, 1);
        assert_eq!(resumed.p50_ms, Some(16.0));
        assert_eq!(resumed.p95_ms, Some(16.0));
        assert_eq!(resumed.max_ms, Some(16.0));
    }

    #[test]
    fn legacy_media_header_is_bounded_before_allocation() {
        assert!(validate_legacy_media_header(1280, 800, 1024).is_ok());
        assert!(validate_legacy_media_header(0, 800, 1024).is_err());
        assert!(validate_legacy_media_header(1280, 800, 0).is_err());
        assert!(validate_legacy_media_header(1280, 800, MAX_MEDIA_PAYLOAD_LEN + 1).is_err());
        assert!(validate_legacy_media_header(u32::MAX, u32::MAX, 1024).is_err());
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
    fn input_events_require_their_negotiated_capability() {
        let pointer = [Capability::AbsolutePointer];
        assert!(input_event_allowed(
            &pointer,
            &InputEvent::MouseMove { x: 1, y: 2 }
        ));
        assert!(input_event_allowed(
            &pointer,
            &InputEvent::MouseClick { b: 1 }
        ));
        assert!(input_event_allowed(
            &pointer,
            &InputEvent::MouseDown { b: 1 }
        ));
        assert!(input_event_allowed(&pointer, &InputEvent::MouseUp { b: 1 }));
        assert!(input_event_allowed(
            &pointer,
            &InputEvent::MouseScroll { dx: 0, dy: 1 }
        ));
        assert!(!input_event_allowed(
            &pointer,
            &InputEvent::MouseMoveRelative { dx: 1, dy: -2 }
        ));
        assert!(!input_event_allowed(
            &pointer,
            &InputEvent::MousePositionSync { x: 640, y: 400 }
        ));
        assert!(!input_event_allowed(
            &pointer,
            &InputEvent::KeyDown { k: "A".into() }
        ));
        assert!(!input_event_allowed(
            &pointer,
            &InputEvent::Text { s: "a".into() }
        ));

        let relative_pointer = [Capability::RelativePointer];
        assert!(input_event_allowed(
            &relative_pointer,
            &InputEvent::MouseMoveRelative { dx: 1, dy: -2 }
        ));
        assert!(input_event_allowed(
            &relative_pointer,
            &InputEvent::MousePositionSync { x: 640, y: 400 }
        ));
        assert!(input_event_allowed(
            &relative_pointer,
            &InputEvent::MouseDown { b: 1 }
        ));
        assert!(input_event_allowed(
            &relative_pointer,
            &InputEvent::MouseUp { b: 1 }
        ));
        assert!(input_event_allowed(
            &relative_pointer,
            &InputEvent::MouseScroll { dx: 0, dy: 1 }
        ));
        assert!(!input_event_allowed(
            &relative_pointer,
            &InputEvent::MouseMove { x: 1, y: 2 }
        ));

        let keyboard = [Capability::Keyboard];
        assert!(input_event_allowed(
            &keyboard,
            &InputEvent::KeyDown { k: "A".into() }
        ));
        assert!(input_event_allowed(
            &keyboard,
            &InputEvent::KeyUp { k: "A".into() }
        ));
        assert!(input_event_allowed(
            &keyboard,
            &InputEvent::KeyClick { k: "A".into() }
        ));
        assert!(!input_event_allowed(
            &keyboard,
            &InputEvent::Text { s: "a".into() }
        ));

        let text = [Capability::Text];
        assert!(input_event_allowed(
            &text,
            &InputEvent::Text { s: "a".into() }
        ));
        assert!(!input_event_allowed(
            &text,
            &InputEvent::KeyDown { k: "A".into() }
        ));

        let gamepad = [Capability::Gamepad];
        assert!(input_event_allowed(
            &gamepad,
            &InputEvent::Gamepad {
                state: sigil_protocol::GamepadState::default(),
            }
        ));
        assert!(!input_event_allowed(
            &keyboard,
            &InputEvent::Gamepad {
                state: sigil_protocol::GamepadState::default(),
            }
        ));
    }

    #[test]
    fn empty_input_capabilities_are_view_only() {
        let capabilities = [];
        assert!(!input_event_allowed(
            &capabilities,
            &InputEvent::MouseMove { x: 1, y: 2 }
        ));
        assert!(!input_event_allowed(
            &capabilities,
            &InputEvent::KeyDown { k: "A".into() }
        ));
        assert!(!input_event_allowed(
            &capabilities,
            &InputEvent::Text { s: "a".into() }
        ));
        assert_eq!(
            InputAvailability::from_capabilities(&capabilities),
            InputAvailability {
                relative_pointer: false,
                pointer_position_feedback: false,
                absolute_pointer: false,
                keyboard: false,
                text: false,
                gamepad: false,
                control: false,
            }
        );
    }

    #[test]
    fn input_availability_reports_each_accepted_capability_exactly() {
        assert_eq!(
            InputAvailability::from_capabilities(&[Capability::AbsolutePointer, Capability::Text]),
            InputAvailability {
                relative_pointer: false,
                pointer_position_feedback: false,
                absolute_pointer: true,
                keyboard: false,
                text: true,
                gamepad: false,
                control: true,
            }
        );
        assert_eq!(
            InputAvailability::from_capabilities(&[Capability::Keyboard]),
            InputAvailability {
                relative_pointer: false,
                pointer_position_feedback: false,
                absolute_pointer: false,
                keyboard: true,
                text: false,
                gamepad: false,
                control: true,
            }
        );
        assert_eq!(
            InputAvailability::from_capabilities(&[Capability::Gamepad]),
            InputAvailability {
                relative_pointer: false,
                pointer_position_feedback: false,
                absolute_pointer: false,
                keyboard: false,
                text: false,
                gamepad: true,
                control: true,
            }
        );
        assert_eq!(
            InputAvailability::from_capabilities(&[
                Capability::RelativePointer,
                Capability::PointerPositionFeedback,
            ]),
            InputAvailability {
                relative_pointer: true,
                pointer_position_feedback: true,
                absolute_pointer: false,
                keyboard: false,
                text: false,
                gamepad: false,
                control: true,
            }
        );
    }

    #[test]
    fn input_capability_fallbacks_remove_only_one_protocol_extension_at_a_time() {
        let [visibility, position, relative, inherited] = input_capability_offers();

        assert!(visibility.contains(&Capability::PointerVisibilityFeedback));
        assert!(visibility.contains(&Capability::PointerPositionFeedback));
        assert!(!position.contains(&Capability::PointerVisibilityFeedback));
        assert!(position.contains(&Capability::PointerPositionFeedback));
        assert!(!relative.contains(&Capability::PointerVisibilityFeedback));
        assert!(!relative.contains(&Capability::PointerPositionFeedback));
        assert!(relative.contains(&Capability::RelativePointer));
        assert!(!inherited.contains(&Capability::RelativePointer));
        assert_eq!(
            inherited,
            vec![
                Capability::AbsolutePointer,
                Capability::Keyboard,
                Capability::Text,
                Capability::Gamepad,
            ]
        );
    }

    #[test]
    fn relative_pointer_accumulator_coalesces_chunks_and_resets() {
        let mut accumulator = RelativePointerAccumulator::default();
        accumulator.push(10, -20);
        accumulator.push(5, 8);
        assert_eq!(
            accumulator.take(),
            Some(InputEvent::MouseMoveRelative { dx: 15, dy: -12 })
        );
        assert_eq!(accumulator.take(), None);

        accumulator.push(RELATIVE_POINTER_DELTA_MAX, RELATIVE_POINTER_DELTA_MIN);
        accumulator.push(1_000, -1_000);
        assert_eq!(
            accumulator.take(),
            Some(InputEvent::MouseMoveRelative {
                dx: RELATIVE_POINTER_DELTA_MAX,
                dy: RELATIVE_POINTER_DELTA_MIN,
            })
        );
        assert_eq!(
            accumulator.take(),
            Some(InputEvent::MouseMoveRelative {
                dx: 1_000,
                dy: -1_000,
            })
        );
        assert_eq!(accumulator.take(), None);
    }

    #[test]
    fn relative_motion_chunks_are_staged_immediately_before_a_following_button() {
        let mut accumulator = RelativePointerAccumulator::default();
        assert_eq!(
            stage_relative_input(
                &mut accumulator,
                InputEvent::MouseMoveRelative {
                    dx: RELATIVE_POINTER_DELTA_MAX,
                    dy: RELATIVE_POINTER_DELTA_MIN,
                }
            ),
            None
        );
        assert_eq!(
            stage_relative_input(
                &mut accumulator,
                InputEvent::MouseMoveRelative { dx: 7, dy: -1 }
            ),
            None
        );
        assert_eq!(
            stage_relative_input(&mut accumulator, InputEvent::MouseDown { b: 1 }),
            Some(InputEvent::MouseDown { b: 1 })
        );
        assert_eq!(
            accumulator.take(),
            Some(InputEvent::MouseMoveRelative {
                dx: RELATIVE_POINTER_DELTA_MAX,
                dy: RELATIVE_POINTER_DELTA_MIN,
            })
        );
        assert_eq!(
            accumulator.take(),
            Some(InputEvent::MouseMoveRelative { dx: 7, dy: -1 })
        );
        assert_eq!(accumulator.take(), None);
    }
}

#[tauri::command]
pub async fn iroh_client_send_input(
    state: State<'_, AppState>,
    event: InputEvent,
) -> Result<bool, String> {
    let tx = state
        .input_send
        .lock()
        .await
        .clone()
        .ok_or_else(|| "Not connected to host".to_string())?;
    match tx.try_send(event) {
        Ok(()) => Ok(true),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(false),
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            Err("Input channel closed".to_string())
        }
    }
}

#[tauri::command]
pub async fn iroh_client_ack_frame(
    state: State<'_, AppState>,
    generation: u64,
) -> Result<bool, String> {
    // Serialize selection of the generation-owned counter against connect and
    // disconnect. Each media task keeps its own counter, so an old callback can
    // never consume a permit reserved by a replacement session.
    let _connection_serial = state.client_connection_serial.lock().await;
    let delivery = state.frame_delivery.lock().await;
    let Some((current_generation, in_flight)) = delivery.as_ref() else {
        return Ok(false);
    };
    Ok(release_frame_channel_slot_for_generation(
        in_flight,
        *current_generation,
        generation,
    ))
}

#[tauri::command]
pub fn iroh_client_ack_audio(
    state: State<'_, AppState>,
    generation: u64,
    delivery_id: u64,
) -> Result<bool, String> {
    lock_audio_deliveries(&state.audio_deliveries).acknowledge(generation, delivery_id)?;
    Ok(true)
}

fn cancel_audio_generation(
    generation_counter: &AtomicU64,
    deliveries: &StdMutex<AudioDeliveryState>,
    expected_generation: u64,
) -> Result<bool, String> {
    let replacement_generation = expected_generation
        .checked_add(1)
        .ok_or_else(|| "Audio connection generation overflowed".to_string())?;
    let mut deliveries = lock_audio_deliveries(deliveries);
    if deliveries.generation() != Some(expected_generation)
        || generation_counter.load(Ordering::SeqCst) != expected_generation
    {
        return Ok(false);
    }
    if generation_counter
        .compare_exchange(
            expected_generation,
            replacement_generation,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_err()
    {
        return Ok(false);
    }
    if !deliveries.cancel_generation(expected_generation) {
        return Err("Audio delivery generation changed during cancellation".to_string());
    }
    Ok(true)
}

#[tauri::command]
pub async fn iroh_client_stop_audio(
    state: State<'_, AppState>,
    expected_generation: u64,
) -> Result<bool, String> {
    if !cancel_audio_generation(
        &state.audio_connection_generation,
        &state.audio_deliveries,
        expected_generation,
    )? {
        return Ok(false);
    }

    let connection = {
        let mut audio_connection = state.audio_connection.lock().await;
        if audio_connection
            .as_ref()
            .is_some_and(|(generation, _)| *generation == expected_generation)
        {
            audio_connection.take().map(|(_, connection)| connection)
        } else {
            None
        }
    };
    if let Some(connection) = connection {
        connection.close(0_u32.into(), b"audio stopped by client");
    }
    Ok(true)
}

#[tauri::command]
pub async fn iroh_client_disconnect(state: State<'_, AppState>) -> Result<bool, String> {
    let _connection_serial = state.client_connection_serial.lock().await;
    next_audio_generation(&state.audio_connection_generation)?;
    lock_audio_deliveries(&state.audio_deliveries).clear();
    // Do not rely on endpoint shutdown alone to retire the session. The frame
    // reader and diagnostics task both own connection clones, and a surviving
    // media connection keeps the host's encoder and one-client lease alive.
    // Closing the media connection explicitly gives the host an immediate,
    // protocol-level session boundary.
    close_generation_connection(state.media_connection.lock().await.take(), |connection| {
        connection.close(0_u32.into(), b"client disconnected");
    });
    close_generation_connection(state.audio_connection.lock().await.take(), |connection| {
        connection.close(0_u32.into(), b"client disconnected");
    });
    {
        let mut ce = state.client_endpoint.lock().await;
        if let Some(endpoint) = ce.take()
            && tokio::time::timeout(CLIENT_ENDPOINT_CLOSE_TIMEOUT, endpoint.close())
                .await
                .is_err()
        {
            eprintln!(
                "[client] timed out waiting for endpoint shutdown after explicit connection close"
            );
        }
    }
    {
        let mut input_send = state.input_send.lock().await;
        *input_send = None;
    }
    *state.frame_delivery.lock().await = None;
    state
        .client_connection_active
        .store(false, Ordering::SeqCst);
    Ok(true)
}
