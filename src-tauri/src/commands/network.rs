use super::auth::derive_iroh_secret_from_key;
use super::enrollment::{connection_enrollment, mark_invitation_redeemed};
use super::moq_catalog::subscribe_goq_video_track;
#[cfg(test)]
use super::network_diagnostics::PathMode;
use super::network_diagnostics::{
    NetworkDiagnosticsSnapshot, NetworkLeg, NetworkSessionDiagnostics,
};
use super::state::{
    AUDIO_DELIVERY_CAPACITY, AccumulatedMediaFeedback, AppState, AudioDeliveryState,
    CLIENT_INPUT_QUEUE_CAPACITY, FRAME_ALPN, INPUT_ALPN, MediaFeedbackSender,
    development_direct_node_available,
};
use base64::Engine;
use iroh::{Endpoint, SecretKey, endpoint::presets};
use iroh_moq::{Moq, MoqSession};
#[cfg(test)]
use moq_net::Track;
use moq_net::{BroadcastConsumer, GroupConsumer, TrackConsumer};
use openh264::{formats::YUVSource, nal_units};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use sigil_protocol::MOQ_VIDEO_H264_TRACK;
use sigil_protocol::{
    AUDIO_ALPN_V1, AdaptiveBitrateDecisionV1, AdaptiveBitrateReasonFlagsV1, AdaptiveBitrateStateV1,
    AudioFlags, AudioPacket, AudioPacketHeader, CONTROL_ALPN_V1, Capability, ClientHello,
    FrameFlags, INPUT_ALPN_V1, InputEvent, InvitationGrants, KeyframeRequestReasonV3,
    MAX_MEDIA_GROUP_BYTES_V3, MAX_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS, MAX_MEDIA_OBJECT_ID_V3,
    MAX_MEDIA_PAYLOAD_LEN, MAX_VIDEO_DIMENSION, MAX_VIDEO_PIXELS, MEDIA_ALPN_V1, MEDIA_ALPN_V2,
    MEDIA_ALPN_V3, MEDIA_FEEDBACK_ALPN_V1, MediaCodec, MediaControlRequestV3, MediaFeedbackFlags,
    MediaFeedbackReportV1, MediaFrame, MediaObjectV3, PointerPosition, PointerSurfaceDimensions,
    ProtocolError, RELATIVE_POINTER_DELTA_MAX, RELATIVE_POINTER_DELTA_MIN,
    decode_media_frame_object, media_moq_broadcast_name, read_adaptive_bitrate_decision_v1,
    read_host_hello, read_input_ack, read_media_frame, read_media_object, read_media_object_v3,
    write_client_hello, write_input_event, write_media_control_request_v3,
    write_media_feedback_report_v1,
};
use std::collections::{BTreeMap, VecDeque};
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard};
use std::time::{Duration, Instant};
use tauri::{
    AppHandle, Emitter, Manager, State,
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
const CLIENT_MOQ_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CLIENT_MOQ_SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(10);
// Match the protocol's maximum single-object delivery horizon. A publisher may
// never hold a partially delivered object open indefinitely.
const CLIENT_MOQ_OBJECT_READ_TIMEOUT: Duration =
    Duration::from_millis(MAX_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS as u64);
// Sigil's external encoder can take 500 ms to reach its next configured IDR.
// Allow another 500 ms for a relay path to deliver the superseding group.
const CLIENT_MOQ_GROUP_RECOVERY_TIMEOUT: Duration = Duration::from_secs(1);
// Absorb brief webview→Rust acknowledgment jitter without allowing Tauri IPC
// to grow without bound. Four 60 fps frames cap this handoff at about 67 ms;
// WebCodecs has a separate, stricter decode-queue bound in the frontend.
const CLIENT_FRAME_CHANNEL_CAPACITY: usize = 4;
// Three 20 ms Opus packets cap the Rust→webview handoff at 60 ms. The
// AudioWorklet owns a separate fixed ring and never feeds back into transport.
const CLIENT_FRAME_STATS_INTERVAL: Duration = Duration::from_millis(250);
const CLIENT_ENDPOINT_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const CLIENT_MEDIA_FEEDBACK_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const CLIENT_MEDIA_FEEDBACK_IO_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_ADAPTIVE_DECISION_DELIVERY_INTERVAL: Duration = Duration::from_secs(1);
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
const FRAME_CHANNEL_FLAG_CODEC_CONFIG: u8 = 1 << 2;
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
        network_diagnostics: NetworkDiagnosticsSnapshot,
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
            stats_version: 4,
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
            path_mode: network_diagnostics.media.mode.as_str(),
            path_rtt_ms: network_diagnostics.media.rtt_current_ms,
            network_diagnostics,
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

fn lock_network_diagnostics(
    diagnostics: &StdMutex<NetworkSessionDiagnostics>,
) -> StdMutexGuard<'_, NetworkSessionDiagnostics> {
    diagnostics
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

fn take_generation_owned<T>(slot: &mut Option<(u64, T)>, expected_generation: u64) -> Option<T> {
    if slot
        .as_ref()
        .is_some_and(|(generation, _)| *generation == expected_generation)
    {
        slot.take().map(|(_, value)| value)
    } else {
        None
    }
}

fn take_generation_owned_triple<T, U>(
    slot: &mut Option<(u64, T, U)>,
    expected_generation: u64,
) -> Option<(T, U)> {
    if slot
        .as_ref()
        .is_some_and(|(generation, _, _)| *generation == expected_generation)
    {
        slot.take().map(|(_, value, companion)| (value, companion))
    } else {
        None
    }
}

async fn retire_media_feedback_generation(app: &AppHandle, generation: u64) -> bool {
    let state = app.state::<AppState>();
    let connection = {
        let _connection_serial = state.client_connection_serial.lock().await;
        let mut slot = state.media_feedback.lock().await;
        take_generation_owned_triple(&mut slot, generation).map(|(connection, _)| connection)
    };
    let Some(connection) = connection else {
        return false;
    };
    connection.close(0_u32.into(), b"adaptive feedback ended");
    true
}

async fn retire_upstream_moq_generation(
    app: &AppHandle,
    media_generation: u64,
    audio_generation: Option<u64>,
) -> bool {
    let state = app.state::<AppState>();
    let endpoint = {
        // Selection and retirement are one generation-checked transaction.
        // A stale reader must never close a replacement session whose task was
        // installed after an explicit disconnect/reconnect.
        let _connection_serial = state.client_connection_serial.lock().await;
        let media_connection = {
            let mut slot = state.media_connection.lock().await;
            take_generation_owned(&mut slot, media_generation)
        };
        let Some(media_connection) = media_connection else {
            return false;
        };

        {
            let mut control = state.media_control.lock().await;
            let _ = take_generation_owned(&mut control, media_generation);
        }
        {
            let mut delivery = state.frame_delivery.lock().await;
            let _ = take_generation_owned(&mut delivery, media_generation);
        }
        let feedback_connection = {
            let mut feedback = state.media_feedback.lock().await;
            take_generation_owned_triple(&mut feedback, media_generation)
                .map(|(connection, _)| connection)
        };
        *state.input_send.lock().await = None;

        let audio_connection = if let Some(audio_generation) = audio_generation {
            if let Err(error) = cancel_audio_generation(
                &state.audio_connection_generation,
                &state.audio_deliveries,
                audio_generation,
            ) {
                eprintln!(
                    "[client] failed to retire audio generation after upstream MoQ ended: {error}"
                );
            }
            let mut slot = state.audio_connection.lock().await;
            take_generation_owned(&mut slot, audio_generation)
        } else {
            None
        };

        let endpoint = state.client_endpoint.lock().await.take();
        media_connection.close(0_u32.into(), b"upstream MoQ media ended");
        if let Some(feedback_connection) = feedback_connection {
            feedback_connection.close(0_u32.into(), b"upstream MoQ media ended");
        }
        if let Some(audio_connection) = audio_connection {
            audio_connection.close(0_u32.into(), b"upstream MoQ media ended");
        }
        state
            .client_connection_active
            .store(false, Ordering::SeqCst);
        endpoint
    };

    if let Some(endpoint) = endpoint
        && tokio::time::timeout(CLIENT_ENDPOINT_CLOSE_TIMEOUT, endpoint.close())
            .await
            .is_err()
    {
        eprintln!("[client] timed out retiring endpoint after upstream MoQ media ended");
    }
    true
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
    network_diagnostics: NetworkDiagnosticsSnapshot,
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
    codec_config: bool,
    sequence: Option<u64>,
    capture_timestamp_micros: Option<u64>,
    pts_micros: Option<i64>,
}

fn encode_frame_envelope(
    metadata: FrameEnvelopeMetadata<'_>,
    payload: &[u8],
) -> Result<Vec<u8>, String> {
    validate_legacy_media_header(metadata.width, metadata.height, payload.len())?;
    if metadata.codec_config && !metadata.keyframe {
        return Err("Frame codec configuration requires a keyframe".to_string());
    }
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
    if metadata.codec_config {
        flags |= FRAME_CHANNEL_FLAG_CODEC_CONFIG;
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
    pub adaptive_feedback_available: bool,
    pub adaptive_feedback_error: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ClientMediaFeedbackReport {
    pub interval_ms: u16,
    pub last_sequence: Option<u64>,
    pub transport_dropped_delta: u32,
    pub frontend_dropped_delta: u32,
    pub decoder_dropped_delta: u32,
    pub presenter_dropped_delta: u32,
    pub frontend_queue_depth: u8,
    pub frontend_queue_capacity: u8,
    pub decode_queue_depth: u8,
    pub decode_queue_capacity: u8,
    pub presenter_queue_depth: u8,
    pub presenter_queue_capacity: u8,
    pub transport_delivery_p95_ms: Option<f64>,
    pub decode_p95_ms: Option<f64>,
    pub presentation_p95_ms: Option<f64>,
    pub resync_active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct AdaptiveBitrateDecisionPayload {
    generation: u64,
    decision: AdaptiveBitrateDecisionDiagnostic,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct AdaptiveBitrateDecisionDiagnostic {
    decision_id: u64,
    report_id: u64,
    target_kbps: u32,
    floor_kbps: u32,
    ceiling_kbps: u32,
    state: &'static str,
    reasons: Vec<&'static str>,
    applied: bool,
}

struct NegotiatedV1Stream {
    session_id: u64,
    capabilities: Vec<Capability>,
    pointer_surface_dimensions: Option<PointerSurfaceDimensions>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaTransport {
    UpstreamMoq,
    LegacyV0,
    ReliableStreamV1,
    IndependentObjectsV2,
    GroupedObjectsV3,
}

impl MediaTransport {
    const fn diagnostic_name(self) -> &'static str {
        match self {
            Self::UpstreamMoq => "iroh-moq",
            Self::LegacyV0 => "reliable-v0",
            Self::ReliableStreamV1 => "reliable-v1",
            Self::IndependentObjectsV2 => "independent-v2",
            Self::GroupedObjectsV3 => "grouped-v3",
        }
    }

    const fn supports_adaptive_feedback(self) -> bool {
        matches!(self, Self::UpstreamMoq | Self::GroupedObjectsV3)
    }
}

#[derive(Debug)]
enum MoqMediaReadOutcome {
    Frame {
        frame: MediaFrame,
        discontinuity: bool,
    },
    Dropped {
        reason: KeyframeRequestReasonV3,
    },
    Malformed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MoqGroupRecovery {
    /// Sigil deliberately cancels the old GOP when a configured IDR starts a
    /// replacement. This is normal live-edge supersession, not evidence that
    /// another keyframe request is needed.
    ExpectedSupersession,
    RecoverableGap(KeyframeRequestReasonV3),
}

struct MoqGroupCursor {
    group: GroupConsumer,
    sequence: u64,
    object_count: usize,
    object_bytes: usize,
    group_gap: bool,
    replacement_for_cancelled_group: bool,
}

/// Own every upstream handle for as long as the Portal frame task is alive.
/// In particular, dropping `Moq` aborts its actor and dropping the consumers
/// cancels their subscriptions, so none of these may be scoped only to setup.
struct MoqMediaLifetime {
    _moq: Moq,
    _session: MoqSession,
    _broadcast: BroadcastConsumer,
}

struct MoqMediaReceiver {
    _lifetime: Option<MoqMediaLifetime>,
    track: TrackConsumer,
    current_group: Option<MoqGroupCursor>,
    last_group_sequence: Option<u64>,
    last_frame_sequence: Option<u64>,
    waiting_for_keyframe: bool,
    pending_group_recovery: Option<MoqGroupRecovery>,
}

impl MoqMediaReceiver {
    fn new(
        moq: Moq,
        session: MoqSession,
        broadcast: BroadcastConsumer,
        track: TrackConsumer,
    ) -> Self {
        Self {
            _lifetime: Some(MoqMediaLifetime {
                _moq: moq,
                _session: session,
                _broadcast: broadcast,
            }),
            track,
            current_group: None,
            last_group_sequence: None,
            last_frame_sequence: None,
            waiting_for_keyframe: true,
            pending_group_recovery: None,
        }
    }

    #[cfg(test)]
    fn for_test(track: TrackConsumer) -> Self {
        Self {
            _lifetime: None,
            track,
            current_group: None,
            last_group_sequence: None,
            last_frame_sequence: None,
            waiting_for_keyframe: true,
            pending_group_recovery: None,
        }
    }

    async fn next(&mut self) -> Result<Option<MoqMediaReadOutcome>, String> {
        self.next_with_timeouts(
            CLIENT_MOQ_OBJECT_READ_TIMEOUT,
            CLIENT_MOQ_GROUP_RECOVERY_TIMEOUT,
        )
        .await
    }

    async fn next_with_timeouts(
        &mut self,
        object_read_timeout: Duration,
        group_recovery_timeout: Duration,
    ) -> Result<Option<MoqMediaReadOutcome>, String> {
        loop {
            if self.current_group.is_none() {
                let replacement_for_cancelled_group = self.pending_group_recovery.is_some();
                let group = if replacement_for_cancelled_group {
                    match tokio::time::timeout(
                        group_recovery_timeout,
                        self.track.next_group(),
                    )
                    .await
                    {
                        Ok(group) => group,
                        Err(_) => {
                            let recovery = self
                                .pending_group_recovery
                                .take()
                                .expect("pending MoQ recovery reason was present");
                            return match recovery {
                                MoqGroupRecovery::ExpectedSupersession => Err(format!(
                                    "Timed out after {} ms waiting for the MoQ group that supersedes an expected GOP cancellation",
                                    group_recovery_timeout.as_millis()
                                )),
                                MoqGroupRecovery::RecoverableGap(reason) => {
                                    Ok(Some(MoqMediaReadOutcome::Dropped { reason }))
                                }
                            };
                        }
                    }
                } else {
                    self.track.next_group().await
                }
                .map_err(|error| format!("Upstream MoQ video track failed: {error}"))?;
                let Some(group) = group else {
                    return Ok(None);
                };
                self.pending_group_recovery = None;
                let sequence = group.sequence;
                let group_gap =
                    match classify_moq_group_sequence(self.last_group_sequence, sequence) {
                        Ok(group_gap) => group_gap,
                        Err(error) => return Ok(Some(MoqMediaReadOutcome::Malformed(error))),
                    };
                if group_gap {
                    self.waiting_for_keyframe = true;
                }
                self.current_group = Some(MoqGroupCursor {
                    group,
                    sequence,
                    object_count: 0,
                    object_bytes: 0,
                    group_gap,
                    replacement_for_cancelled_group,
                });
            }

            let cursor = self
                .current_group
                .as_mut()
                .expect("MoQ group cursor was initialized");
            let object =
                match tokio::time::timeout(object_read_timeout, cursor.group.read_frame()).await {
                    Err(_) => {
                        let sequence = cursor.sequence;
                        self.last_group_sequence = Some(sequence);
                        self.current_group = None;
                        self.waiting_for_keyframe = true;
                        return Ok(Some(MoqMediaReadOutcome::Dropped {
                            reason: KeyframeRequestReasonV3::DeliveryTimeout,
                        }));
                    }
                    Ok(Ok(object)) => object,
                    Ok(Err(error)) if moq_group_error_is_recoverable(&error) => {
                        let sequence = cursor.sequence;
                        self.last_group_sequence = Some(sequence);
                        self.current_group = None;
                        self.waiting_for_keyframe = true;
                        let reason = moq_group_error_reason(&error);
                        self.pending_group_recovery =
                            Some(if matches!(error, moq_net::Error::Cancel) {
                                MoqGroupRecovery::ExpectedSupersession
                            } else {
                                MoqGroupRecovery::RecoverableGap(reason)
                            });
                        continue;
                    }
                    Ok(Err(error)) => {
                        return Ok(Some(MoqMediaReadOutcome::Malformed(format!(
                            "Upstream MoQ group {} failed: {error}",
                            cursor.sequence
                        ))));
                    }
                };
            let Some(object) = object else {
                if cursor.object_count == 0 {
                    if cursor.replacement_for_cancelled_group {
                        self.last_group_sequence = Some(cursor.sequence);
                        self.current_group = None;
                        self.waiting_for_keyframe = true;
                        return Ok(Some(MoqMediaReadOutcome::Dropped {
                            reason: KeyframeRequestReasonV3::TransportGap,
                        }));
                    }
                    return Ok(Some(MoqMediaReadOutcome::Malformed(format!(
                        "Upstream MoQ group {} was empty",
                        cursor.sequence
                    ))));
                }
                self.last_group_sequence = Some(cursor.sequence);
                self.current_group = None;
                continue;
            };

            let next_group_bytes = match validate_moq_object_bounds(
                cursor.sequence,
                cursor.object_count,
                cursor.object_bytes,
                object.len(),
            ) {
                Ok(next_group_bytes) => next_group_bytes,
                Err(error) => return Ok(Some(MoqMediaReadOutcome::Malformed(error))),
            };
            let frame = match decode_media_frame_object(&object) {
                Ok(frame) => frame,
                Err(error) => {
                    return Ok(Some(MoqMediaReadOutcome::Malformed(format!(
                        "Invalid upstream MoQ media object in group {} object {}: {error}",
                        cursor.sequence, cursor.object_count
                    ))));
                }
            };

            let first_object = cursor.object_count == 0;
            let frame_contiguous = match validate_moq_group_frame(
                cursor.sequence,
                first_object,
                self.last_frame_sequence,
                &frame,
            ) {
                Ok(frame_contiguous) => frame_contiguous,
                Err(_error) if first_object && cursor.replacement_for_cancelled_group => {
                    self.last_group_sequence = Some(cursor.sequence);
                    self.current_group = None;
                    self.waiting_for_keyframe = true;
                    return Ok(Some(MoqMediaReadOutcome::Dropped {
                        reason: KeyframeRequestReasonV3::TransportGap,
                    }));
                }
                Err(_error) if !first_object => {
                    self.last_group_sequence = Some(cursor.sequence);
                    self.current_group = None;
                    self.waiting_for_keyframe = true;
                    return Ok(Some(MoqMediaReadOutcome::Dropped {
                        reason: KeyframeRequestReasonV3::TransportGap,
                    }));
                }
                Err(error) => return Ok(Some(MoqMediaReadOutcome::Malformed(error))),
            };
            let discontinuity = self.waiting_for_keyframe
                || (first_object && cursor.group_gap)
                || (first_object && !frame_contiguous)
                || frame.header.flags.contains(FrameFlags::DISCONTINUITY);
            cursor.object_count += 1;
            cursor.object_bytes = next_group_bytes;
            self.last_frame_sequence = Some(frame.header.sequence);
            self.waiting_for_keyframe = false;
            return Ok(Some(MoqMediaReadOutcome::Frame {
                frame,
                discontinuity,
            }));
        }
    }
}

fn classify_moq_group_sequence(previous: Option<u64>, current: u64) -> Result<bool, String> {
    let Some(previous) = previous else {
        return Ok(false);
    };
    if current <= previous {
        return Err(format!(
            "Upstream MoQ group sequence did not increase: previous={previous}, current={current}"
        ));
    }
    Ok(previous.checked_add(1) != Some(current))
}

fn validate_moq_object_bounds(
    group_sequence: u64,
    object_count: usize,
    object_bytes: usize,
    object_len: usize,
) -> Result<usize, String> {
    let max_objects = MAX_MEDIA_OBJECT_ID_V3 as usize + 1;
    if object_count >= max_objects {
        return Err(format!(
            "Upstream MoQ group {group_sequence} exceeded {max_objects} media objects"
        ));
    }
    let next_group_bytes = object_bytes
        .checked_add(object_len)
        .ok_or_else(|| "Upstream MoQ group byte count overflowed".to_string())?;
    if next_group_bytes > MAX_MEDIA_GROUP_BYTES_V3 {
        return Err(format!(
            "Upstream MoQ group {group_sequence} exceeded the {MAX_MEDIA_GROUP_BYTES_V3} byte limit"
        ));
    }
    Ok(next_group_bytes)
}

fn validate_moq_group_frame(
    group_sequence: u64,
    first_object: bool,
    last_frame_sequence: Option<u64>,
    frame: &MediaFrame,
) -> Result<bool, String> {
    if first_object
        && !(frame.header.codec == MediaCodec::H264
            && frame.header.flags.contains(FrameFlags::KEYFRAME)
            && frame.header.flags.contains(FrameFlags::CODEC_CONFIG))
    {
        return Err(format!(
            "Upstream MoQ group {group_sequence} did not begin with a configured H.264 keyframe"
        ));
    }
    let contiguous = last_frame_sequence
        .is_none_or(|previous| previous.checked_add(1) == Some(frame.header.sequence));
    if !first_object && !contiguous {
        return Err(format!(
            "Upstream MoQ group {group_sequence} contains a non-contiguous access-unit sequence"
        ));
    }
    Ok(contiguous)
}

fn moq_group_error_is_recoverable(error: &moq_net::Error) -> bool {
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

fn moq_group_error_reason(error: &moq_net::Error) -> KeyframeRequestReasonV3 {
    if matches!(error, moq_net::Error::Timeout | moq_net::Error::Remote(3)) {
        KeyframeRequestReasonV3::DeliveryTimeout
    } else {
        KeyframeRequestReasonV3::TransportGap
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

#[derive(Debug)]
enum MediaObjectReadOutcomeV3 {
    Object {
        accept_index: u64,
        object: MediaObjectV3,
    },
    Dropped {
        accept_index: u64,
        reason: KeyframeRequestReasonV3,
    },
    Malformed(String),
}

impl MediaObjectReadOutcomeV3 {
    fn accept_index(&self) -> Option<u64> {
        match self {
            Self::Object { accept_index, .. } | Self::Dropped { accept_index, .. } => {
                Some(*accept_index)
            }
            Self::Malformed(_) => None,
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
    completed: BTreeMap<u64, MediaObjectReadOutcomeV3>,
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

    fn push(
        &mut self,
        outcome: MediaObjectReadOutcomeV3,
    ) -> Result<Option<MediaObjectReadOutcomeV3>, String> {
        let Some(accept_index) = outcome.accept_index() else {
            return Ok(Some(outcome));
        };
        if accept_index < self.next_accept_index {
            // A discontinuity barrier may advance beyond older in-flight
            // reads. Their eventual timeout/reset outcomes belong to the
            // superseded GOP and must not poison the recovered sequence.
            return Ok(None);
        }
        if self.completed.insert(accept_index, outcome).is_some() {
            return Err(format!(
                "Duplicate media v3 accept index {accept_index} completed"
            ));
        }
        if self
            .completed
            .get(&accept_index)
            .is_some_and(MediaObjectReadOutcomeV3::is_fast_forward_barrier)
        {
            self.completed.retain(|index, _| *index >= accept_index);
            self.next_accept_index = accept_index;
        }
        self.take_next()
    }

    fn take_next(&mut self) -> Result<Option<MediaObjectReadOutcomeV3>, String> {
        let Some(outcome) = self.completed.remove(&self.next_accept_index) else {
            return Ok(None);
        };
        self.next_accept_index = self
            .next_accept_index
            .checked_add(1)
            .ok_or_else(|| "Media v3 accept index overflowed".to_string())?;
        Ok(Some(outcome))
    }
}

struct MediaObjectReceiverV3 {
    connection: iroh::endpoint::Connection,
    reads: tokio::task::JoinSet<MediaObjectReadOutcomeV3>,
    reorder: MediaObjectReorderV3,
    next_accept_index: u64,
    connection_closed: bool,
}

impl MediaObjectReceiverV3 {
    fn new(connection: iroh::endpoint::Connection) -> Self {
        Self {
            connection,
            reads: tokio::task::JoinSet::new(),
            reorder: MediaObjectReorderV3::new(1),
            next_accept_index: 0,
            connection_closed: false,
        }
    }

    async fn next(&mut self) -> Result<Option<MediaObjectReadOutcomeV3>, String> {
        loop {
            if let Some(completed) = self.reorder.take_next()? {
                return Ok(Some(completed));
            }
            if self.connection_closed && self.reads.is_empty() {
                if self.reorder.pending_len() != 0 {
                    return Err("Media v3 connection closed with incomplete object order".into());
                }
                return Ok(None);
            }

            tokio::select! {
                biased;
                completed = self.reads.join_next(), if !self.reads.is_empty() => {
                    let completed = completed
                        .ok_or_else(|| "Media v3 object reader ended unexpectedly".to_string())?
                        .map_err(|error| format!("Media v3 object reader task failed: {error}"))?;
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
                    self.next_accept_index = self.next_accept_index.checked_add(1)
                        .ok_or_else(|| "Media v3 accept index overflowed".to_string())?;
                    let accept_index = self.next_accept_index;
                    self.reads.spawn(async move {
                        match tokio::time::timeout(
                            CLIENT_MEDIA_OBJECT_READ_TIMEOUT,
                            read_media_object_v3(&mut stream),
                        )
                        .await
                        {
                            Err(_) => MediaObjectReadOutcomeV3::Dropped {
                                accept_index,
                                reason: KeyframeRequestReasonV3::DeliveryTimeout,
                            },
                            Ok(Err(ProtocolError::Io(_))) => MediaObjectReadOutcomeV3::Dropped {
                                accept_index,
                                reason: KeyframeRequestReasonV3::TransportGap,
                            },
                            Ok(Err(error)) => MediaObjectReadOutcomeV3::Malformed(format!(
                                "Invalid media v3 object: {error}"
                            )),
                            Ok(Ok(object)) => MediaObjectReadOutcomeV3::Object {
                                accept_index,
                                object,
                            },
                        }
                    });
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaObjectSequenceDecisionV3 {
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

    fn note_dropped_object(&mut self) -> bool {
        let entered = !self.waiting_for_keyframe;
        self.waiting_for_keyframe = true;
        entered
    }

    fn classify(&mut self, object: &MediaObjectV3) -> MediaObjectSequenceDecisionV3 {
        let header = &object.header;
        if self
            .group_id
            .is_some_and(|group_id| header.group_id < group_id)
            || self
                .last_sequence
                .is_some_and(|sequence| header.sequence <= sequence)
        {
            return MediaObjectSequenceDecisionV3::DropLate;
        }

        let new_group = self.group_id != Some(header.group_id);
        let recovery_keyframe = header.object_id == 0
            && header.flags.contains(FrameFlags::KEYFRAME)
            && header.flags.contains(FrameFlags::CODEC_CONFIG);
        if new_group && !recovery_keyframe {
            self.waiting_for_keyframe = true;
            return MediaObjectSequenceDecisionV3::DropUntilKeyframe;
        }
        if !new_group && self.waiting_for_keyframe {
            return MediaObjectSequenceDecisionV3::DropUntilKeyframe;
        }

        let sequence_contiguous = self
            .last_sequence
            .is_none_or(|sequence| sequence.checked_add(1) == Some(header.sequence));
        let object_contiguous = new_group
            || self
                .last_object_id
                .is_some_and(|object_id| object_id.checked_add(1) == Some(header.object_id));
        let next_group_bytes = if new_group {
            object.payload.len()
        } else {
            self.group_payload_bytes
                .saturating_add(object.payload.len())
        };
        if (!sequence_contiguous && !new_group)
            || !object_contiguous
            || next_group_bytes > MAX_MEDIA_GROUP_BYTES_V3
        {
            self.waiting_for_keyframe = true;
            return MediaObjectSequenceDecisionV3::DropUntilKeyframe;
        }

        let discontinuity = header.flags.contains(FrameFlags::DISCONTINUITY)
            || self.waiting_for_keyframe
            || !sequence_contiguous;
        self.group_id = Some(header.group_id);
        self.last_object_id = Some(header.object_id);
        self.last_sequence = Some(header.sequence);
        self.group_payload_bytes = next_group_bytes;
        self.waiting_for_keyframe = false;
        MediaObjectSequenceDecisionV3::Deliver { discontinuity }
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
    input_ack: bool,
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
        let input_ack = capabilities.contains(&Capability::InputAck);
        Self {
            relative_pointer,
            pointer_position_feedback,
            absolute_pointer,
            keyboard,
            text,
            gamepad,
            input_ack,
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
    invitation: Option<&str>,
) -> Result<NegotiatedV1Stream, String> {
    let mut hello = ClientHello::new("portal/0.1.0", nonce, capabilities.clone());
    if let Some(invitation) = invitation {
        hello = hello.with_invitation(invitation);
    }
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
        iroh::endpoint::Connection,
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
    let negotiation = negotiate_v1(
        &mut send,
        &mut recv,
        nonce,
        capabilities,
        None,
        "input",
        None,
    )
    .await?;
    Ok((connection, send, recv, negotiation))
}

fn connection_error_is_unsupported_alpn(error: &iroh::endpoint::ConnectionError) -> bool {
    matches!(
        error,
        iroh::endpoint::ConnectionError::ConnectionClosed(close)
            if close.error_code == iroh::endpoint::TransportErrorCode::crypto(0x78)
    )
}

fn connect_error_is_unsupported_alpn(error: &iroh::endpoint::ConnectError) -> bool {
    match error {
        iroh::endpoint::ConnectError::Connecting {
            source: iroh::endpoint::ConnectingError::ConnectionError { source, .. },
            ..
        }
        | iroh::endpoint::ConnectError::Connection { source, .. } => {
            connection_error_is_unsupported_alpn(source)
        }
        _ => false,
    }
}

async fn open_negotiated_feedback_stream(
    endpoint: &Endpoint,
    address: &iroh::EndpointAddr,
    nonce: [u8; 16],
    media_session_id: u64,
) -> Result<
    Option<(
        iroh::endpoint::Connection,
        iroh::endpoint::SendStream,
        iroh::endpoint::RecvStream,
    )>,
    String,
> {
    let connection = match endpoint
        .connect(address.clone(), MEDIA_FEEDBACK_ALPN_V1)
        .await
    {
        Ok(connection) => connection,
        Err(error) if connect_error_is_unsupported_alpn(&error) => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Failed to connect adaptive feedback stream: {error}"
            ));
        }
    };
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|error| format!("Failed to open adaptive feedback handshake: {error}"))?;
    let negotiation = negotiate_v1(
        &mut send,
        &mut recv,
        nonce,
        vec![Capability::VideoH264],
        Some(Capability::VideoH264),
        "adaptive feedback",
        None,
    )
    .await?;
    if negotiation.session_id != media_session_id {
        return Err("Host returned mismatched media and adaptive feedback sessions".to_string());
    }
    Ok(Some((connection, send, recv)))
}

fn adaptive_bitrate_state_name(state: AdaptiveBitrateStateV1) -> &'static str {
    match state {
        AdaptiveBitrateStateV1::Hold => "hold",
        AdaptiveBitrateStateV1::Decrease => "decrease",
        AdaptiveBitrateStateV1::Increase => "increase",
    }
}

fn adaptive_bitrate_reason_names(reasons: AdaptiveBitrateReasonFlagsV1) -> Vec<&'static str> {
    [
        (AdaptiveBitrateReasonFlagsV1::RTT_INFLATION, "rtt-inflation"),
        (
            AdaptiveBitrateReasonFlagsV1::LOSS_OR_CANCELLATION,
            "loss-or-cancellation",
        ),
        (
            AdaptiveBitrateReasonFlagsV1::SENDER_BACKPRESSURE,
            "sender-backpressure",
        ),
        (
            AdaptiveBitrateReasonFlagsV1::RECEIVER_QUEUE,
            "receiver-queue",
        ),
        (
            AdaptiveBitrateReasonFlagsV1::DECODE_BACKLOG,
            "decode-backlog",
        ),
        (
            AdaptiveBitrateReasonFlagsV1::DELIVERY_LATENCY,
            "delivery-latency",
        ),
        (
            AdaptiveBitrateReasonFlagsV1::CLEAN_RECOVERY,
            "clean-recovery",
        ),
        (
            AdaptiveBitrateReasonFlagsV1::FEEDBACK_STALE,
            "feedback-stale",
        ),
    ]
    .into_iter()
    .filter_map(|(flag, name)| reasons.contains(flag).then_some(name))
    .collect()
}

fn adaptive_bitrate_decision_diagnostic(
    decision: AdaptiveBitrateDecisionV1,
) -> AdaptiveBitrateDecisionDiagnostic {
    AdaptiveBitrateDecisionDiagnostic {
        decision_id: decision.decision_id,
        report_id: decision.report_id,
        target_kbps: decision.target_kbps,
        floor_kbps: decision.floor_kbps,
        ceiling_kbps: decision.ceiling_kbps,
        state: adaptive_bitrate_state_name(decision.state),
        reasons: adaptive_bitrate_reason_names(decision.reasons),
        applied: decision.applied,
    }
}

#[derive(Debug, Default)]
struct AdaptiveDecisionSequence {
    last_decision_id: Option<u64>,
    last_report_id: Option<u64>,
}

impl AdaptiveDecisionSequence {
    fn accept(&mut self, decision: &AdaptiveBitrateDecisionV1) -> Result<(), String> {
        if self
            .last_decision_id
            .is_some_and(|previous| decision.decision_id <= previous)
        {
            return Err(format!(
                "adaptive decision ID did not increase: previous={:?}, current={}",
                self.last_decision_id, decision.decision_id
            ));
        }
        if self
            .last_report_id
            .is_some_and(|previous| decision.report_id <= previous)
        {
            return Err(format!(
                "adaptive decision report ID did not increase: previous={:?}, current={}",
                self.last_report_id, decision.report_id
            ));
        }
        self.last_decision_id = Some(decision.decision_id);
        self.last_report_id = Some(decision.report_id);
        Ok(())
    }
}

async fn emit_paced_adaptive_decisions<F>(
    generation: u64,
    mut decisions: tokio::sync::watch::Receiver<Option<AdaptiveBitrateDecisionV1>>,
    delivery_interval: Duration,
    mut emit: F,
) -> Result<(), String>
where
    F: FnMut(AdaptiveBitrateDecisionPayload) -> Result<(), String>,
{
    let mut delivery = tokio::time::interval(delivery_interval);
    delivery.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        decisions
            .changed()
            .await
            .map_err(|_| "adaptive decision receiver closed".to_string())?;
        // The first tick is immediate. Later ticks pace the webview boundary;
        // any host burst replaces the watch value while this task waits.
        delivery.tick().await;
        let Some(decision) = *decisions.borrow_and_update() else {
            continue;
        };
        emit(AdaptiveBitrateDecisionPayload {
            generation,
            decision: adaptive_bitrate_decision_diagnostic(decision),
        })?;
    }
}

async fn run_media_feedback_session(
    app: AppHandle,
    generation: u64,
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    mut reports: tokio::sync::watch::Receiver<Option<AccumulatedMediaFeedback>>,
) {
    let (decision_tx, decision_rx) = tokio::sync::watch::channel(None);
    let writer = async {
        let mut last_written = None;
        loop {
            reports
                .changed()
                .await
                .map_err(|_| "adaptive feedback sender closed".to_string())?;
            let accumulated = *reports.borrow_and_update();
            let Some(accumulated) = accumulated else {
                continue;
            };
            let report = accumulated.report_since(last_written);
            tokio::time::timeout(
                CLIENT_MEDIA_FEEDBACK_IO_TIMEOUT,
                write_media_feedback_report_v1(&mut send, &report),
            )
            .await
            .map_err(|_| "adaptive feedback write timed out".to_string())?
            .map_err(|error| format!("adaptive feedback write failed: {error}"))?;
            last_written = Some(accumulated);
        }
        #[allow(unreachable_code)]
        Ok::<(), String>(())
    };
    let reader = async {
        let mut sequence = AdaptiveDecisionSequence::default();
        loop {
            let decision = read_adaptive_bitrate_decision_v1(&mut recv)
                .await
                .map_err(|error| format!("adaptive decision read failed: {error}"))?
                .ok_or_else(|| "adaptive decision stream closed".to_string())?;
            sequence.accept(&decision)?;
            decision_tx.send_replace(Some(decision));
        }
        #[allow(unreachable_code)]
        Ok::<(), String>(())
    };
    let decision_app = app.clone();
    let decision_emitter = emit_paced_adaptive_decisions(
        generation,
        decision_rx,
        CLIENT_ADAPTIVE_DECISION_DELIVERY_INTERVAL,
        move |payload| {
            decision_app
                .emit("adaptive-bitrate-decision", payload)
                .map_err(|error| format!("adaptive decision event delivery failed: {error}"))
        },
    );
    let terminal = tokio::select! {
        terminal = writer => terminal,
        terminal = reader => terminal,
        terminal = decision_emitter => terminal,
    };
    if let Err(error) = terminal {
        eprintln!("[client] {error}");
        if let Err(emit_error) = app.emit(
            "adaptive-feedback-state",
            serde_json::json!({
                "generation": generation,
                "available": false,
                "error": error,
            }),
        ) {
            eprintln!("[client] adaptive feedback terminal event delivery failed: {emit_error}");
        }
    }
    retire_media_feedback_generation(&app, generation).await;
}

async fn open_legacy_negotiated_media_stream(
    endpoint: &Endpoint,
    address: &iroh::EndpointAddr,
    nonce: [u8; 16],
    invitation: Option<&str>,
) -> Result<
    (
        iroh::endpoint::Connection,
        iroh::endpoint::RecvStream,
        Option<iroh::endpoint::SendStream>,
        NegotiatedV1Stream,
        MediaTransport,
    ),
    String,
> {
    match endpoint.connect(address.clone(), MEDIA_ALPN_V3).await {
        Ok(connection) => {
            let (mut send, mut recv) = connection
                .open_bi()
                .await
                .map_err(|error| format!("Failed to open media v3 handshake: {error}"))?;
            let negotiation = negotiate_v1(
                &mut send,
                &mut recv,
                nonce,
                vec![Capability::VideoH264],
                Some(Capability::VideoH264),
                "media v3",
                invitation,
            )
            .await?;
            Ok((
                connection,
                recv,
                Some(send),
                negotiation,
                MediaTransport::GroupedObjectsV3,
            ))
        }
        Err(v3_error) if connect_error_is_unsupported_alpn(&v3_error) => {
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
                        invitation,
                    )
                    .await?;
                    send.finish()
                        .map_err(|error| format!("Failed to finish media v2 handshake: {error}"))?;
                    Ok((
                        connection,
                        recv,
                        None,
                        negotiation,
                        MediaTransport::IndependentObjectsV2,
                    ))
                }
                Err(v2_error) if connect_error_is_unsupported_alpn(&v2_error) => {
                    let connection = endpoint
                    .connect(address.clone(), MEDIA_ALPN_V1)
                    .await
                    .map_err(|v1_error| {
                        format!(
                            "Failed to connect media v3 ({v3_error}); v2 compatibility connection failed ({v2_error}); v1 compatibility connection also failed ({v1_error})"
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
                        invitation,
                    )
                    .await?;
                    send.finish()
                        .map_err(|error| format!("Failed to finish media v1 handshake: {error}"))?;
                    Ok((
                        connection,
                        recv,
                        None,
                        negotiation,
                        MediaTransport::ReliableStreamV1,
                    ))
                }
                Err(v2_error) => Err(format!(
                    "Media v2 compatibility connection failed without an explicit unsupported-ALPN signal; refusing an unsafe downgrade to v1: {v2_error}"
                )),
            }
        }
        Err(v3_error) => Err(format!(
            "Media v3 connection failed without an explicit unsupported-ALPN signal; refusing an unsafe compatibility downgrade: {v3_error}"
        )),
    }
}

async fn open_negotiated_media_stream(
    endpoint: &Endpoint,
    address: &iroh::EndpointAddr,
    nonce: [u8; 16],
    invitation: Option<&str>,
) -> Result<
    (
        iroh::endpoint::Connection,
        iroh::endpoint::RecvStream,
        Option<iroh::endpoint::SendStream>,
        NegotiatedV1Stream,
        MediaTransport,
    ),
    String,
> {
    match endpoint.connect(address.clone(), CONTROL_ALPN_V1).await {
        Ok(connection) => {
            let (mut send, mut recv) = connection
                .open_bi()
                .await
                .map_err(|error| format!("Failed to open control handshake: {error}"))?;
            let negotiation = negotiate_v1(
                &mut send,
                &mut recv,
                nonce,
                vec![Capability::VideoH264],
                Some(Capability::VideoH264),
                "control",
                invitation,
            )
            .await?;
            // CONTROL owns the authenticated host lease. Keep both the
            // connection and the client->host send leg alive for keyframe
            // requests while media uses a separate upstream MoQ session.
            Ok((
                connection,
                recv,
                Some(send),
                negotiation,
                MediaTransport::UpstreamMoq,
            ))
        }
        Err(control_error) if connect_error_is_unsupported_alpn(&control_error) => {
            open_legacy_negotiated_media_stream(endpoint, address, nonce, invitation).await
        }
        Err(control_error) => Err(format!(
            "Control connection failed without an explicit unsupported-ALPN signal; refusing an unsafe media downgrade: {control_error}"
        )),
    }
}

async fn open_upstream_moq_media(
    endpoint: &Endpoint,
    address: &iroh::EndpointAddr,
    session_id: u64,
) -> Result<(MoqMediaReceiver, iroh::endpoint::Connection), String> {
    let broadcast_name = media_moq_broadcast_name(session_id)
        .map_err(|error| format!("Invalid MoQ media session name: {error}"))?;
    let moq = Moq::new(endpoint.clone());
    let mut session =
        tokio::time::timeout(CLIENT_MOQ_CONNECT_TIMEOUT, moq.connect(address.clone()))
            .await
            .map_err(|_| "Timed out connecting upstream MoQ media session".to_string())?
            .map_err(|error| format!("Failed to connect upstream MoQ media session: {error:#}"))?;
    if session.remote_id() != address.id {
        session.close(1, b"remote identity mismatch");
        return Err(format!(
            "Upstream MoQ connected to unexpected peer {}; expected {}",
            session.remote_id(),
            address.id
        ));
    }
    let diagnostics_connection = session.conn().clone();
    let broadcast = tokio::time::timeout(
        CLIENT_MOQ_SUBSCRIBE_TIMEOUT,
        session.subscribe(&broadcast_name),
    )
    .await
    .map_err(|_| format!("Timed out waiting for upstream MoQ broadcast {broadcast_name}"))?
    .map_err(|error| {
        format!("Failed to subscribe to upstream MoQ broadcast {broadcast_name}: {error}")
    })?;
    let catalog = subscribe_goq_video_track(&broadcast, CLIENT_MOQ_SUBSCRIBE_TIMEOUT).await?;
    eprintln!("[client] moq catalog: {}", catalog.mode.label());
    Ok((
        MoqMediaReceiver::new(moq, session, broadcast, catalog.track),
        diagnostics_connection,
    ))
}

async fn run_media_control_writer_v3(
    mut stream: iroh::endpoint::SendStream,
    mut requests: tokio::sync::mpsc::Receiver<(KeyframeRequestReasonV3, Option<u64>)>,
) {
    let mut request_id = 0_u64;
    while let Some((reason, last_sequence)) = requests.recv().await {
        let Some(next_request_id) = request_id.checked_add(1) else {
            eprintln!("[client] media v3 keyframe request id overflowed");
            break;
        };
        request_id = next_request_id;
        let request = MediaControlRequestV3::request_keyframe(request_id, last_sequence, reason);
        match tokio::time::timeout(
            Duration::from_secs(1),
            write_media_control_request_v3(&mut stream, &request),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!("[client] media v3 keyframe request failed: {error}");
                break;
            }
            Err(_) => {
                eprintln!("[client] media v3 keyframe request timed out");
                break;
            }
        }
    }
    let _ = stream.finish();
}

fn parse_keyframe_request_reason(reason: &str) -> Result<KeyframeRequestReasonV3, String> {
    match reason {
        "join" => Ok(KeyframeRequestReasonV3::Join),
        "transport-gap" => Ok(KeyframeRequestReasonV3::TransportGap),
        "delivery-timeout" => Ok(KeyframeRequestReasonV3::DeliveryTimeout),
        "discontinuity" | "decoder-reset" | "decoder-error" => {
            Ok(KeyframeRequestReasonV3::DecoderReset)
        }
        "frontend-backpressure" => Ok(KeyframeRequestReasonV3::FrontendBackpressure),
        _ => Err(format!("Unsupported keyframe request reason: {reason}")),
    }
}

fn try_queue_media_keyframe_request(
    sender: Option<&tokio::sync::mpsc::Sender<(KeyframeRequestReasonV3, Option<u64>)>>,
    reason: KeyframeRequestReasonV3,
    last_sequence: Option<u64>,
) {
    if let Some(sender) = sender {
        let _ = sender.try_send((reason, last_sequence));
    }
}

fn input_capability_offers(grants: InvitationGrants) -> Vec<Vec<Capability>> {
    let base = [
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
    ];
    let has_input_grant = grants.contains(InvitationGrants::POINTER_KEYBOARD)
        || grants.contains(InvitationGrants::GAMEPAD);
    let mut offers = Vec::with_capacity(base.len() * 2);
    if has_input_grant {
        for mut offer in base.clone() {
            offer.push(Capability::InputAck);
            offers.push(offer);
        }
    }
    offers.extend(base);
    for offer in &mut offers {
        offer.retain(|capability| match capability {
            Capability::Gamepad => grants.contains(InvitationGrants::GAMEPAD),
            Capability::AbsolutePointer
            | Capability::RelativePointer
            | Capability::Keyboard
            | Capability::Text
            | Capability::PointerPositionFeedback
            | Capability::PointerVisibilityFeedback => {
                grants.contains(InvitationGrants::POINTER_KEYBOARD)
            }
            Capability::InputAck => has_input_grant,
            _ => true,
        });
    }
    offers.dedup();
    offers
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
    diagnostics: Option<&Arc<StdMutex<NetworkSessionDiagnostics>>>,
) -> Result<(), String> {
    if let Some(diagnostics) = diagnostics {
        lock_network_diagnostics(diagnostics).begin_input_send(Instant::now());
    }
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

fn observe_input_ack_if_negotiated(
    diagnostics: &StdMutex<NetworkSessionDiagnostics>,
    negotiated: bool,
    sequence: u64,
    now: Instant,
) -> Result<(), String> {
    if !negotiated {
        return Ok(());
    }
    lock_network_diagnostics(diagnostics).observe_input_ack(sequence, now)
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
        None,
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

    let (client_secret, development_mode) = if let Some(node_id) = state.dev_connect_node_id {
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
        (SecretKey::generate(), true)
    } else {
        // FIDO2 derivation — 30s timeout so a missing/stuck key surfaces quickly.
        let client_secret = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::task::spawn_blocking(move || derive_iroh_secret_from_key(&pin)),
        )
        .await
        .map_err(|_| "Security key timed out (30s). Make sure your key is connected.".to_string())?
        .map_err(|e| format!("Task failed: {}", e))?
        .map_err(|e| format!("FIDO2 error: {:?}", e))?;

        // Key has been tapped — relay connection is next; update the UI overlay.
        let _ = app.emit("fido-done", ());
        (client_secret, false)
    };

    let (host_node_id, grants, invitation) = if development_mode {
        (
            state
                .dev_connect_node_id
                .ok_or_else(|| "Development host routing disappeared".to_string())?,
            InvitationGrants::ALL,
            None,
        )
    } else {
        let enrollment = connection_enrollment(&app, client_secret.public())?;
        (
            enrollment.host_node_id,
            enrollment.grants,
            enrollment.pending_invitation,
        )
    };
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

    // Public Sigil authorization exists only on the bounded, negotiated v1
    // protocols. The inherited v0 leg is retained below solely as migration
    // code and is no longer selected by an ordinary Portal connection.
    let use_v1 = true;
    let input_alpn = if use_v1 { INPUT_ALPN_V1 } else { INPUT_ALPN };

    let (frame_conn, mut frame_recv, media_control_stream, media_negotiation, media_transport) =
        if use_v1 {
            let first_attempt = open_negotiated_media_stream(
                &endpoint,
                &addr,
                handshake_nonce,
                invitation.as_deref(),
            )
            .await;
            let (connection, recv, control, negotiation, transport) = match first_attempt {
                Ok(result) => result,
                Err(invitation_error)
                    if invitation.is_some()
                        && invitation_error.contains("Portal peer is not authorized") =>
                {
                    // Recover only the narrow crash window where Sigil durably
                    // consumed the invitation but Portal did not durably clear it.
                    // The replay itself remains rejected; a second, ticket-free
                    // connection can succeed only as the already-enrolled Iroh
                    // peer authenticated by the exact invited host.
                    open_negotiated_media_stream(&endpoint, &addr, handshake_nonce, None)
                    .await
                    .map_err(|retry_error| {
                        format!(
                            "{invitation_error}; ticket-free enrollment recovery also failed: {retry_error}"
                        )
                    })?
                }
                Err(error) => return Err(error),
            };
            (connection, recv, control, Some(negotiation), transport)
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
            (connection, recv, None, None, MediaTransport::LegacyV0)
        };
    let media_session_id = media_negotiation
        .as_ref()
        .map(|negotiation| negotiation.session_id);
    let (upstream_moq_media, frame_connection_for_stats) = if media_transport
        == MediaTransport::UpstreamMoq
    {
        let session_id = media_session_id
            .ok_or_else(|| "Host omitted the control session ID required by MoQ".to_string())?;
        let (receiver, diagnostics_connection) =
            match open_upstream_moq_media(&endpoint, &addr, session_id).await {
                Ok(media) => media,
                Err(error) => {
                    // CONTROL already authenticated and owns the host's
                    // one-client lease. A post-auth MoQ failure is
                    // terminal, and must explicitly release that lease;
                    // it must never fall through to a legacy media ALPN.
                    frame_conn.close(1_u32.into(), b"upstream MoQ setup failed");
                    let _ =
                        tokio::time::timeout(CLIENT_ENDPOINT_CLOSE_TIMEOUT, endpoint.close()).await;
                    return Err(error);
                }
            };
        (Some(receiver), diagnostics_connection)
    } else {
        (None, frame_conn.clone())
    };
    let pointer_surface_dimensions = media_negotiation
        .as_ref()
        .and_then(|negotiation| negotiation.pointer_surface_dimensions);
    if !development_mode && let Some(expected_invitation) = invitation.as_deref() {
        // An accepted media hello means Sigil durably committed the one-time
        // enrollment before returning. Future PIN/tap/play sessions send no
        // bearer credential and authenticate by the stable Iroh peer instead.
        mark_invitation_redeemed(&app, expected_invitation)?;
    }

    // Feedback is a v3 sidecar for both the preferred upstream-MoQ transport
    // and the grouped-v3 compatibility path. Unsupported ALPN is normal
    // compatibility with older Sigil hosts; all other failures remain visible
    // diagnostics but never downgrade the authenticated media session.
    let (adaptive_feedback_stream, adaptive_feedback_error) = if media_transport
        .supports_adaptive_feedback()
    {
        match tokio::time::timeout(
            CLIENT_MEDIA_FEEDBACK_CONNECT_TIMEOUT,
            open_negotiated_feedback_stream(
                &endpoint,
                &addr,
                handshake_nonce,
                media_session_id.ok_or_else(|| "Media v3 omitted its session ID".to_string())?,
            ),
        )
        .await
        {
            Ok(Ok(stream)) => (stream, None),
            Ok(Err(error)) => {
                eprintln!("[client] adaptive feedback unavailable: {error}");
                (None, Some(error))
            }
            Err(_) => {
                let error = "Adaptive feedback negotiation timed out".to_string();
                eprintln!("[client] adaptive feedback unavailable: {error}");
                (None, Some(error))
            }
        }
    } else {
        (None, None)
    };

    let (input_connection, input_send, input_recv, input_capabilities) = if use_v1 {
        let mut errors = Vec::new();
        let mut accepted = None;
        // Older hosts reject unknown capability enum values. Try all four
        // pointer feature levels with ACK first, then repeat the exact legacy
        // offers without ACK so lack of diagnostics never forces absolute
        // pointer input prematurely.
        for capabilities in input_capability_offers(grants) {
            match open_negotiated_input_stream(&endpoint, &addr, handshake_nonce, capabilities)
                .await
            {
                Ok(result) => {
                    accepted = Some(result);
                    break;
                }
                Err(error) => errors.push(error),
            }
        }
        let (connection, send, recv, input_negotiation) = accepted
            .ok_or_else(|| format!("All input capability offers failed: {}", errors.join("; ")))?;
        if Some(input_negotiation.session_id) != media_session_id {
            return Err("Host returned mismatched media and input sessions".to_string());
        }
        (connection, send, recv, input_negotiation.capabilities)
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
            input_conn,
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
    let network_diagnostics = Arc::new(StdMutex::new(NetworkSessionDiagnostics::new(
        Instant::now(),
        input_availability.input_ack,
    )));

    if input_availability.pointer_position_feedback || input_availability.input_ack {
        let mut input_feedback = input_recv;
        let feedback_diagnostics = Arc::clone(&network_diagnostics);
        let mut pointer_feedback_enabled = input_availability.pointer_position_feedback;
        let input_ack_enabled = input_availability.input_ack;
        tokio::spawn(async move {
            let terminal_reason = loop {
                let response = match read_input_ack(&mut input_feedback).await {
                    Ok(Some(response)) => response,
                    Ok(None) => {
                        lock_network_diagnostics(&feedback_diagnostics)
                            .mark_input_feedback_closed();
                        break PointerFeedbackTerminalReason::Eof;
                    }
                    Err(error) => {
                        lock_network_diagnostics(&feedback_diagnostics)
                            .mark_input_feedback_malformed();
                        eprintln!("[client] invalid input feedback: {error}");
                        break PointerFeedbackTerminalReason::Malformed;
                    }
                };
                if let Err(error) = observe_input_ack_if_negotiated(
                    &feedback_diagnostics,
                    input_ack_enabled,
                    response.sequence,
                    Instant::now(),
                ) {
                    eprintln!("[client] invalid input acknowledgement: {error}");
                    break PointerFeedbackTerminalReason::Malformed;
                }
                if pointer_feedback_enabled
                    && pointer_channel
                        .send(PointerFeedbackPayload::Position {
                            sequence: response.sequence,
                            position: response.pointer_position,
                            pointer_visible: response.pointer_visible,
                        })
                        .is_err()
                {
                    // Losing the webview's pointer channel must not stop ACK
                    // draining and apply backpressure to host input.
                    pointer_feedback_enabled = false;
                    if !input_ack_enabled {
                        return;
                    }
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
            address: addr.clone(),
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
    let mut audio_connection_for_stats = None;
    let (audio_available, connected_audio_generation, audio_error) = match audio_result {
        Ok(connection) => {
            audio_connection_for_stats = Some(connection.clone());
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
    let adaptive_feedback_available = adaptive_feedback_stream.is_some();
    if let Some((connection, send, recv)) = adaptive_feedback_stream {
        let (feedback_tx, feedback_rx) = tokio::sync::watch::channel(None);
        let feedback_sender: MediaFeedbackSender = feedback_tx;
        *state.media_feedback.lock().await =
            Some((media_generation, connection, feedback_sender.clone()));
        tokio::spawn(run_media_feedback_session(
            app.clone(),
            media_generation,
            send,
            recv,
            feedback_rx,
        ));
    } else {
        *state.media_feedback.lock().await = None;
    }
    let media_control_requests = if let Some(control_stream) = media_control_stream {
        let (control_tx, control_rx) = tokio::sync::mpsc::channel(1);
        *state.media_control.lock().await = Some((media_generation, control_tx.clone()));
        tokio::spawn(run_media_control_writer_v3(control_stream, control_rx));
        if media_transport == MediaTransport::GroupedObjectsV3 {
            let _ = control_tx.try_send((KeyframeRequestReasonV3::Join, None));
        }
        Some(control_tx)
    } else {
        *state.media_control.lock().await = None;
        None
    };
    let frame_events_in_flight = Arc::new(AtomicUsize::new(0));
    *state.frame_delivery.lock().await =
        Some((media_generation, Arc::clone(&frame_events_in_flight)));

    // Input forwarder: absolute motion is latest-value state and may be
    // dropped at the 60 Hz boundary. Relative motion is displacement, so it
    // owns a separate accumulator and timer that coalesces rather than drops.
    let mut input_stream = input_send;
    let input_send_diagnostics = Arc::clone(&network_diagnostics);
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
                    if let Err(error) = write_client_input_event(
                        &mut input_stream,
                        &event,
                        use_v1,
                        Some(&input_send_diagnostics),
                    )
                    .await
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
                        if let Err(error) = write_client_input_event(
                            &mut input_stream,
                            &event,
                            use_v1,
                            Some(&input_send_diagnostics),
                        ).await {
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
                if let Err(error) = write_client_input_event(
                    &mut input_stream,
                    &relative_barrier,
                    use_v1,
                    Some(&input_send_diagnostics),
                )
                .await
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
            if let Err(error) = write_client_input_event(
                &mut input_stream,
                &event,
                use_v1,
                Some(&input_send_diagnostics),
            )
            .await
            {
                eprintln!("[client] input stream write failed: {error}; disconnecting");
                break;
            }
        }
        while let Some(event) = pending_relative.take() {
            if let Err(error) = write_client_input_event(
                &mut input_stream,
                &event,
                use_v1,
                Some(&input_send_diagnostics),
            )
            .await
            {
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
        let mut media_objects_v3 = (media_transport == MediaTransport::GroupedObjectsV3)
            .then(|| MediaObjectReceiverV3::new(frame_connection_for_stats.clone()));
        let mut media_object_sequence_v3 = MediaObjectSequenceV3::new();
        let mut upstream_moq_media = upstream_moq_media;

        let mut decoder = if use_webcodecs {
            None
        } else {
            match openh264::decoder::Decoder::new() {
                Ok(d) => Some(d),
                Err(e) => {
                    emit_frame_error(&app, media_generation, format!("Decoder init failed: {e}"));
                    if media_transport == MediaTransport::UpstreamMoq {
                        retire_upstream_moq_generation(
                            &app,
                            media_generation,
                            connected_audio_generation,
                        )
                        .await;
                    }
                    return;
                }
            }
        };

        let (stats_stop, mut stats_stop_rx) = tokio::sync::watch::channel(false);
        let stats_app = app.clone();
        let stats_metrics = Arc::clone(&metrics);
        let stats_in_flight = Arc::clone(&frame_events_in_flight);
        let stats_connection = frame_connection_for_stats.clone();
        let stats_input_connection = input_connection.clone();
        let stats_audio_connection = audio_connection_for_stats.clone();
        let stats_network_diagnostics = Arc::clone(&network_diagnostics);
        let stats_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(CLIENT_FRAME_STATS_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Tokio intervals tick immediately once. Consume that tick so the
            // first payload represents a full diagnostics interval.
            interval.tick().await;
            let mut last_path_sample = Instant::now() - Duration::from_secs(1);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if last_path_sample.elapsed() >= Duration::from_secs(1) {
                            let mut diagnostics = lock_network_diagnostics(&stats_network_diagnostics);
                            diagnostics.observe_connection(NetworkLeg::Media, &stats_connection);
                            diagnostics.observe_connection(NetworkLeg::Input, &stats_input_connection);
                            if let Some(audio_connection) = stats_audio_connection.as_ref() {
                                diagnostics.observe_connection(NetworkLeg::Audio, audio_connection);
                            }
                            last_path_sample = Instant::now();
                        }
                        let queue_depth = stats_in_flight.load(Ordering::SeqCst);
                        let network_snapshot = lock_network_diagnostics(&stats_network_diagnostics)
                            .snapshot(Instant::now());
                        let payload = lock_client_media_metrics(&stats_metrics).snapshot(
                            metrics_started.elapsed(),
                            queue_depth,
                            network_snapshot,
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
                codec_config,
            ) = match media_transport {
                MediaTransport::UpstreamMoq => {
                    let receiver = upstream_moq_media
                        .as_mut()
                        .expect("upstream MoQ receiver must exist for MoQ transport");
                    loop {
                        let outcome = match receiver.next().await {
                            Ok(Some(outcome)) => outcome,
                            Ok(None) => {
                                emit_frame_error(
                                    &app,
                                    media_generation,
                                    "Upstream MoQ video track closed",
                                );
                                break 'frames;
                            }
                            Err(error) => {
                                emit_frame_error(&app, media_generation, error);
                                break 'frames;
                            }
                        };
                        match outcome {
                            MoqMediaReadOutcome::Dropped { reason } => {
                                frontend_waiting_for_keyframe = true;
                                let mut metrics = lock_client_media_metrics(&metrics);
                                metrics.observe_transport_object_drop(false);
                                metrics.begin_frontend_resync(metrics_started.elapsed());
                                try_queue_media_keyframe_request(
                                    media_control_requests.as_ref(),
                                    reason,
                                    receiver.last_frame_sequence,
                                );
                            }
                            MoqMediaReadOutcome::Malformed(error) => {
                                emit_frame_error(&app, media_generation, error);
                                break 'frames;
                            }
                            MoqMediaReadOutcome::Frame {
                                frame,
                                discontinuity,
                            } => {
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
                                    frame.header.flags.contains(FrameFlags::CODEC_CONFIG),
                                );
                            }
                        }
                    }
                }
                MediaTransport::GroupedObjectsV3 => {
                    let receiver = media_objects_v3
                        .as_mut()
                        .expect("media v3 receiver must exist for media v3 transport");
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
                            MediaObjectReadOutcomeV3::Dropped { reason, .. } => {
                                let begins_resync = media_object_sequence_v3.note_dropped_object();
                                let mut metrics = lock_client_media_metrics(&metrics);
                                metrics.observe_transport_object_drop(!begins_resync);
                                if begins_resync {
                                    frontend_waiting_for_keyframe = true;
                                    metrics.begin_frontend_resync(metrics_started.elapsed());
                                    try_queue_media_keyframe_request(
                                        media_control_requests.as_ref(),
                                        reason,
                                        media_object_sequence_v3.last_sequence,
                                    );
                                }
                            }
                            MediaObjectReadOutcomeV3::Malformed(error) => {
                                emit_frame_error(&app, media_generation, error);
                                break 'frames;
                            }
                            MediaObjectReadOutcomeV3::Object { object, .. } => {
                                let was_waiting = media_object_sequence_v3.waiting_for_keyframe;
                                let discontinuity = match media_object_sequence_v3.classify(&object)
                                {
                                    MediaObjectSequenceDecisionV3::Deliver { discontinuity } => {
                                        discontinuity
                                    }
                                    MediaObjectSequenceDecisionV3::DropLate => {
                                        lock_client_media_metrics(&metrics)
                                            .observe_transport_object_drop(true);
                                        continue;
                                    }
                                    MediaObjectSequenceDecisionV3::DropUntilKeyframe => {
                                        frontend_waiting_for_keyframe = true;
                                        let mut metrics = lock_client_media_metrics(&metrics);
                                        metrics.observe_transport_object_drop(false);
                                        metrics.begin_frontend_resync(metrics_started.elapsed());
                                        if !was_waiting {
                                            try_queue_media_keyframe_request(
                                                media_control_requests.as_ref(),
                                                KeyframeRequestReasonV3::TransportGap,
                                                media_object_sequence_v3.last_sequence,
                                            );
                                        }
                                        continue;
                                    }
                                };
                                let codec = match object.header.codec {
                                    MediaCodec::H264 => "h264".to_string(),
                                };
                                break (
                                    u32::from(object.header.width),
                                    u32::from(object.header.height),
                                    object.payload,
                                    object.header.flags.contains(FrameFlags::KEYFRAME),
                                    codec,
                                    Some(object.header.sequence),
                                    Some(object.header.capture_timestamp_us),
                                    Some(object.header.pts_us),
                                    discontinuity,
                                    object.header.flags.contains(FrameFlags::CODEC_CONFIG),
                                );
                            }
                        }
                    }
                }
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
                                    frame.header.flags.contains(FrameFlags::CODEC_CONFIG),
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
                        frame.header.flags.contains(FrameFlags::CODEC_CONFIG),
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
                    (
                        w,
                        h,
                        frame_buf,
                        is_keyframe,
                        codec,
                        None,
                        None,
                        None,
                        false,
                        false,
                    )
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
                let begins_resync = !frontend_waiting_for_keyframe;
                frontend_waiting_for_keyframe = true;
                let mut metrics = lock_client_media_metrics(&metrics);
                metrics.observe_frontend_queue_drop();
                metrics.begin_frontend_resync(metrics_started.elapsed());
                if begins_resync {
                    try_queue_media_keyframe_request(
                        media_control_requests.as_ref(),
                        KeyframeRequestReasonV3::FrontendBackpressure,
                        sequence,
                    );
                }
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
                        codec_config,
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
        if media_transport == MediaTransport::UpstreamMoq {
            retire_upstream_moq_generation(&app, media_generation, connected_audio_generation)
                .await;
        }
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
        adaptive_feedback_available,
        adaptive_feedback_error,
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

    fn media_object_v3(
        group_id: u64,
        object_id: u32,
        sequence: u64,
        flags: FrameFlags,
    ) -> MediaObjectV3 {
        let payload = vec![sequence as u8];
        let header = sigil_protocol::MediaObjectHeaderV3::h264(
            1280,
            800,
            payload.len(),
            if object_id == 0 { 0 } else { 128 },
            flags,
            object_id,
            group_id,
            sequence,
            sequence * 1_000,
            sequence as i64 * 1_000,
            100,
        )
        .unwrap();
        MediaObjectV3::new(header, payload).unwrap()
    }

    fn media_object_outcome_v3(
        accept_index: u64,
        group_id: u64,
        object_id: u32,
        sequence: u64,
        flags: FrameFlags,
    ) -> MediaObjectReadOutcomeV3 {
        MediaObjectReadOutcomeV3::Object {
            accept_index,
            object: media_object_v3(group_id, object_id, sequence, flags),
        }
    }

    #[test]
    fn upstream_moq_group_ids_detect_only_real_transport_gaps() {
        assert!(!classify_moq_group_sequence(None, 41).unwrap());
        assert!(!classify_moq_group_sequence(Some(41), 42).unwrap());
        assert!(classify_moq_group_sequence(Some(42), 44).unwrap());
        assert!(classify_moq_group_sequence(Some(44), 44).is_err());
        assert!(classify_moq_group_sequence(Some(44), 43).is_err());
    }

    #[test]
    fn upstream_moq_group_gap_keyframe_is_the_recovery_barrier() {
        let keyframe = media_object_frame(80, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        let contiguous = validate_moq_group_frame(9, true, Some(42), &keyframe).unwrap();
        assert!(!contiguous);

        // A native group-id gap puts the decoder into resync, but this exact
        // configured frame 0 exits it immediately. There is deliberately no
        // keyframe-request action on MoqMediaReadOutcome::Frame: requesting a
        // replacement here would recreate the grouped-v3 feedback loop.
        let group_gap = classify_moq_group_sequence(Some(7), 9).unwrap();
        let discontinuity = group_gap || !contiguous;
        assert!(discontinuity);
    }

    #[tokio::test]
    async fn upstream_moq_idr_abort_to_next_frame_zero_has_no_feedback_loop() {
        let mut producer = Track::new(MOQ_VIDEO_H264_TRACK).produce();
        let mut receiver = MoqMediaReceiver::for_test(producer.consume());

        let mut prior_group = producer.append_group().unwrap();
        let prior_keyframe =
            media_object_frame(40, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        prior_group
            .write_frame(sigil_protocol::encode_media_frame_object(&prior_keyframe).unwrap())
            .unwrap();
        assert!(matches!(
            receiver.next().await.unwrap(),
            Some(MoqMediaReadOutcome::Frame { .. })
        ));

        prior_group.abort(moq_net::Error::Cancel).unwrap();
        let mut replacement = producer.append_group().unwrap();
        let replacement_keyframe =
            media_object_frame(80, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        replacement
            .write_frame(sigil_protocol::encode_media_frame_object(&replacement_keyframe).unwrap())
            .unwrap();
        replacement.finish().unwrap();

        // `next` consumes the expected Cancel internally and returns the
        // replacement frame directly. A Dropped outcome here would enqueue a
        // needless keyframe request and recreate the recovery feedback loop.
        assert!(matches!(
            receiver.next().await.unwrap(),
            Some(MoqMediaReadOutcome::Frame {
                discontinuity: true,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn upstream_moq_expected_cancel_without_replacement_is_terminal_without_request() {
        let mut producer = Track::new(MOQ_VIDEO_H264_TRACK).produce();
        let mut receiver = MoqMediaReceiver::for_test(producer.consume());
        let mut group = producer.append_group().unwrap();
        let keyframe = media_object_frame(1, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        group
            .write_frame(sigil_protocol::encode_media_frame_object(&keyframe).unwrap())
            .unwrap();
        assert!(matches!(
            receiver.next().await.unwrap(),
            Some(MoqMediaReadOutcome::Frame { .. })
        ));
        group.abort(moq_net::Error::Cancel).unwrap();

        let error = receiver
            .next_with_timeouts(Duration::from_millis(25), Duration::from_millis(25))
            .await
            .unwrap_err();
        assert!(error.contains("expected GOP cancellation"));
    }

    #[tokio::test]
    async fn upstream_moq_partial_object_read_has_protocol_bounded_deadline() {
        let mut producer = Track::new(MOQ_VIDEO_H264_TRACK).produce();
        let mut receiver = MoqMediaReceiver::for_test(producer.consume());
        let _stalled_group = producer.append_group().unwrap();

        assert!(matches!(
            receiver
                .next_with_timeouts(Duration::from_millis(25), Duration::from_millis(25))
                .await
                .unwrap(),
            Some(MoqMediaReadOutcome::Dropped {
                reason: KeyframeRequestReasonV3::DeliveryTimeout
            })
        ));
        assert_eq!(
            CLIENT_MOQ_OBJECT_READ_TIMEOUT,
            Duration::from_millis(MAX_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS as u64)
        );
        assert!(CLIENT_MOQ_GROUP_RECOVERY_TIMEOUT >= Duration::from_millis(1_000));
    }

    #[tokio::test]
    async fn upstream_moq_invalid_cancel_replacement_requests_recovery() {
        let mut producer = Track::new(MOQ_VIDEO_H264_TRACK).produce();
        let mut receiver = MoqMediaReceiver::for_test(producer.consume());
        let mut prior = producer.append_group().unwrap();
        let keyframe = media_object_frame(10, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        prior
            .write_frame(sigil_protocol::encode_media_frame_object(&keyframe).unwrap())
            .unwrap();
        assert!(matches!(
            receiver.next().await.unwrap(),
            Some(MoqMediaReadOutcome::Frame { .. })
        ));
        prior.abort(moq_net::Error::Cancel).unwrap();

        let mut invalid_replacement = producer.append_group().unwrap();
        let delta = media_object_frame(11, FrameFlags::NONE);
        invalid_replacement
            .write_frame(sigil_protocol::encode_media_frame_object(&delta).unwrap())
            .unwrap();
        invalid_replacement.finish().unwrap();
        assert!(matches!(
            receiver.next().await.unwrap(),
            Some(MoqMediaReadOutcome::Dropped {
                reason: KeyframeRequestReasonV3::TransportGap
            })
        ));
    }

    #[test]
    fn upstream_moq_requires_configured_keyframe_zero_and_contiguous_deltas() {
        let delta = media_object_frame(10, FrameFlags::NONE);
        assert!(validate_moq_group_frame(3, true, None, &delta).is_err());

        let unconfigured_keyframe = media_object_frame(10, FrameFlags::KEYFRAME);
        assert!(validate_moq_group_frame(3, true, None, &unconfigured_keyframe).is_err());

        let configured =
            media_object_frame(10, FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG));
        assert!(validate_moq_group_frame(3, true, None, &configured).is_ok());
        assert!(validate_moq_group_frame(3, false, Some(8), &delta).is_err());
        assert!(validate_moq_group_frame(3, false, Some(9), &delta).is_ok());
    }

    #[test]
    fn upstream_moq_bounds_objects_and_group_bytes_before_growth() {
        assert_eq!(validate_moq_object_bounds(1, 0, 0, 40).unwrap(), 40);
        assert!(validate_moq_object_bounds(1, MAX_MEDIA_OBJECT_ID_V3 as usize + 1, 0, 1,).is_err());
        assert!(validate_moq_object_bounds(1, 0, MAX_MEDIA_GROUP_BYTES_V3, 1).is_err());
        assert!(validate_moq_object_bounds(1, 0, usize::MAX, 1).is_err());
    }

    #[test]
    fn upstream_moq_cancellation_is_resync_but_protocol_errors_are_terminal() {
        for recoverable in [
            moq_net::Error::Cancel,
            moq_net::Error::Old,
            moq_net::Error::Timeout,
            moq_net::Error::Dropped,
            moq_net::Error::CacheFull,
            moq_net::Error::Remote(3),
        ] {
            assert!(moq_group_error_is_recoverable(&recoverable));
        }
        assert!(!moq_group_error_is_recoverable(
            &moq_net::Error::ProtocolViolation
        ));
        assert!(!moq_group_error_is_recoverable(&moq_net::Error::WrongSize));
        assert_eq!(
            moq_group_error_reason(&moq_net::Error::Timeout),
            KeyframeRequestReasonV3::DeliveryTimeout
        );
        assert_eq!(
            moq_group_error_reason(&moq_net::Error::Cancel),
            KeyframeRequestReasonV3::TransportGap
        );
    }

    #[test]
    fn upstream_moq_transport_is_distinct_from_legacy_compatibility() {
        assert_eq!(MediaTransport::UpstreamMoq.diagnostic_name(), "iroh-moq");
        assert!(MediaTransport::UpstreamMoq.supports_adaptive_feedback());
        assert!(MediaTransport::GroupedObjectsV3.supports_adaptive_feedback());
        assert!(!MediaTransport::IndependentObjectsV2.supports_adaptive_feedback());
        assert_ne!(
            MediaTransport::UpstreamMoq,
            MediaTransport::GroupedObjectsV3
        );
        assert!(decode_media_frame_object(&[0_u8; 8]).is_err());
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
    fn media_v3_groups_use_wire_object_identity_and_recover_on_new_group_zero() {
        let keyframe = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let mut sequence = MediaObjectSequenceV3::new();

        assert_eq!(
            sequence.classify(&media_object_v3(10, 0, 10, keyframe)),
            MediaObjectSequenceDecisionV3::Deliver {
                discontinuity: true
            }
        );
        assert_eq!(
            sequence.classify(&media_object_v3(10, 1, 11, FrameFlags::NONE)),
            MediaObjectSequenceDecisionV3::Deliver {
                discontinuity: false
            }
        );
        assert_eq!(
            sequence.classify(&media_object_v3(10, 3, 13, FrameFlags::NONE)),
            MediaObjectSequenceDecisionV3::DropUntilKeyframe
        );
        assert_eq!(
            sequence.classify(&media_object_v3(10, 2, 12, FrameFlags::NONE)),
            MediaObjectSequenceDecisionV3::DropUntilKeyframe
        );
        assert_eq!(
            sequence.classify(&media_object_v3(
                20,
                0,
                20,
                keyframe.union(FrameFlags::DISCONTINUITY),
            )),
            MediaObjectSequenceDecisionV3::Deliver {
                discontinuity: true
            }
        );
        assert_eq!(
            sequence.classify(&media_object_v3(10, 2, 12, FrameFlags::NONE)),
            MediaObjectSequenceDecisionV3::DropLate
        );
    }

    #[test]
    fn late_dropped_completion_after_v3_barrier_cannot_poison_recovered_group() {
        let barrier = FrameFlags::KEYFRAME
            .union(FrameFlags::CODEC_CONFIG)
            .union(FrameFlags::DISCONTINUITY);
        let mut reorder = MediaObjectReorderV3::new(1);
        let mut sequence = MediaObjectSequenceV3::new();

        let recovered = reorder
            .push(media_object_outcome_v3(5, 20, 0, 20, barrier))
            .unwrap()
            .unwrap();
        let MediaObjectReadOutcomeV3::Object { object, .. } = recovered else {
            panic!("recovery barrier must remain an object");
        };
        assert_eq!(
            sequence.classify(&object),
            MediaObjectSequenceDecisionV3::Deliver {
                discontinuity: true
            }
        );

        assert!(
            reorder
                .push(MediaObjectReadOutcomeV3::Dropped {
                    accept_index: 1,
                    reason: KeyframeRequestReasonV3::DeliveryTimeout,
                })
                .unwrap()
                .is_none()
        );

        let delta = reorder
            .push(media_object_outcome_v3(6, 20, 1, 21, FrameFlags::NONE))
            .unwrap()
            .unwrap();
        let MediaObjectReadOutcomeV3::Object { object, .. } = delta else {
            panic!("new group delta must remain an object");
        };
        assert_eq!(
            sequence.classify(&object),
            MediaObjectSequenceDecisionV3::Deliver {
                discontinuity: false
            }
        );
    }

    #[test]
    fn media_v3_receiver_rejects_group_payload_growth_beyond_the_shared_cap() {
        let keyframe = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let mut sequence = MediaObjectSequenceV3::new();
        assert!(matches!(
            sequence.classify(&media_object_v3(1, 0, 1, keyframe)),
            MediaObjectSequenceDecisionV3::Deliver { .. }
        ));
        sequence.group_payload_bytes = MAX_MEDIA_GROUP_BYTES_V3;
        assert_eq!(
            sequence.classify(&media_object_v3(1, 1, 2, FrameFlags::NONE)),
            MediaObjectSequenceDecisionV3::DropUntilKeyframe
        );
    }

    #[test]
    fn keyframe_request_reason_mapping_is_strict_and_coalesces_decoder_reasons() {
        assert_eq!(
            parse_keyframe_request_reason("transport-gap").unwrap(),
            KeyframeRequestReasonV3::TransportGap
        );
        assert_eq!(
            parse_keyframe_request_reason("discontinuity").unwrap(),
            KeyframeRequestReasonV3::DecoderReset
        );
        assert_eq!(
            parse_keyframe_request_reason("decoder-error").unwrap(),
            KeyframeRequestReasonV3::DecoderReset
        );
        assert!(parse_keyframe_request_reason("please").is_err());
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
                codec_config: true,
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
        assert_eq!(envelope[6], 0b111);
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
                codec_config: false,
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
            codec_config: false,
            sequence: None,
            capture_timestamp_micros: None,
            pts_micros: None,
        };
        assert!(encode_frame_envelope(metadata("vp9"), &[1]).is_err());
        assert!(encode_frame_envelope(metadata("h264"), &[]).is_err());
        assert!(
            encode_frame_envelope(
                FrameEnvelopeMetadata {
                    codec_config: true,
                    ..metadata("h264")
                },
                &[1]
            )
            .is_err()
        );
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

    fn client_feedback() -> ClientMediaFeedbackReport {
        ClientMediaFeedbackReport {
            interval_ms: 1_000,
            last_sequence: Some(99),
            transport_dropped_delta: 1,
            frontend_dropped_delta: 2,
            decoder_dropped_delta: 3,
            presenter_dropped_delta: 4,
            frontend_queue_depth: 1,
            frontend_queue_capacity: 4,
            decode_queue_depth: 1,
            decode_queue_capacity: 2,
            presenter_queue_depth: 0,
            presenter_queue_capacity: 2,
            transport_delivery_p95_ms: Some(17.4),
            decode_p95_ms: Some(4.6),
            presentation_p95_ms: None,
            resync_active: true,
        }
    }

    #[test]
    fn adaptive_feedback_conversion_is_bounded_and_protocol_valid() {
        let report = media_feedback_report(7, client_feedback()).unwrap();
        assert_eq!(report.report_id, 7);
        assert_eq!(report.interval_ms, 1_000);
        assert_eq!(report.last_sequence, Some(99));
        assert_eq!(report.transport_delivery_p95_ms, Some(17));
        assert_eq!(report.decode_p95_ms, Some(5));
        assert!(report.flags.contains(MediaFeedbackFlags::RESYNC_ACTIVE));
        report.validate().unwrap();

        let mut invalid = client_feedback();
        invalid.decode_queue_depth = 3;
        assert!(media_feedback_report(8, invalid).is_err());
        let mut invalid_interval = client_feedback();
        invalid_interval.interval_ms = 249;
        assert!(media_feedback_report(9, invalid_interval).is_err());
        assert!(feedback_latency_ms(Some(f64::INFINITY), "decode").is_err());
        assert!(feedback_latency_ms(Some(60_001.0), "decode").is_err());
    }

    #[test]
    fn feedback_report_ids_are_nonzero_monotonic_and_checked() {
        let counter = AtomicU64::new(0);
        assert_eq!(next_feedback_report_id(&counter).unwrap(), 1);
        assert_eq!(next_feedback_report_id(&counter).unwrap(), 2);
        assert!(next_feedback_report_id(&AtomicU64::new(u64::MAX)).is_err());
    }

    #[tokio::test]
    async fn adaptive_feedback_watch_coalesces_latest_state_without_losing_pressure() {
        let (sender, mut receiver) = tokio::sync::watch::channel(None);
        let first = media_feedback_report(1, client_feedback()).unwrap();
        sender.send_replace(Some(AccumulatedMediaFeedback::new(first)));
        receiver.changed().await.unwrap();
        let stalled_write = receiver.borrow_and_update().unwrap();

        let mut latest = client_feedback();
        latest.interval_ms = 1_250;
        latest.transport_dropped_delta = 9;
        let second = media_feedback_report(2, latest).unwrap();
        sender.send_modify(|pending| pending.as_mut().unwrap().merge(second));
        let mut newest = client_feedback();
        newest.interval_ms = 1_500;
        newest.transport_dropped_delta = 11;
        newest.frontend_queue_depth = 3;
        let third = media_feedback_report(3, newest).unwrap();
        sender.send_modify(|pending| pending.as_mut().unwrap().merge(third));

        assert_eq!(stalled_write.report_since(None), first);
        receiver.changed().await.unwrap();
        let coalesced = receiver
            .borrow_and_update()
            .unwrap()
            .report_since(Some(stalled_write));
        assert_eq!(coalesced.report_id, 3);
        assert_eq!(coalesced.interval_ms, 2_750);
        assert_eq!(coalesced.transport_dropped_delta, 20);
        assert_eq!(coalesced.frontend_queue_depth, 3);
    }

    #[test]
    fn adaptive_decision_diagnostics_are_explicitly_shadow_state() {
        let diagnostic = adaptive_bitrate_decision_diagnostic(adaptive_decision(2, 1));
        assert_eq!(diagnostic.state, "decrease");
        assert_eq!(diagnostic.reasons, vec!["receiver-queue", "decode-backlog"]);
        assert!(!diagnostic.applied);
    }

    fn adaptive_decision(decision_id: u64, report_id: u64) -> AdaptiveBitrateDecisionV1 {
        AdaptiveBitrateDecisionV1 {
            decision_id,
            report_id,
            target_kbps: 8_000,
            floor_kbps: 4_000,
            ceiling_kbps: 20_000,
            state: AdaptiveBitrateStateV1::Decrease,
            reasons: AdaptiveBitrateReasonFlagsV1::RECEIVER_QUEUE
                .union(AdaptiveBitrateReasonFlagsV1::DECODE_BACKLOG),
            applied: false,
        }
    }

    #[test]
    fn adaptive_decision_sequence_rejects_duplicate_or_regressing_ids() {
        let mut sequence = AdaptiveDecisionSequence::default();
        sequence.accept(&adaptive_decision(1, 10)).unwrap();
        sequence.accept(&adaptive_decision(3, 12)).unwrap();
        assert!(sequence.accept(&adaptive_decision(3, 13)).is_err());

        let mut report_regression = AdaptiveDecisionSequence::default();
        report_regression.accept(&adaptive_decision(1, 10)).unwrap();
        assert!(report_regression.accept(&adaptive_decision(2, 10)).is_err());
        assert!(report_regression.accept(&adaptive_decision(3, 9)).is_err());
    }

    #[tokio::test]
    async fn adaptive_decision_delivery_is_paced_latest_value_and_emit_failure_is_terminal() {
        let (sender, receiver) = tokio::sync::watch::channel(None);
        let (delivered_tx, mut delivered_rx) = tokio::sync::mpsc::channel(2);
        let delivery = tokio::spawn(emit_paced_adaptive_decisions(
            7,
            receiver,
            Duration::from_millis(20),
            move |payload| {
                delivered_tx
                    .try_send(payload)
                    .map_err(|error| format!("test delivery failed: {error}"))
            },
        ));
        sender.send_replace(Some(adaptive_decision(1, 1)));
        sender.send_replace(Some(adaptive_decision(2, 2)));
        let first = tokio::time::timeout(Duration::from_millis(100), delivered_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.generation, 7);
        assert_eq!(first.decision.decision_id, 2);

        sender.send_replace(Some(adaptive_decision(3, 3)));
        assert!(
            tokio::time::timeout(Duration::from_millis(5), delivered_rx.recv())
                .await
                .is_err()
        );
        let second = tokio::time::timeout(Duration::from_millis(100), delivered_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.decision.decision_id, 3);
        delivery.abort();

        let (sender, receiver) = tokio::sync::watch::channel(None);
        let terminal = tokio::spawn(emit_paced_adaptive_decisions(
            7,
            receiver,
            Duration::from_millis(1),
            |_payload| Err("webview channel closed".to_string()),
        ));
        sender.send_replace(Some(adaptive_decision(4, 4)));
        let error = tokio::time::timeout(Duration::from_millis(100), terminal)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error, "webview channel closed");
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
    fn generation_owned_retirement_cannot_take_a_replacement_session() {
        let mut slot = Some((12, "replacement"));
        assert_eq!(take_generation_owned(&mut slot, 11), None);
        assert_eq!(slot, Some((12, "replacement")));
        assert_eq!(take_generation_owned(&mut slot, 12), Some("replacement"));
        assert_eq!(slot, None);

        let mut feedback = Some((22, "replacement connection", "replacement sender"));
        assert_eq!(take_generation_owned_triple(&mut feedback, 21), None);
        assert_eq!(
            feedback,
            Some((22, "replacement connection", "replacement sender"))
        );
        assert_eq!(
            take_generation_owned_triple(&mut feedback, 22),
            Some(("replacement connection", "replacement sender"))
        );
        assert_eq!(feedback, None);
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

        let payload = metrics.snapshot(
            Duration::from_millis(250),
            2,
            NetworkDiagnosticsSnapshot::test_fixture(
                PathMode::Direct,
                Some(Duration::from_millis(8)),
            ),
            9,
        );
        let json = serde_json::to_value(payload).unwrap();

        assert_eq!(json["generation"], 9);
        assert_eq!(json["stats_version"], 4);
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
        assert_eq!(json["path_rtt_ms"], 8.0);
        assert_eq!(json["network_diagnostics"]["version"], 1);

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

        let active = metrics.snapshot(
            Duration::from_millis(75),
            4,
            NetworkDiagnosticsSnapshot::test_fixture(PathMode::Direct, None),
            1,
        );
        assert_eq!(active.generation, 1);
        assert_eq!(active.frontend_resync_episode_total, 1);
        assert!(active.frontend_resync_active);
        assert_eq!(active.frontend_resync_duration_ms_total, 75.0);
        assert_eq!(active.frontend_resync_duration_ms_current, Some(75.0));
        assert_eq!(active.frontend_resync_duration_ms_max, 75.0);
        assert_eq!(active.frontend_resync_dropped_total, 1);

        metrics.finish_frontend_resync(Duration::from_millis(100));
        let completed = metrics.snapshot(
            Duration::from_millis(150),
            0,
            NetworkDiagnosticsSnapshot::test_fixture(PathMode::Direct, None),
            1,
        );
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

        let active = metrics.snapshot(
            Duration::from_secs(1),
            0,
            NetworkDiagnosticsSnapshot::test_fixture(PathMode::Relay, None),
            1,
        );
        assert!((active.transport_receive_fps - 60.0).abs() < 0.001);
        assert!((active.frontend_send_fps - 60.0).abs() < 0.001);

        let idle = metrics.snapshot(
            Duration::from_secs(3),
            0,
            NetworkDiagnosticsSnapshot::test_fixture(PathMode::Relay, None),
            1,
        );
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
                input_ack: false,
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
                input_ack: false,
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
                input_ack: false,
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
                input_ack: false,
                control: true,
            }
        );
        assert!(
            InputAvailability::from_capabilities(&[Capability::Gamepad, Capability::InputAck])
                .input_ack
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
                input_ack: false,
                control: true,
            }
        );
    }

    #[test]
    fn pointer_feedback_without_input_ack_does_not_fail_ack_validation() {
        let diagnostics = StdMutex::new(NetworkSessionDiagnostics::new(Instant::now(), false));
        assert!(observe_input_ack_if_negotiated(&diagnostics, false, 0, Instant::now()).is_ok());
    }

    #[test]
    fn input_capability_fallbacks_remove_only_one_protocol_extension_at_a_time() {
        let offers = input_capability_offers(InvitationGrants::ALL);
        assert_eq!(offers.len(), 8);
        let visibility = &offers[0];
        let position = &offers[1];
        let relative = &offers[2];
        let inherited = &offers[3];

        assert!(
            offers[..4]
                .iter()
                .all(|offer| offer.contains(&Capability::InputAck))
        );
        assert!(
            offers[4..]
                .iter()
                .all(|offer| !offer.contains(&Capability::InputAck))
        );
        assert!(visibility.contains(&Capability::PointerVisibilityFeedback));
        assert!(visibility.contains(&Capability::PointerPositionFeedback));
        assert!(!position.contains(&Capability::PointerVisibilityFeedback));
        assert!(position.contains(&Capability::PointerPositionFeedback));
        assert!(!relative.contains(&Capability::PointerVisibilityFeedback));
        assert!(!relative.contains(&Capability::PointerPositionFeedback));
        assert!(relative.contains(&Capability::RelativePointer));
        assert!(!inherited.contains(&Capability::RelativePointer));
        assert_eq!(
            inherited.as_slice(),
            &[
                Capability::AbsolutePointer,
                Capability::Keyboard,
                Capability::Text,
                Capability::Gamepad,
                Capability::InputAck,
            ]
        );
    }

    #[test]
    fn compatibility_downgrade_requires_tls_no_application_protocol() {
        let unsupported = iroh::endpoint::ConnectionError::ConnectionClosed(
            iroh::endpoint::TransportError::new(
                iroh::endpoint::TransportErrorCode::crypto(0x78),
                "no application protocol".to_string(),
            )
            .into(),
        );
        assert!(connection_error_is_unsupported_alpn(&unsupported));
        assert!(!connection_error_is_unsupported_alpn(
            &iroh::endpoint::ConnectionError::TimedOut
        ));
        assert!(!connection_error_is_unsupported_alpn(
            &iroh::endpoint::ConnectionError::Reset
        ));
    }

    #[test]
    fn local_invitation_grants_bound_input_offers_before_the_host_intersection() {
        let view_only = input_capability_offers(InvitationGrants::VIEW);
        assert!(view_only.iter().all(Vec::is_empty));

        let pointer = input_capability_offers(
            InvitationGrants::VIEW.union(InvitationGrants::POINTER_KEYBOARD),
        );
        assert!(pointer[0].contains(&Capability::Keyboard));
        assert!(pointer[0].contains(&Capability::InputAck));
        assert!(!pointer[0].contains(&Capability::Gamepad));

        let gamepad =
            input_capability_offers(InvitationGrants::VIEW.union(InvitationGrants::GAMEPAD));
        assert_eq!(gamepad[0], vec![Capability::Gamepad, Capability::InputAck]);
        assert_eq!(gamepad[1], vec![Capability::Gamepad]);
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
pub async fn iroh_client_request_keyframe(
    state: State<'_, AppState>,
    generation: u64,
    reason: String,
) -> Result<bool, String> {
    let reason = parse_keyframe_request_reason(&reason)?;
    let control = state.media_control.lock().await;
    let Some((current_generation, sender)) = control.as_ref() else {
        return Ok(false);
    };
    if *current_generation != generation {
        return Ok(false);
    }
    match sender.try_send((reason, None)) {
        Ok(()) => Ok(true),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(false),
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            Err("Media control channel closed".to_string())
        }
    }
}

fn feedback_latency_ms(value: Option<f64>, name: &str) -> Result<Option<u16>, String> {
    value
        .map(|value| {
            if !value.is_finite() || !(0.0..=60_000.0).contains(&value) {
                return Err(format!("{name} must be finite and between 0 and 60000 ms"));
            }
            Ok(value.round() as u16)
        })
        .transpose()
}

fn next_feedback_report_id(counter: &AtomicU64) -> Result<u64, String> {
    counter
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            current.checked_add(1)
        })
        .map(|previous| previous + 1)
        .map_err(|_| "Adaptive feedback report ID overflowed".to_string())
}

fn media_feedback_report(
    report_id: u64,
    input: ClientMediaFeedbackReport,
) -> Result<MediaFeedbackReportV1, String> {
    let flags = if input.resync_active {
        MediaFeedbackFlags::RESYNC_ACTIVE
    } else {
        MediaFeedbackFlags::NONE
    };
    let report = MediaFeedbackReportV1 {
        report_id,
        interval_ms: input.interval_ms,
        flags,
        last_sequence: input.last_sequence,
        transport_dropped_delta: input.transport_dropped_delta,
        frontend_dropped_delta: input.frontend_dropped_delta,
        decoder_dropped_delta: input.decoder_dropped_delta,
        presenter_dropped_delta: input.presenter_dropped_delta,
        frontend_queue_depth: input.frontend_queue_depth,
        frontend_queue_capacity: input.frontend_queue_capacity,
        decode_queue_depth: input.decode_queue_depth,
        decode_queue_capacity: input.decode_queue_capacity,
        presenter_queue_depth: input.presenter_queue_depth,
        presenter_queue_capacity: input.presenter_queue_capacity,
        transport_delivery_p95_ms: feedback_latency_ms(
            input.transport_delivery_p95_ms,
            "delivery p95",
        )?,
        decode_p95_ms: feedback_latency_ms(input.decode_p95_ms, "decode p95")?,
        presentation_p95_ms: feedback_latency_ms(input.presentation_p95_ms, "presentation p95")?,
    };
    report
        .validate()
        .map_err(|error| format!("Invalid adaptive feedback report: {error}"))?;
    Ok(report)
}

#[tauri::command]
pub async fn iroh_client_send_media_feedback(
    state: State<'_, AppState>,
    generation: u64,
    report: ClientMediaFeedbackReport,
) -> Result<bool, String> {
    let feedback = state.media_feedback.lock().await;
    let Some((current_generation, _connection, sender)) = feedback.as_ref() else {
        return Ok(false);
    };
    if *current_generation != generation {
        return Ok(false);
    }
    if sender.receiver_count() == 0 {
        return Err("Adaptive feedback channel closed".to_string());
    }
    let report_id = next_feedback_report_id(&state.media_feedback_report_id)?;
    let report = media_feedback_report(report_id, report)?;
    sender.send_modify(|pending| match pending {
        Some(pending) => pending.merge(report),
        None => *pending = Some(AccumulatedMediaFeedback::new(report)),
    });
    Ok(true)
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
    *state.media_control.lock().await = None;
    if let Some((_generation, connection, _sender)) = state.media_feedback.lock().await.take() {
        connection.close(0_u32.into(), b"client disconnected");
    }
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
