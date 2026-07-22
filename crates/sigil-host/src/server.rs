use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use iroh::EndpointId;
use iroh::endpoint::{Connection, SendStream};
use iroh::protocol::ProtocolHandler;
use moq_net::{
    Broadcast, BroadcastConsumer, BroadcastProducer, Error as MoqError, GroupProducer, Origin,
    Track, TrackProducer,
};
use sigil_protocol::{
    AUDIO_HEADER_LEN, AdaptiveBitrateDecisionV1, AdaptiveBitrateReasonFlagsV1,
    AdaptiveBitrateStateV1, AudioFlags, AudioPacket, AudioPacketHeader, Capability, ClientHello,
    FrameFlags, HostHello, InputAck, InvitationGrants, KeyframeRequestReasonV3,
    MAX_AUDIO_PAYLOAD_LEN, MAX_MEDIA_GROUP_BYTES_V3, MAX_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS,
    MAX_MEDIA_OBJECT_ID_V3, MIN_MEDIA_OBJECT_DELIVERY_TIMEOUT_MS, MOQ_VIDEO_H264_TRACK,
    MediaControlRequestV3, MediaFeedbackFlags, MediaFeedbackReportV1, MediaFrame, MediaFrameHeader,
    MediaObjectHeaderV3, MediaObjectV3, encode_media_frame_object, media_moq_broadcast_name,
    read_client_hello, read_input_event, read_media_control_request_v3,
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
const MOQ_VIDEO_TRACK_PRIORITY: u8 = u8::MAX;
const MOQ_REJECT_CODE: u32 = 0x534d;
const ADAPTIVE_BITRATE_FLOOR_KBPS: u32 = 1_000;
const TEST_PATTERN_BITRATE_CEILING_KBPS: u32 = 12_000;
const ADAPTIVE_BITRATE_QUANTUM_KBPS: u32 = 250;
const ADAPTIVE_BITRATE_CLEAN_WINDOWS: u8 = 10;
const ADAPTIVE_BITRATE_MODERATE_WINDOWS: u8 = 2;
const ADAPTIVE_BITRATE_COOLDOWN_WINDOWS: u8 = 10;
const FEEDBACK_FRESH_INTERVAL_MIN_MS: u16 = 750;
const FEEDBACK_FRESH_INTERVAL_MAX_MS: u16 = 1_500;
const FEEDBACK_FRESH_ARRIVAL_GAP_MIN: Duration = Duration::from_millis(750);
const FEEDBACK_STALE_ARRIVAL_GAP: Duration = Duration::from_millis(2_500);
const FEEDBACK_MIN_READ_INTERVAL: Duration = Duration::from_millis(250);
const FEEDBACK_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
const ENCODER_CONTROL_COMMIT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FeedbackSeverity {
    Clean,
    InsufficientEvidence,
    Moderate,
    Severe,
    Stale,
}

#[derive(Clone, Debug)]
struct ShadowBitrateController {
    floor_kbps: u32,
    ceiling_kbps: u32,
    target_kbps: u32,
    next_decision_id: u64,
    last_report_id: Option<u64>,
    last_report_received_at: Option<Instant>,
    previous_telemetry: MediaV3TelemetrySnapshot,
    baseline_rtt_micros: Option<u64>,
    last_sequence: Option<u64>,
    moderate_windows: u8,
    clean_windows: u8,
    cooldown_windows: u8,
}

impl ShadowBitrateController {
    fn new(configured_ceiling_kbps: u32, telemetry: MediaV3TelemetrySnapshot) -> Self {
        let ceiling_kbps = quantize_down(configured_ceiling_kbps).max(ADAPTIVE_BITRATE_FLOOR_KBPS);
        Self {
            floor_kbps: ADAPTIVE_BITRATE_FLOOR_KBPS,
            ceiling_kbps,
            target_kbps: ceiling_kbps,
            next_decision_id: 1,
            last_report_id: None,
            last_report_received_at: None,
            previous_telemetry: telemetry,
            baseline_rtt_micros: None,
            last_sequence: None,
            moderate_windows: 0,
            clean_windows: 0,
            cooldown_windows: 0,
        }
    }

    fn decide(
        &mut self,
        report: &MediaFeedbackReportV1,
        telemetry: MediaV3TelemetrySnapshot,
        received_at: Instant,
    ) -> Result<AdaptiveBitrateDecisionV1> {
        ensure!(
            self.last_report_id
                .is_none_or(|last| report.report_id > last),
            "feedback report IDs must increase monotonically"
        );
        let (severity, reasons, stale, trusted_host_pressure) =
            self.classify(report, telemetry, received_at);
        let mut state = AdaptiveBitrateStateV1::Hold;

        if self.cooldown_windows > 0 {
            self.cooldown_windows -= 1;
        }
        match severity {
            FeedbackSeverity::Severe => {
                self.moderate_windows = 0;
                self.clean_windows = 0;
                self.cooldown_windows = ADAPTIVE_BITRATE_COOLDOWN_WINDOWS;
                let next =
                    quantize_down(self.target_kbps.saturating_mul(3) / 4).max(self.floor_kbps);
                if next < self.target_kbps {
                    self.target_kbps = next;
                    state = AdaptiveBitrateStateV1::Decrease;
                }
            }
            FeedbackSeverity::Moderate => {
                self.moderate_windows = self.moderate_windows.saturating_add(1);
                self.clean_windows = 0;
                if self.moderate_windows >= ADAPTIVE_BITRATE_MODERATE_WINDOWS {
                    self.moderate_windows = 0;
                    self.cooldown_windows = ADAPTIVE_BITRATE_COOLDOWN_WINDOWS;
                    let next =
                        quantize_down(self.target_kbps.saturating_mul(4) / 5).max(self.floor_kbps);
                    if next < self.target_kbps {
                        self.target_kbps = next;
                        state = AdaptiveBitrateStateV1::Decrease;
                    }
                }
            }
            FeedbackSeverity::Clean => {
                self.moderate_windows = 0;
                self.clean_windows = self.clean_windows.saturating_add(1);
                if self.clean_windows >= ADAPTIVE_BITRATE_CLEAN_WINDOWS
                    && self.cooldown_windows == 0
                {
                    self.clean_windows = 0;
                    let maximum_step =
                        (self.target_kbps / 20).clamp(ADAPTIVE_BITRATE_QUANTUM_KBPS, 500);
                    let next = quantize_down(
                        self.target_kbps
                            .saturating_add(maximum_step)
                            .min(self.ceiling_kbps),
                    );
                    if next > self.target_kbps {
                        self.target_kbps = next;
                        state = AdaptiveBitrateStateV1::Increase;
                    }
                }
            }
            FeedbackSeverity::InsufficientEvidence | FeedbackSeverity::Stale => {
                self.moderate_windows = 0;
                self.clean_windows = 0;
            }
        }

        self.target_kbps = self.target_kbps.clamp(self.floor_kbps, self.ceiling_kbps);
        let decision = AdaptiveBitrateDecisionV1 {
            decision_id: self.next_decision_id,
            report_id: report.report_id,
            target_kbps: self.target_kbps,
            floor_kbps: self.floor_kbps,
            ceiling_kbps: self.ceiling_kbps,
            state,
            reasons,
            applied: false,
        };
        decision.validate()?;
        self.next_decision_id = self
            .next_decision_id
            .checked_add(1)
            .context("adaptive bitrate decision ID exhausted")?;
        self.last_report_id = Some(report.report_id);
        self.last_report_received_at = Some(received_at);
        if !stale
            || !trusted_host_pressure
            || state == AdaptiveBitrateStateV1::Decrease
            || self.target_kbps == self.floor_kbps
        {
            self.previous_telemetry = telemetry;
        }
        if !stale && let Some(sequence) = report.last_sequence {
            self.last_sequence = Some(
                self.last_sequence
                    .map_or(sequence, |last| last.max(sequence)),
            );
        }
        Ok(decision)
    }

    fn classify(
        &mut self,
        report: &MediaFeedbackReportV1,
        telemetry: MediaV3TelemetrySnapshot,
        received_at: Instant,
    ) -> (FeedbackSeverity, AdaptiveBitrateReasonFlagsV1, bool, bool) {
        let stale = !(FEEDBACK_FRESH_INTERVAL_MIN_MS..=FEEDBACK_FRESH_INTERVAL_MAX_MS)
            .contains(&report.interval_ms)
            || self.last_report_received_at.is_none()
            || self.last_report_received_at.is_some_and(|last| {
                let gap = received_at.saturating_duration_since(last);
                !(FEEDBACK_FRESH_ARRIVAL_GAP_MIN..=FEEDBACK_STALE_ARRIVAL_GAP).contains(&gap)
            });
        let mut severity = FeedbackSeverity::Clean;
        let mut reasons = AdaptiveBitrateReasonFlagsV1::NONE;

        let lost_delta = telemetry
            .selected_path_lost_packets
            .saturating_sub(self.previous_telemetry.selected_path_lost_packets);
        let congestion_delta = telemetry
            .selected_path_congestion_events
            .saturating_sub(self.previous_telemetry.selected_path_congestion_events);
        let cancellation_delta = telemetry
            .scheduler_cancellations
            .saturating_sub(self.previous_telemetry.scheduler_cancellations);
        let failure_delta = telemetry
            .send_failures
            .saturating_sub(self.previous_telemetry.send_failures);

        if failure_delta > 0 || lost_delta > 0 || cancellation_delta >= 2 {
            add_feedback_signal(
                &mut severity,
                &mut reasons,
                FeedbackSeverity::Severe,
                AdaptiveBitrateReasonFlagsV1::LOSS_OR_CANCELLATION,
            );
        } else if cancellation_delta > 0 || congestion_delta > 0 {
            add_feedback_signal(
                &mut severity,
                &mut reasons,
                FeedbackSeverity::Moderate,
                AdaptiveBitrateReasonFlagsV1::LOSS_OR_CANCELLATION,
            );
        }
        if cancellation_delta >= 2 || failure_delta > 0 {
            add_feedback_signal(
                &mut severity,
                &mut reasons,
                FeedbackSeverity::Severe,
                AdaptiveBitrateReasonFlagsV1::SENDER_BACKPRESSURE,
            );
        } else if cancellation_delta > 0 {
            add_feedback_signal(
                &mut severity,
                &mut reasons,
                FeedbackSeverity::Moderate,
                AdaptiveBitrateReasonFlagsV1::SENDER_BACKPRESSURE,
            );
        }

        let rtt = telemetry.selected_path_rtt_micros;
        if rtt > 0 {
            let baseline = self.baseline_rtt_micros.get_or_insert(rtt);
            if rtt < *baseline {
                *baseline = rtt;
            } else if rtt >= baseline.saturating_mul(2).max(80_000) {
                add_feedback_signal(
                    &mut severity,
                    &mut reasons,
                    FeedbackSeverity::Severe,
                    AdaptiveBitrateReasonFlagsV1::RTT_INFLATION,
                );
            } else if rtt >= baseline.saturating_mul(3) / 2 {
                add_feedback_signal(
                    &mut severity,
                    &mut reasons,
                    FeedbackSeverity::Moderate,
                    AdaptiveBitrateReasonFlagsV1::RTT_INFLATION,
                );
            }
        }

        let trusted_host_pressure = severity != FeedbackSeverity::Clean;
        if stale {
            reasons = reasons.union(AdaptiveBitrateReasonFlagsV1::FEEDBACK_STALE);
            if !trusted_host_pressure {
                severity = FeedbackSeverity::Stale;
            }
            return (severity, reasons, true, trusted_host_pressure);
        }

        let receiver_drops = report
            .transport_dropped_delta
            .saturating_add(report.frontend_dropped_delta)
            .saturating_add(report.decoder_dropped_delta)
            .saturating_add(report.presenter_dropped_delta);
        let queue_severe = queue_ratio_at_least(
            report.frontend_queue_depth,
            report.frontend_queue_capacity,
            3,
            4,
        ) || queue_ratio_at_least(
            report.decode_queue_depth,
            report.decode_queue_capacity,
            3,
            4,
        ) || queue_ratio_at_least(
            report.presenter_queue_depth,
            report.presenter_queue_capacity,
            3,
            4,
        );
        let queue_moderate = queue_ratio_at_least(
            report.frontend_queue_depth,
            report.frontend_queue_capacity,
            1,
            2,
        ) || queue_ratio_at_least(
            report.decode_queue_depth,
            report.decode_queue_capacity,
            1,
            2,
        ) || queue_ratio_at_least(
            report.presenter_queue_depth,
            report.presenter_queue_capacity,
            1,
            2,
        );
        if report.flags.contains(MediaFeedbackFlags::RESYNC_ACTIVE)
            || queue_severe
            || receiver_drops >= 4
        {
            add_feedback_signal(
                &mut severity,
                &mut reasons,
                FeedbackSeverity::Severe,
                AdaptiveBitrateReasonFlagsV1::RECEIVER_QUEUE,
            );
        } else if queue_moderate || receiver_drops > 0 {
            add_feedback_signal(
                &mut severity,
                &mut reasons,
                FeedbackSeverity::Moderate,
                AdaptiveBitrateReasonFlagsV1::RECEIVER_QUEUE,
            );
        }
        if report.decode_queue_depth == report.decode_queue_capacity
            || report.decoder_dropped_delta > 0
        {
            add_feedback_signal(
                &mut severity,
                &mut reasons,
                FeedbackSeverity::Severe,
                AdaptiveBitrateReasonFlagsV1::DECODE_BACKLOG,
            );
        } else if report.decode_queue_depth >= 2 {
            add_feedback_signal(
                &mut severity,
                &mut reasons,
                FeedbackSeverity::Moderate,
                AdaptiveBitrateReasonFlagsV1::DECODE_BACKLOG,
            );
        }

        let delivery_p95 = report.transport_delivery_p95_ms.unwrap_or(0);
        let decode_p95 = report.decode_p95_ms.unwrap_or(0);
        let presentation_p95 = report.presentation_p95_ms.unwrap_or(0);
        if delivery_p95 >= 200 || decode_p95 >= 100 || presentation_p95 >= 100 {
            add_feedback_signal(
                &mut severity,
                &mut reasons,
                FeedbackSeverity::Severe,
                AdaptiveBitrateReasonFlagsV1::DELIVERY_LATENCY,
            );
        } else if delivery_p95 >= 100 || decode_p95 >= 50 || presentation_p95 >= 50 {
            add_feedback_signal(
                &mut severity,
                &mut reasons,
                FeedbackSeverity::Moderate,
                AdaptiveBitrateReasonFlagsV1::DELIVERY_LATENCY,
            );
        }

        if severity == FeedbackSeverity::Clean {
            let sequence_progressed = self
                .last_sequence
                .zip(report.last_sequence)
                .is_some_and(|(last, current)| current > last);
            let complete_latency = report.transport_delivery_p95_ms.is_some()
                && report.decode_p95_ms.is_some()
                && report.presentation_p95_ms.is_some();
            if sequence_progressed && complete_latency {
                if self.target_kbps < self.ceiling_kbps {
                    reasons = reasons.union(AdaptiveBitrateReasonFlagsV1::CLEAN_RECOVERY);
                }
            } else {
                severity = FeedbackSeverity::InsufficientEvidence;
            }
        }
        (severity, reasons, false, trusted_host_pressure)
    }

    fn commit_observation_from(&mut self, candidate: &Self) {
        self.next_decision_id = candidate.next_decision_id;
        self.last_report_id = candidate.last_report_id;
        self.last_report_received_at = candidate.last_report_received_at;
        self.previous_telemetry = candidate.previous_telemetry;
        self.baseline_rtt_micros = candidate.baseline_rtt_micros;
        self.last_sequence = candidate.last_sequence;
    }
}

fn add_feedback_signal(
    severity: &mut FeedbackSeverity,
    reasons: &mut AdaptiveBitrateReasonFlagsV1,
    signal: FeedbackSeverity,
    reason: AdaptiveBitrateReasonFlagsV1,
) {
    *reasons = reasons.union(reason);
    if signal == FeedbackSeverity::Severe
        || (*severity == FeedbackSeverity::Clean && signal == FeedbackSeverity::Moderate)
    {
        *severity = signal;
    }
}

fn quantize_down(kbps: u32) -> u32 {
    kbps / ADAPTIVE_BITRATE_QUANTUM_KBPS * ADAPTIVE_BITRATE_QUANTUM_KBPS
}

fn queue_ratio_at_least(depth: u8, capacity: u8, numerator: u16, denominator: u16) -> bool {
    capacity > 0
        && u16::from(depth).saturating_mul(denominator)
            >= u16::from(capacity).saturating_mul(numerator)
}

#[derive(Debug)]
pub struct SessionRegistry {
    active: Mutex<Option<ActiveSession>>,
    pending_moq: Mutex<Option<PendingMoqAttachment>>,
    next_session_id: AtomicU64,
    session_changed: tokio::sync::Notify,
    pending_handshakes: tokio::sync::Semaphore,
}

struct PendingMoqAttachment {
    remote: EndpointId,
    session_id: u64,
    broadcast_name: String,
    broadcast: BroadcastConsumer,
    attached: tokio::sync::oneshot::Sender<()>,
    closed: tokio::sync::oneshot::Sender<()>,
    telemetry: Arc<MediaV3Telemetry>,
}

impl std::fmt::Debug for PendingMoqAttachment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingMoqAttachment")
            .field("remote", &self.remote)
            .field("session_id", &self.session_id)
            .field("broadcast_name", &self.broadcast_name)
            .finish_non_exhaustive()
    }
}

struct ClaimedMoqAttachment {
    session_id: u64,
    broadcast_name: String,
    broadcast: BroadcastConsumer,
    attached: tokio::sync::oneshot::Sender<()>,
    closed: tokio::sync::oneshot::Sender<()>,
    telemetry: Arc<MediaV3Telemetry>,
}

struct MoqAttachmentWait {
    attached: tokio::sync::oneshot::Receiver<()>,
    closed: tokio::sync::oneshot::Receiver<()>,
}

#[derive(Clone, Debug)]
struct ActiveSession {
    remote: EndpointId,
    session_id: u64,
    nonce: [u8; 16],
    session_clock: SessionClock,
    grants: InvitationGrants,
    media_active: bool,
    input_claimed: bool,
    audio_claimed: bool,
    feedback_claimed: bool,
    media_v3_telemetry: Arc<MediaV3Telemetry>,
    encoder_control: Option<EncoderControl>,
}

#[derive(Debug, Default)]
struct MediaV3Telemetry {
    scheduler_cancellations: AtomicU64,
    send_failures: AtomicU64,
    selected_path_rtt_micros: AtomicU64,
    selected_path_lost_packets: AtomicU64,
    selected_path_congestion_events: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MediaV3TelemetrySnapshot {
    scheduler_cancellations: u64,
    send_failures: u64,
    selected_path_rtt_micros: u64,
    selected_path_lost_packets: u64,
    selected_path_congestion_events: u64,
}

#[derive(Clone, Debug)]
struct AdaptiveEncoderProposal {
    control: EncoderControl,
    target_kbps: u32,
    bitrate_revision: u64,
    force_keyframe_revision: Option<u64>,
}

impl MediaV3Telemetry {
    fn snapshot(&self) -> MediaV3TelemetrySnapshot {
        MediaV3TelemetrySnapshot {
            scheduler_cancellations: self.scheduler_cancellations.load(Ordering::Relaxed),
            send_failures: self.send_failures.load(Ordering::Relaxed),
            selected_path_rtt_micros: self.selected_path_rtt_micros.load(Ordering::Relaxed),
            selected_path_lost_packets: self.selected_path_lost_packets.load(Ordering::Relaxed),
            selected_path_congestion_events: self
                .selected_path_congestion_events
                .load(Ordering::Relaxed),
        }
    }

    fn record_selected_path(&self, connection: &Connection) {
        let paths = connection.paths();
        let Some(path) = paths.iter().find(|path| path.is_selected()) else {
            return;
        };
        let stats = path.stats();
        self.selected_path_rtt_micros.store(
            u64::try_from(stats.rtt.as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        self.selected_path_lost_packets
            .store(stats.lost_packets, Ordering::Relaxed);
        self.selected_path_congestion_events
            .store(stats.congestion_events, Ordering::Relaxed);
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self {
            active: Mutex::new(None),
            pending_moq: Mutex::new(None),
            next_session_id: AtomicU64::new(0),
            session_changed: tokio::sync::Notify::new(),
            pending_handshakes: tokio::sync::Semaphore::new(MAX_PENDING_HANDSHAKES),
        }
    }
}

impl SessionRegistry {
    fn claim(
        self: &Arc<Self>,
        remote: EndpointId,
        nonce: [u8; 16],
        grants: InvitationGrants,
    ) -> Result<SessionLease> {
        let mut active = self.active.lock().expect("session registry poisoned");
        if let Some(current) = active.as_ref() {
            bail!("host already has active client {}", current.remote);
        }
        let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed) + 1;
        let session_clock = SessionClock::start();
        let media_v3_telemetry = Arc::new(MediaV3Telemetry::default());
        *active = Some(ActiveSession {
            remote,
            session_id,
            nonce,
            session_clock,
            grants,
            media_active: true,
            input_claimed: false,
            audio_claimed: false,
            feedback_claimed: false,
            media_v3_telemetry: Arc::clone(&media_v3_telemetry),
            encoder_control: None,
        });
        Ok(SessionLease {
            registry: Arc::clone(self),
            remote,
            session_id,
            session_clock,
            media_v3_telemetry,
        })
    }

    fn claim_input(self: &Arc<Self>, remote: EndpointId, nonce: [u8; 16]) -> Result<InputLease> {
        let mut active = self.active.lock().expect("session registry poisoned");
        let session = active
            .as_mut()
            .filter(|session| {
                session.media_active && session.remote == remote && session.nonce == nonce
            })
            .context("input connection does not match the active media session")?;
        ensure!(
            !session.input_claimed,
            "active client already has an input stream"
        );
        session.input_claimed = true;
        Ok(InputLease {
            registry: Arc::clone(self),
            remote,
            session_id: session.session_id,
            grants: session.grants,
        })
    }

    fn install_encoder_control(
        &self,
        remote: EndpointId,
        session_id: u64,
        encoder_control: Option<EncoderControl>,
    ) -> Result<()> {
        let mut active = self.active.lock().expect("session registry poisoned");
        let session = active
            .as_mut()
            .filter(|session| {
                session.media_active && session.remote == remote && session.session_id == session_id
            })
            .context("encoder control does not match the active media session")?;
        ensure!(
            session.encoder_control.is_none(),
            "active media session already has encoder control"
        );
        session.encoder_control = encoder_control;
        Ok(())
    }

    fn propose_adaptive_encoder_update(
        &self,
        remote: EndpointId,
        session_id: u64,
        target_kbps: u32,
        force_keyframe: bool,
    ) -> Result<Option<AdaptiveEncoderProposal>> {
        let active = self.active.lock().expect("session registry poisoned");
        let session = active
            .as_ref()
            .filter(|session| {
                session.media_active && session.remote == remote && session.session_id == session_id
            })
            .context("adaptive encoder update does not match the active media session")?;
        let Some(control) = session.encoder_control.clone() else {
            return Ok(None);
        };
        let bitrate_revision = control.request_bitrate_kbps(target_kbps)?;
        let force_keyframe_revision = force_keyframe
            .then(|| control.request_force_keyframe())
            .transpose()?;
        Ok(Some(AdaptiveEncoderProposal {
            control,
            target_kbps,
            bitrate_revision,
            force_keyframe_revision,
        }))
    }

    fn claim_feedback(
        self: &Arc<Self>,
        remote: EndpointId,
        nonce: [u8; 16],
    ) -> Result<FeedbackLease> {
        let mut active = self.active.lock().expect("session registry poisoned");
        let session = active
            .as_mut()
            .filter(|session| {
                session.media_active && session.remote == remote && session.nonce == nonce
            })
            .context("feedback connection does not match the active media session")?;
        ensure!(
            session.grants.contains(InvitationGrants::VIEW),
            "active Portal session lacks feedback view permission"
        );
        ensure!(
            !session.feedback_claimed,
            "active client already has a feedback connection"
        );
        session.feedback_claimed = true;
        Ok(FeedbackLease {
            registry: Arc::clone(self),
            remote,
            session_id: session.session_id,
            telemetry: Arc::clone(&session.media_v3_telemetry),
            encoder_control: session.encoder_control.clone(),
        })
    }

    fn claim_audio(self: &Arc<Self>, remote: EndpointId, nonce: [u8; 16]) -> Result<AudioLease> {
        let mut active = self.active.lock().expect("session registry poisoned");
        let session = active
            .as_mut()
            .filter(|session| {
                session.media_active && session.remote == remote && session.nonce == nonce
            })
            .context("audio connection does not match the active media session")?;
        ensure!(
            !session.audio_claimed,
            "active client already has an audio connection"
        );
        session.audio_claimed = true;
        Ok(AudioLease {
            registry: Arc::clone(self),
            remote,
            session_id: session.session_id,
            session_clock: session.session_clock,
            grants: session.grants,
        })
    }

    fn expect_moq(
        &self,
        remote: EndpointId,
        session_id: u64,
        broadcast_name: String,
        broadcast: BroadcastConsumer,
    ) -> Result<MoqAttachmentWait> {
        let active = self.active.lock().expect("session registry poisoned");
        let telemetry = active
            .as_ref()
            .filter(|session| {
                session.media_active && session.remote == remote && session.session_id == session_id
            })
            .map(|session| Arc::clone(&session.media_v3_telemetry))
            .context("MoQ expectation does not match the active control session")?;
        let mut pending = self.pending_moq.lock().expect("MoQ registry poisoned");
        ensure!(
            pending.is_none(),
            "active control session already expects MoQ"
        );
        let (attached, attached_rx) = tokio::sync::oneshot::channel();
        let (closed, closed_rx) = tokio::sync::oneshot::channel();
        *pending = Some(PendingMoqAttachment {
            remote,
            session_id,
            broadcast_name,
            broadcast,
            attached,
            closed,
            telemetry,
        });
        Ok(MoqAttachmentWait {
            attached: attached_rx,
            closed: closed_rx,
        })
    }

    fn claim_moq(&self, remote: EndpointId) -> Result<ClaimedMoqAttachment> {
        let active = self.active.lock().expect("session registry poisoned");
        let session = active
            .as_ref()
            .filter(|session| session.media_active && session.remote == remote)
            .context("MoQ connection does not match the active control session")?;
        let mut pending = self.pending_moq.lock().expect("MoQ registry poisoned");
        let attachment = pending
            .as_ref()
            .filter(|attachment| {
                attachment.remote == remote && attachment.session_id == session.session_id
            })
            .context("active control session is not expecting a MoQ connection")?;
        debug_assert_eq!(attachment.remote, session.remote);
        let attachment = pending
            .take()
            .expect("validated pending MoQ attachment disappeared");
        Ok(ClaimedMoqAttachment {
            session_id: attachment.session_id,
            broadcast_name: attachment.broadcast_name,
            broadcast: attachment.broadcast,
            attached: attachment.attached,
            closed: attachment.closed,
            telemetry: attachment.telemetry,
        })
    }

    fn release(&self, remote: EndpointId, session_id: u64) {
        let mut active = self.active.lock().expect("session registry poisoned");
        if let Some(session) = active.as_mut()
            && session.remote == remote
            && session.session_id == session_id
        {
            let mut pending = self.pending_moq.lock().expect("MoQ registry poisoned");
            if pending.as_ref().is_some_and(|attachment| {
                attachment.remote == remote && attachment.session_id == session_id
            }) {
                *pending = None;
            }
            // Keep the registry occupied until the input handler has observed
            // media shutdown and released all held uinput transitions. This
            // prevents a reconnect from sharing the device with a draining
            // predecessor session.
            session.media_active = false;
            session.encoder_control = None;
            if !session.input_claimed && !session.audio_claimed && !session.feedback_claimed {
                *active = None;
            }
            drop(active);
            self.session_changed.notify_waiters();
        }
    }

    fn is_active(&self, remote: EndpointId, session_id: u64) -> bool {
        self.active
            .lock()
            .expect("session registry poisoned")
            .as_ref()
            .is_some_and(|active| {
                active.media_active && active.remote == remote && active.session_id == session_id
            })
    }

    fn release_input(&self, remote: EndpointId, session_id: u64) {
        let mut active = self.active.lock().expect("session registry poisoned");
        if let Some(session) = active.as_mut()
            && session.remote == remote
            && session.session_id == session_id
        {
            session.input_claimed = false;
            if !session.media_active && !session.audio_claimed && !session.feedback_claimed {
                *active = None;
            }
        }
    }

    fn release_audio(&self, remote: EndpointId, session_id: u64) {
        let mut active = self.active.lock().expect("session registry poisoned");
        if let Some(session) = active.as_mut()
            && session.remote == remote
            && session.session_id == session_id
        {
            session.audio_claimed = false;
            if !session.media_active && !session.input_claimed && !session.feedback_claimed {
                *active = None;
            }
        }
    }

    fn release_feedback(&self, remote: EndpointId, session_id: u64) {
        let mut active = self.active.lock().expect("session registry poisoned");
        if let Some(session) = active.as_mut()
            && session.remote == remote
            && session.session_id == session_id
        {
            session.feedback_claimed = false;
            if !session.media_active && !session.input_claimed && !session.audio_claimed {
                *active = None;
            }
        }
    }
}

#[derive(Debug)]
struct SessionLease {
    registry: Arc<SessionRegistry>,
    remote: EndpointId,
    session_id: u64,
    session_clock: SessionClock,
    media_v3_telemetry: Arc<MediaV3Telemetry>,
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        self.registry.release(self.remote, self.session_id);
    }
}

#[derive(Debug)]
struct InputLease {
    registry: Arc<SessionRegistry>,
    remote: EndpointId,
    session_id: u64,
    grants: InvitationGrants,
}

#[derive(Debug)]
struct AudioLease {
    registry: Arc<SessionRegistry>,
    remote: EndpointId,
    session_id: u64,
    session_clock: SessionClock,
    grants: InvitationGrants,
}

#[derive(Debug)]
struct FeedbackLease {
    registry: Arc<SessionRegistry>,
    remote: EndpointId,
    session_id: u64,
    telemetry: Arc<MediaV3Telemetry>,
    encoder_control: Option<EncoderControl>,
}

#[derive(Debug)]
struct SourceTaskGuard(Option<tokio::task::JoinHandle<Result<()>>>);

impl SourceTaskGuard {
    fn new(task: tokio::task::JoinHandle<Result<()>>) -> Self {
        Self(Some(task))
    }

    async fn abort_and_wait(mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }

    async fn wait_or_abort(mut self, grace_timeout: Duration) {
        let Some(mut task) = self.0.take() else {
            return;
        };
        if tokio::time::timeout(grace_timeout, &mut task)
            .await
            .is_err()
        {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for SourceTaskGuard {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

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

impl Drop for InputLease {
    fn drop(&mut self) {
        self.registry.release_input(self.remote, self.session_id);
    }
}

impl Drop for AudioLease {
    fn drop(&mut self) {
        self.registry.release_audio(self.remote, self.session_id);
    }
}

impl Drop for FeedbackLease {
    fn drop(&mut self) {
        self.registry.release_feedback(self.remote, self.session_id);
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
    let _encoder_control = encoder_control;

    let mut broadcast = Broadcast::new().produce();
    let track = broadcast
        .create_track(Track {
            name: MOQ_VIDEO_H264_TRACK.to_owned(),
            priority: MOQ_VIDEO_TRACK_PRIORITY,
        })
        .context("creating static MoQ H.264 track")?;
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
        Arc::clone(&lease.media_v3_telemetry),
    )
    .await;

    drop(current_gop_receiver);
    drop(frame_receiver);
    source_task.wait_or_abort(SOURCE_REAP_GRACE_TIMEOUT).await;
    drop(lease);
    info!(%remote, "MoQ control client released");
    session_result
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
                    let cancelled_group = publisher.request_keyframe();
                    if cancelled_group.is_some() {
                        telemetry
                            .scheduler_cancellations
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    replay_cursor.enter_resync_through(through_sequence);
                    debug!(
                        %remote,
                        request_id = request.request_id,
                        ?request.reason,
                        advisory_last_sequence = ?request.last_sequence,
                        coalesced = cancelled_group.is_none(),
                        ?cancelled_group,
                        "accepted MoQ keyframe request"
                    );
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

    publisher.abort();
    let _ = broadcast.abort(MoqError::Cancel);
    if !control_task_finished {
        control_task.abort();
        let _ = control_task.await;
    }
    result
}

async fn serve_media_feedback(
    connection: Connection,
    config: &HostConfig,
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
        .context("timed out accepting media feedback stream")?
        .context("accepting media feedback stream")?;
    let hello = receive_hello(&mut recv, Capability::VideoH264).await?;
    drop(handshake_permit);
    debug!(%remote, agent = %hello.agent, "media feedback hello received");
    ensure!(
        hello.invitation.is_none(),
        "invitations are accepted only on the first media handshake"
    );

    let grants = match authorization.authorize_or_redeem(remote, None, unix_timestamp_now()?) {
        Ok(grants) => grants,
        Err(error) => {
            send_rejection(&mut send, "Portal peer is not authorized").await?;
            return Err(error.context("authorizing media feedback peer"));
        }
    };
    ensure!(
        grants.contains(InvitationGrants::VIEW),
        "authorized feedback peer lacks view permission"
    );
    let ceiling_kbps = adaptive_bitrate_ceiling_kbps(config)?;
    let lease = match sessions.claim_feedback(remote, hello.nonce) {
        Ok(lease) => lease,
        Err(error) => {
            send_rejection(&mut send, error.to_string()).await?;
            return Err(error);
        }
    };
    let encoder_actuation_available =
        adaptive_bitrate_actuation_enabled(config) && lease.encoder_control.is_some();

    write_host_hello(
        &mut send,
        &HostHello::accepted(
            lease.session_id,
            negotiated_capabilities(&hello, MEDIA_CAPABILITIES),
        ),
    )
    .await?;
    info!(
        %remote,
        session_id = lease.session_id,
        ceiling_kbps,
        applied = false,
        mode = if encoder_actuation_available { "active" } else { "shadow" },
        "media feedback client accepted for bitrate control"
    );

    let (feedback_sender, mut feedback_receiver) = tokio::sync::watch::channel(None);
    let mut reader_task = tokio::spawn(forward_media_feedback_reports(recv, feedback_sender));
    let mut reader_finished = false;
    let mut receiver_open = true;
    let mut controller = ShadowBitrateController::new(ceiling_kbps, lease.telemetry.snapshot());

    let result: Result<()> = loop {
        let session_changed = sessions.session_changed.notified();
        tokio::pin!(session_changed);
        session_changed.as_mut().enable();
        if !sessions.is_active(remote, lease.session_id) {
            break Ok(());
        }
        tokio::select! {
            biased;
            reader_result = &mut reader_task, if !reader_finished => {
                reader_finished = true;
                match reader_result {
                    Ok(Ok(())) => {
                        debug!(%remote, "media feedback report stream closed");
                    }
                    Ok(Err(error)) => {
                        break Err(error).context("reading media feedback reports");
                    }
                    Err(error) => {
                        break Err(error).context("media feedback reader task failed");
                    }
                }
            }
            changed = feedback_receiver.changed(), if receiver_open => {
                if changed.is_err() {
                    receiver_open = false;
                    if reader_finished {
                        break Ok(());
                    }
                    continue;
                }
                let Some(report) = *feedback_receiver.borrow_and_update() else {
                    continue;
                };
                let telemetry = lease.telemetry.snapshot();
                let mut candidate = controller.clone();
                let mut decision = candidate.decide(&report, telemetry, Instant::now())?;
                let changes_bitrate = decision.state != AdaptiveBitrateStateV1::Hold;
                let can_actuate = changes_bitrate && encoder_actuation_available;
                let mut recovery_acknowledged = None;
                if can_actuate {
                    let force_keyframe =
                        adaptive_recovery_keyframe_required(&report, &decision);
                    let proposal = match sessions.propose_adaptive_encoder_update(
                        remote,
                        lease.session_id,
                        decision.target_kbps,
                        force_keyframe,
                    ) {
                        Ok(Some(proposal)) => Some(proposal),
                        Ok(None) => {
                            warn!(
                                %remote,
                                session_id = lease.session_id,
                                "active adaptive session lost encoder control; retaining committed bitrate"
                            );
                            None
                        }
                        Err(error) => {
                            warn!(
                                %remote,
                                session_id = lease.session_id,
                                %error,
                                "adaptive encoder proposal failed; retaining committed bitrate"
                            );
                            None
                        }
                    };
                    if let Some(proposal) = proposal {
                        match commit_adaptive_encoder_proposal(
                            sessions,
                            remote,
                            lease.session_id,
                            &proposal,
                        )
                        .await
                        {
                            AdaptiveEncoderCommit::GenerationEnded => break Ok(()),
                            AdaptiveEncoderCommit::NotApplied(error) => {
                                warn!(
                                    %remote,
                                    session_id = lease.session_id,
                                    %error,
                                    "adaptive encoder application failed; retaining committed bitrate"
                                );
                            }
                            AdaptiveEncoderCommit::Applied => {
                                decision.applied = true;
                                controller = candidate.clone();
                                if proposal.force_keyframe_revision.is_some() {
                                    match await_adaptive_recovery_keyframe(
                                        sessions,
                                        remote,
                                        lease.session_id,
                                        &proposal,
                                    )
                                    .await
                                    {
                                        None => break Ok(()),
                                        Some(Ok(())) => recovery_acknowledged = Some(true),
                                        Some(Err(error)) => {
                                            recovery_acknowledged = Some(false);
                                            warn!(
                                                %remote,
                                                session_id = lease.session_id,
                                                %error,
                                                "adaptive bitrate applied but forced-IDR recovery was not acknowledged"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if !decision.applied {
                        controller.commit_observation_from(&candidate);
                    }
                } else {
                    // External gst-launch and sources without EncoderControl
                    // remain intentionally shadow-only. Hold decisions do not
                    // need encoder actuation.
                    controller = candidate;
                }
                tokio::time::timeout(
                    FEEDBACK_WRITE_TIMEOUT,
                    write_adaptive_bitrate_decision_v1(&mut send, &decision),
                )
                .await
                .context("timed out writing adaptive bitrate shadow decision")??;
                debug!(
                    %remote,
                    session_id = lease.session_id,
                    report_id = decision.report_id,
                    decision_id = decision.decision_id,
                    target_kbps = decision.target_kbps,
                    ?decision.state,
                    reasons = decision.reasons.bits(),
                    applied = decision.applied,
                    ?recovery_acknowledged,
                    path_rtt_micros = telemetry.selected_path_rtt_micros,
                    path_lost_packets = telemetry.selected_path_lost_packets,
                    path_congestion_events = telemetry.selected_path_congestion_events,
                    scheduler_cancellations = telemetry.scheduler_cancellations,
                    send_failures = telemetry.send_failures,
                    mode = if can_actuate { "active" } else { "shadow" },
                    "adaptive bitrate decision"
                );
            }
            _ = &mut session_changed => {
                if !sessions.is_active(remote, lease.session_id) {
                    break Ok(());
                }
            }
            closed = connection.closed() => {
                debug!(%remote, ?closed, "media feedback connection closed");
                break Ok(());
            }
        }
        if reader_finished && !receiver_open {
            break Ok(());
        }
    };

    if !reader_finished {
        reader_task.abort();
        let _ = reader_task.await;
    }
    let _ = send.finish();
    drop(lease);
    info!(%remote, "media feedback client released");
    result
}

async fn forward_media_feedback_reports<R>(
    mut reader: R,
    sender: tokio::sync::watch::Sender<Option<MediaFeedbackReportV1>>,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut last_report_id = None;
    let mut next_read_at = tokio::time::Instant::now();
    loop {
        tokio::time::sleep_until(next_read_at).await;
        let Some(report) = read_media_feedback_report_v1(&mut reader).await? else {
            return Ok(());
        };
        ensure!(
            last_report_id.is_none_or(|last| report.report_id > last),
            "feedback report IDs must increase monotonically"
        );
        last_report_id = Some(report.report_id);
        sender.send_replace(Some(report));
        next_read_at = tokio::time::Instant::now() + FEEDBACK_MIN_READ_INTERVAL;
    }
}

fn adaptive_bitrate_ceiling_kbps(config: &HostConfig) -> Result<u32> {
    let configured = match config.source {
        VideoSource::TestPattern => TEST_PATTERN_BITRATE_CEILING_KBPS,
        VideoSource::GamescopePipewire => config
            .gamescope_pipewire
            .as_ref()
            .and_then(|gamescope| gamescope.bitrate_kbps)
            .context("adaptive bitrate shadow mode requires a Gamescope CBR bitrate")?,
    };
    let ceiling = quantize_down(configured);
    ensure!(
        ceiling >= ADAPTIVE_BITRATE_FLOOR_KBPS,
        "adaptive bitrate ceiling is below the 1000 kbps floor"
    );
    Ok(ceiling)
}

fn adaptive_bitrate_actuation_enabled(config: &HostConfig) -> bool {
    config.source == VideoSource::GamescopePipewire
        && config.gamescope_pipewire.as_ref().is_some_and(|gamescope| {
            gamescope.encoder_backend == GamescopeEncoderBackend::InProcessGstreamer
                && gamescope.rate_control == VaapiRateControl::Cbr
        })
}

fn adaptive_recovery_keyframe_required(
    report: &MediaFeedbackReportV1,
    decision: &AdaptiveBitrateDecisionV1,
) -> bool {
    decision.state != AdaptiveBitrateStateV1::Hold
        && report.flags.contains(MediaFeedbackFlags::RESYNC_ACTIVE)
}

#[derive(Debug)]
enum AdaptiveEncoderCommit {
    GenerationEnded,
    NotApplied(anyhow::Error),
    Applied,
}

async fn await_while_session_active<F, T>(
    sessions: &SessionRegistry,
    remote: EndpointId,
    session_id: u64,
    future: F,
) -> Option<T>
where
    F: Future<Output = T>,
{
    tokio::pin!(future);
    loop {
        let session_changed = sessions.session_changed.notified();
        tokio::pin!(session_changed);
        session_changed.as_mut().enable();
        if !sessions.is_active(remote, session_id) {
            return None;
        }
        tokio::select! {
            result = &mut future => return sessions.is_active(remote, session_id).then_some(result),
            _ = &mut session_changed => {}
        }
    }
}

async fn commit_adaptive_encoder_proposal(
    sessions: &SessionRegistry,
    remote: EndpointId,
    session_id: u64,
    proposal: &AdaptiveEncoderProposal,
) -> AdaptiveEncoderCommit {
    let bitrate_result = await_while_session_active(
        sessions,
        remote,
        session_id,
        proposal
            .control
            .wait_for_bitrate_applied(proposal.bitrate_revision, ENCODER_CONTROL_COMMIT_TIMEOUT),
    )
    .await;
    let Some(bitrate_result) = bitrate_result else {
        return AdaptiveEncoderCommit::GenerationEnded;
    };
    let status = match bitrate_result {
        Ok(status) => status,
        Err(error) => return AdaptiveEncoderCommit::NotApplied(error),
    };
    if status.applied_bitrate_revision != Some(proposal.bitrate_revision)
        || status.applied_bitrate_kbps != Some(proposal.target_kbps)
    {
        return AdaptiveEncoderCommit::NotApplied(anyhow::anyhow!(
            "encoder bitrate commit mismatch: revision {:?}, readback {:?}, expected revision {} at {} kbps",
            status.applied_bitrate_revision,
            status.applied_bitrate_kbps,
            proposal.bitrate_revision,
            proposal.target_kbps
        ));
    }

    AdaptiveEncoderCommit::Applied
}

async fn await_adaptive_recovery_keyframe(
    sessions: &SessionRegistry,
    remote: EndpointId,
    session_id: u64,
    proposal: &AdaptiveEncoderProposal,
) -> Option<Result<()>> {
    let force_keyframe_revision = proposal.force_keyframe_revision?;
    await_while_session_active(
        sessions,
        remote,
        session_id,
        proposal.control.wait_for_force_keyframe_acknowledged(
            force_keyframe_revision,
            ENCODER_CONTROL_COMMIT_TIMEOUT,
        ),
    )
    .await
    .map(|result| result.map(|_| ()))
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
                let mut flags = FrameFlags::NONE;
                if frame.keyframe {
                    flags = flags.union(FrameFlags::KEYFRAME);
                }
                if frame.codec_config {
                    flags = flags.union(FrameFlags::CODEC_CONFIG);
                }
                if discontinuity {
                    flags = flags.union(FrameFlags::DISCONTINUITY);
                }
                let width = u16::try_from(config.width).context("width exceeds protocol")?;
                let height = u16::try_from(config.height).context("height exceeds protocol")?;
                let header = MediaFrameHeader::h264(
                    width,
                    height,
                    frame.data.len(),
                    frame.sequence,
                    frame.capture_timestamp_micros,
                    frame.presentation_timestamp_micros,
                    flags,
                )?;
                let media_frame = MediaFrame::new(header, frame.data.as_ref().to_vec())?;
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
        .context("finishing media v3 handshake response")?;
    drop(send);
    info!(%remote, session_id = lease.session_id, "media v3 client accepted");

    let session_result = run_media_v3_session(
        &connection,
        &config,
        &mut current_gop_receiver,
        recv,
        remote,
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
    config: &HostConfig,
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
    if discontinuity {
        flags = flags.union(FrameFlags::DISCONTINUITY);
    }
    let width = u16::try_from(config.width).context("width exceeds protocol")?;
    let height = u16::try_from(config.height).context("height exceeds protocol")?;
    let header = MediaFrameHeader::h264(
        width,
        height,
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
                debug!(
                    %remote,
                    request_id = request.request_id,
                    ?request.reason,
                    advisory_last_sequence = ?request.last_sequence,
                    coalesced = !transitioned,
                    ?cancel_sequences,
                    "accepted media v3 keyframe request"
                );
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
    config: &HostConfig,
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
    if discontinuity {
        flags = flags.union(FrameFlags::DISCONTINUITY);
    }
    let publisher_priority = if position.object_id == 0 {
        MEDIA_V3_KEYFRAME_PUBLISHER_PRIORITY
    } else {
        MEDIA_V3_DELTA_PUBLISHER_PRIORITY
    };
    let header = MediaObjectHeaderV3::h264(
        u16::try_from(config.width).context("width exceeds protocol")?,
        u16::try_from(config.height).context("height exceeds protocol")?,
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
mod tests {
    use super::*;

    fn clean_feedback(report_id: u64) -> MediaFeedbackReportV1 {
        MediaFeedbackReportV1 {
            report_id,
            interval_ms: 1_000,
            flags: MediaFeedbackFlags::NONE,
            last_sequence: Some(report_id),
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

    fn controller_decide(
        controller: &mut ShadowBitrateController,
        report: &MediaFeedbackReportV1,
        second: u64,
    ) -> AdaptiveBitrateDecisionV1 {
        controller
            .decide(
                report,
                MediaV3TelemetrySnapshot::default(),
                Instant::now() + Duration::from_secs(second),
            )
            .unwrap()
    }

    #[test]
    fn shadow_controller_immediately_decreases_for_severe_feedback_and_stays_bounded() {
        let mut controller =
            ShadowBitrateController::new(12_137, MediaV3TelemetrySnapshot::default());
        assert_eq!(controller.ceiling_kbps, 12_000);
        let first = controller_decide(&mut controller, &clean_feedback(1), 0);
        assert_eq!(first.state, AdaptiveBitrateStateV1::Hold);
        assert!(
            first
                .reasons
                .contains(AdaptiveBitrateReasonFlagsV1::FEEDBACK_STALE)
        );
        for report_id in 2..=21 {
            let mut report = clean_feedback(report_id);
            report.flags = MediaFeedbackFlags::RESYNC_ACTIVE;
            let decision = controller_decide(&mut controller, &report, report_id - 1);
            assert!(!decision.applied);
            assert!(decision.target_kbps >= ADAPTIVE_BITRATE_FLOOR_KBPS);
            assert!(decision.target_kbps <= 12_000);
            assert_eq!(decision.target_kbps % ADAPTIVE_BITRATE_QUANTUM_KBPS, 0);
            assert!(
                decision
                    .reasons
                    .contains(AdaptiveBitrateReasonFlagsV1::RECEIVER_QUEUE)
            );
        }
        assert_eq!(controller.target_kbps, ADAPTIVE_BITRATE_FLOOR_KBPS);
    }

    #[test]
    fn shadow_controller_requires_two_moderate_windows() {
        let mut controller =
            ShadowBitrateController::new(12_000, MediaV3TelemetrySnapshot::default());
        controller_decide(&mut controller, &clean_feedback(1), 0);
        let mut first = clean_feedback(2);
        first.frontend_queue_depth = 2;
        assert_eq!(
            controller_decide(&mut controller, &first, 1).state,
            AdaptiveBitrateStateV1::Hold
        );
        let mut second = clean_feedback(3);
        second.frontend_queue_depth = 2;
        let decision = controller_decide(&mut controller, &second, 2);
        assert_eq!(decision.state, AdaptiveBitrateStateV1::Decrease);
        assert_eq!(decision.target_kbps, 9_500);
    }

    #[test]
    fn shadow_controller_increases_only_after_clean_cooldown() {
        let mut controller =
            ShadowBitrateController::new(12_000, MediaV3TelemetrySnapshot::default());
        controller_decide(&mut controller, &clean_feedback(1), 0);
        let mut severe = clean_feedback(2);
        severe.decoder_dropped_delta = 1;
        assert_eq!(
            controller_decide(&mut controller, &severe, 1).target_kbps,
            9_000
        );
        for report_id in 3..=11 {
            let decision =
                controller_decide(&mut controller, &clean_feedback(report_id), report_id - 1);
            assert_eq!(decision.state, AdaptiveBitrateStateV1::Hold);
            assert_eq!(decision.target_kbps, 9_000);
        }
        let decision = controller_decide(&mut controller, &clean_feedback(12), 11);
        assert_eq!(decision.state, AdaptiveBitrateStateV1::Increase);
        assert_eq!(decision.target_kbps, 9_250);
        assert!(
            decision
                .reasons
                .contains(AdaptiveBitrateReasonFlagsV1::CLEAN_RECOVERY)
        );
    }

    #[test]
    fn stale_or_non_monotonic_feedback_cannot_drive_an_increase() {
        let mut controller =
            ShadowBitrateController::new(12_000, MediaV3TelemetrySnapshot::default());
        controller_decide(&mut controller, &clean_feedback(1), 0);
        let mut severe = clean_feedback(2);
        severe.flags = MediaFeedbackFlags::RESYNC_ACTIVE;
        controller_decide(&mut controller, &severe, 1);
        let stale = controller_decide(&mut controller, &clean_feedback(3), 4);
        assert_eq!(stale.state, AdaptiveBitrateStateV1::Hold);
        assert_eq!(stale.target_kbps, 9_000);
        assert!(
            stale
                .reasons
                .contains(AdaptiveBitrateReasonFlagsV1::FEEDBACK_STALE)
        );
        assert!(
            controller
                .decide(
                    &clean_feedback(2),
                    MediaV3TelemetrySnapshot::default(),
                    Instant::now() + Duration::from_secs(4),
                )
                .is_err()
        );

        let now = Instant::now();
        let mut rapid = ShadowBitrateController::new(12_000, MediaV3TelemetrySnapshot::default());
        rapid
            .decide(&clean_feedback(1), MediaV3TelemetrySnapshot::default(), now)
            .unwrap();
        let too_fast = rapid
            .decide(
                &clean_feedback(2),
                MediaV3TelemetrySnapshot::default(),
                now + Duration::from_millis(500),
            )
            .unwrap();
        assert_eq!(too_fast.state, AdaptiveBitrateStateV1::Hold);
        assert!(
            too_fast
                .reasons
                .contains(AdaptiveBitrateReasonFlagsV1::FEEDBACK_STALE)
        );
    }

    #[test]
    fn full_decode_queue_is_severe_even_at_capacity_two() {
        let mut controller =
            ShadowBitrateController::new(12_000, MediaV3TelemetrySnapshot::default());
        controller_decide(&mut controller, &clean_feedback(1), 0);
        let mut report = clean_feedback(2);
        report.decode_queue_capacity = 2;
        report.decode_queue_depth = 2;
        let decision = controller_decide(&mut controller, &report, 1);
        assert_eq!(decision.state, AdaptiveBitrateStateV1::Decrease);
        assert!(
            decision
                .reasons
                .contains(AdaptiveBitrateReasonFlagsV1::DECODE_BACKLOG)
        );
    }

    #[test]
    fn decision_id_exhaustion_fails_instead_of_repeating() {
        let mut controller =
            ShadowBitrateController::new(12_000, MediaV3TelemetrySnapshot::default());
        controller.next_decision_id = u64::MAX;
        assert!(
            controller
                .decide(
                    &clean_feedback(1),
                    MediaV3TelemetrySnapshot::default(),
                    Instant::now(),
                )
                .is_err()
        );
        assert_eq!(controller.next_decision_id, u64::MAX);
    }

    #[test]
    fn stale_feedback_applies_trusted_loss_and_send_failure_pressure_once() {
        for telemetry in [
            MediaV3TelemetrySnapshot {
                selected_path_lost_packets: 1,
                ..MediaV3TelemetrySnapshot::default()
            },
            MediaV3TelemetrySnapshot {
                send_failures: 1,
                ..MediaV3TelemetrySnapshot::default()
            },
        ] {
            let now = Instant::now();
            let mut controller =
                ShadowBitrateController::new(12_000, MediaV3TelemetrySnapshot::default());
            let decision = controller
                .decide(&clean_feedback(1), telemetry, now)
                .unwrap();
            assert_eq!(decision.state, AdaptiveBitrateStateV1::Decrease);
            assert_eq!(decision.target_kbps, 9_000);
            assert!(
                decision
                    .reasons
                    .contains(AdaptiveBitrateReasonFlagsV1::FEEDBACK_STALE)
            );
            assert!(
                decision
                    .reasons
                    .contains(AdaptiveBitrateReasonFlagsV1::LOSS_OR_CANCELLATION)
            );

            let repeated = controller
                .decide(
                    &clean_feedback(2),
                    telemetry,
                    now + Duration::from_millis(500),
                )
                .unwrap();
            assert_eq!(repeated.state, AdaptiveBitrateStateV1::Hold);
            assert_eq!(repeated.target_kbps, 9_000);
        }
    }

    #[test]
    fn stale_moderate_cancellation_is_retained_until_hysteresis_reduces() {
        let now = Instant::now();
        let telemetry = MediaV3TelemetrySnapshot {
            scheduler_cancellations: 1,
            ..MediaV3TelemetrySnapshot::default()
        };
        let mut controller =
            ShadowBitrateController::new(12_000, MediaV3TelemetrySnapshot::default());
        let first = controller
            .decide(&clean_feedback(1), telemetry, now)
            .unwrap();
        assert_eq!(first.state, AdaptiveBitrateStateV1::Hold);
        assert_eq!(controller.previous_telemetry.scheduler_cancellations, 0);

        let second = controller
            .decide(
                &clean_feedback(2),
                telemetry,
                now + Duration::from_millis(500),
            )
            .unwrap();
        assert_eq!(second.state, AdaptiveBitrateStateV1::Decrease);
        assert_eq!(second.target_kbps, 9_500);
        assert_eq!(controller.previous_telemetry.scheduler_cancellations, 1);
    }

    #[test]
    fn clean_recovery_below_five_mbps_advances_by_one_quantum() {
        let mut controller =
            ShadowBitrateController::new(12_000, MediaV3TelemetrySnapshot::default());
        controller.target_kbps = 4_000;
        controller_decide(&mut controller, &clean_feedback(1), 0);
        controller_decide(&mut controller, &clean_feedback(2), 1);
        for report_id in 3..=11 {
            let decision =
                controller_decide(&mut controller, &clean_feedback(report_id), report_id - 1);
            assert_eq!(decision.state, AdaptiveBitrateStateV1::Hold);
        }
        let decision = controller_decide(&mut controller, &clean_feedback(12), 11);
        assert_eq!(decision.state, AdaptiveBitrateStateV1::Increase);
        assert_eq!(decision.target_kbps, 4_250);
    }

    #[test]
    fn recovery_requires_latency_samples_and_sequence_progress_but_pressure_still_reduces() {
        let mut controller =
            ShadowBitrateController::new(12_000, MediaV3TelemetrySnapshot::default());
        controller.target_kbps = 9_000;
        controller_decide(&mut controller, &clean_feedback(1), 0);
        controller_decide(&mut controller, &clean_feedback(2), 1);

        for report_id in 3..=14 {
            let mut report = clean_feedback(report_id);
            report.last_sequence = Some(2);
            let decision = controller_decide(&mut controller, &report, report_id - 1);
            assert_eq!(decision.state, AdaptiveBitrateStateV1::Hold);
            assert_eq!(decision.target_kbps, 9_000);
        }
        assert_eq!(controller.clean_windows, 0);

        for report_id in 15..=26 {
            let mut report = clean_feedback(report_id);
            report.decode_p95_ms = None;
            let decision = controller_decide(&mut controller, &report, report_id - 1);
            assert_eq!(decision.state, AdaptiveBitrateStateV1::Hold);
            assert_eq!(decision.target_kbps, 9_000);
        }
        assert_eq!(controller.clean_windows, 0);

        let mut pressure = clean_feedback(27);
        pressure.decode_p95_ms = None;
        pressure.decode_queue_capacity = 2;
        pressure.decode_queue_depth = 2;
        let decision = controller_decide(&mut controller, &pressure, 26);
        assert_eq!(decision.state, AdaptiveBitrateStateV1::Decrease);
        assert_eq!(decision.target_kbps, 6_750);
        assert!(
            decision
                .reasons
                .contains(AdaptiveBitrateReasonFlagsV1::DECODE_BACKLOG)
        );
    }

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
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: Instant::now(),
            keyframe,
            codec_config,
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

    fn moq_test_config() -> HostConfig {
        HostConfig {
            identity_path: "identity".into(),
            state_path: "state".into(),
            source: VideoSource::TestPattern,
            width: 1280,
            height: 800,
            framerate: 60,
            codec: "h264".to_owned(),
            input_mode: crate::config::InputMode::Disabled,
            uinput: None,
            ffmpeg_path: "ffmpeg".into(),
            gamescope_pipewire: None,
            audio: None,
        }
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

    fn endpoint(byte: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[byte; 32]).public()
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
            capture_timestamp_micros: 0,
            presentation_timestamp_micros: 0,
            observed_at: now,
            keyframe: true,
            codec_config: true,
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
    fn only_one_remote_can_hold_session() {
        let sessions = Arc::new(SessionRegistry::default());
        let nonce = [7; 16];
        let first = sessions
            .claim(endpoint(1), nonce, InvitationGrants::ALL)
            .unwrap();
        assert!(
            sessions
                .claim(endpoint(2), nonce, InvitationGrants::ALL)
                .is_err()
        );
        assert!(sessions.claim_input(endpoint(1), [8; 16]).is_err());
        let input = sessions.claim_input(endpoint(1), nonce).unwrap();
        assert_eq!(input.session_id, first.session_id);
        assert!(sessions.claim_input(endpoint(1), nonce).is_err());
        let audio = sessions.claim_audio(endpoint(1), nonce).unwrap();
        assert!(sessions.claim_audio(endpoint(1), nonce).is_err());
        drop(input);
        let draining_input = sessions.claim_input(endpoint(1), nonce).unwrap();
        drop(first);
        assert!(
            sessions
                .claim(endpoint(2), nonce, InvitationGrants::ALL)
                .is_err()
        );
        drop(draining_input);
        assert!(
            sessions
                .claim(endpoint(2), nonce, InvitationGrants::ALL)
                .is_err()
        );
        drop(audio);
        assert!(
            sessions
                .claim(endpoint(2), nonce, InvitationGrants::ALL)
                .is_ok()
        );
    }

    fn test_moq_broadcast() -> (BroadcastProducer, BroadcastConsumer) {
        let producer = Broadcast::new().produce();
        let consumer = producer.consume();
        (producer, consumer)
    }

    #[test]
    fn moq_attachment_requires_exact_active_control_remote_and_is_single_use() {
        let sessions = Arc::new(SessionRegistry::default());
        assert!(sessions.claim_moq(endpoint(1)).is_err());
        let lease = sessions
            .claim(endpoint(1), [1; 16], InvitationGrants::VIEW)
            .unwrap();
        let (_producer, consumer) = test_moq_broadcast();
        let _wait = sessions
            .expect_moq(
                endpoint(1),
                lease.session_id,
                media_moq_broadcast_name(lease.session_id).unwrap(),
                consumer,
            )
            .unwrap();

        // A wrong peer cannot consume the exact pending attachment.
        assert!(sessions.claim_moq(endpoint(2)).is_err());
        let attachment = sessions.claim_moq(endpoint(1)).unwrap();
        assert_eq!(attachment.session_id, lease.session_id);
        assert_eq!(
            attachment.broadcast_name,
            media_moq_broadcast_name(lease.session_id).unwrap()
        );
        // The pending token was atomically consumed before the MoQ handshake.
        assert!(sessions.claim_moq(endpoint(1)).is_err());
    }

    #[test]
    fn competing_moq_connections_cannot_both_claim_one_control_attachment() {
        let sessions = Arc::new(SessionRegistry::default());
        let lease = sessions
            .claim(endpoint(1), [1; 16], InvitationGrants::VIEW)
            .unwrap();
        let (_producer, consumer) = test_moq_broadcast();
        let _wait = sessions
            .expect_moq(
                endpoint(1),
                lease.session_id,
                media_moq_broadcast_name(lease.session_id).unwrap(),
                consumer,
            )
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let contenders = (0..2)
            .map(|_| {
                let sessions = Arc::clone(&sessions);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    sessions.claim_moq(endpoint(1)).is_ok()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let claimed = contenders
            .into_iter()
            .map(|thread| usize::from(thread.join().unwrap()))
            .sum::<usize>();
        assert_eq!(claimed, 1);
    }

    #[tokio::test]
    async fn releasing_control_clears_an_unclaimed_moq_attachment() {
        let sessions = Arc::new(SessionRegistry::default());
        let lease = sessions
            .claim(endpoint(1), [1; 16], InvitationGrants::VIEW)
            .unwrap();
        let (_producer, consumer) = test_moq_broadcast();
        let wait = sessions
            .expect_moq(
                endpoint(1),
                lease.session_id,
                media_moq_broadcast_name(lease.session_id).unwrap(),
                consumer,
            )
            .unwrap();
        drop(lease);
        assert!(sessions.claim_moq(endpoint(1)).is_err());
        assert!(wait.attached.await.is_err());
        assert!(wait.closed.await.is_err());
    }

    #[test]
    fn feedback_attaches_only_to_exact_active_view_session() {
        let sessions = Arc::new(SessionRegistry::default());
        let nonce = [9; 16];
        assert!(sessions.claim_feedback(endpoint(1), nonce).is_err());

        let no_view = sessions
            .claim(endpoint(1), nonce, InvitationGrants::GAMEPAD)
            .unwrap();
        assert!(sessions.claim_feedback(endpoint(1), nonce).is_err());
        drop(no_view);

        let media = sessions
            .claim(endpoint(1), nonce, InvitationGrants::VIEW)
            .unwrap();
        assert!(sessions.claim_feedback(endpoint(2), nonce).is_err());
        assert!(sessions.claim_feedback(endpoint(1), [8; 16]).is_err());
        let feedback = sessions.claim_feedback(endpoint(1), nonce).unwrap();
        assert_eq!(feedback.session_id, media.session_id);
        assert!(feedback.encoder_control.is_none());
        assert!(sessions.claim_feedback(endpoint(1), nonce).is_err());

        drop(media);
        assert!(
            sessions
                .claim(endpoint(2), nonce, InvitationGrants::VIEW)
                .is_err(),
            "feedback teardown must keep the draining session isolated"
        );
        drop(feedback);
        assert!(
            sessions
                .claim(endpoint(2), nonce, InvitationGrants::VIEW)
                .is_ok()
        );
    }

    #[test]
    fn adaptive_encoder_proposals_are_bound_to_the_exact_active_generation() {
        let sessions = Arc::new(SessionRegistry::default());
        let remote = endpoint(1);
        let media = sessions
            .claim(remote, [3; 16], InvitationGrants::VIEW)
            .unwrap();
        let harness = crate::source::EncoderControlTestHarness::new();

        assert!(
            sessions
                .install_encoder_control(
                    endpoint(2),
                    media.session_id,
                    Some(harness.control.clone())
                )
                .is_err()
        );
        sessions
            .install_encoder_control(remote, media.session_id, Some(harness.control.clone()))
            .unwrap();
        let feedback = sessions.claim_feedback(remote, [3; 16]).unwrap();
        assert!(feedback.encoder_control.is_some());
        let proposal = sessions
            .propose_adaptive_encoder_update(remote, media.session_id, 8_000, true)
            .unwrap()
            .unwrap();
        assert_eq!(proposal.target_kbps, 8_000);
        assert!(proposal.force_keyframe_revision > Some(proposal.bitrate_revision));

        let old_session_id = media.session_id;
        drop(media);
        assert!(
            sessions
                .propose_adaptive_encoder_update(remote, old_session_id, 7_000, false)
                .is_err(),
            "a draining generation must not issue another encoder proposal"
        );
        drop(feedback);
    }

    #[tokio::test]
    async fn adaptive_encoder_commit_requires_exact_readback_and_tracks_recovery_separately() {
        let sessions = Arc::new(SessionRegistry::default());
        let remote = endpoint(1);
        let media = sessions
            .claim(remote, [4; 16], InvitationGrants::VIEW)
            .unwrap();
        let harness = crate::source::EncoderControlTestHarness::new();
        sessions
            .install_encoder_control(remote, media.session_id, Some(harness.control.clone()))
            .unwrap();

        let proposal = sessions
            .propose_adaptive_encoder_update(remote, media.session_id, 8_000, true)
            .unwrap()
            .unwrap();
        harness.status.send_modify(|status| {
            status.applied_bitrate_revision = Some(proposal.bitrate_revision);
            status.applied_bitrate_kbps = Some(8_000);
            status.requested_force_keyframe_revision = proposal.force_keyframe_revision;
            status.acknowledged_force_keyframe_revision = proposal.force_keyframe_revision;
        });
        assert!(matches!(
            commit_adaptive_encoder_proposal(&sessions, remote, media.session_id, &proposal).await,
            AdaptiveEncoderCommit::Applied
        ));
        assert!(matches!(
            await_adaptive_recovery_keyframe(&sessions, remote, media.session_id, &proposal).await,
            Some(Ok(()))
        ));

        let mismatch = sessions
            .propose_adaptive_encoder_update(remote, media.session_id, 7_000, false)
            .unwrap()
            .unwrap();
        harness.status.send_modify(|status| {
            status.applied_bitrate_revision = Some(mismatch.bitrate_revision);
            status.applied_bitrate_kbps = Some(7_250);
        });
        assert!(matches!(
            commit_adaptive_encoder_proposal(&sessions, remote, media.session_id, &mismatch).await,
            AdaptiveEncoderCommit::NotApplied(_)
        ));

        let stale_generation = sessions
            .propose_adaptive_encoder_update(remote, media.session_id, 6_000, false)
            .unwrap()
            .unwrap();
        let session_id = media.session_id;
        drop(media);
        assert!(matches!(
            commit_adaptive_encoder_proposal(&sessions, remote, session_id, &stale_generation)
                .await,
            AdaptiveEncoderCommit::GenerationEnded
        ));
    }

    #[test]
    fn failed_adaptive_application_commits_observation_but_not_target() {
        let mut committed =
            ShadowBitrateController::new(12_000, MediaV3TelemetrySnapshot::default());
        controller_decide(&mut committed, &clean_feedback(1), 0);
        let old_target = committed.target_kbps;
        let old_decision_id = committed.next_decision_id;
        let mut severe = clean_feedback(2);
        severe.flags = MediaFeedbackFlags::RESYNC_ACTIVE;

        let mut candidate = committed.clone();
        let decision = controller_decide(&mut candidate, &severe, 1);
        assert_eq!(decision.state, AdaptiveBitrateStateV1::Decrease);
        assert!(candidate.target_kbps < old_target);
        committed.commit_observation_from(&candidate);

        assert_eq!(committed.target_kbps, old_target);
        assert_eq!(committed.last_report_id, Some(2));
        assert_eq!(committed.next_decision_id, old_decision_id + 1);
        assert!(
            committed
                .decide(
                    &clean_feedback(3),
                    MediaV3TelemetrySnapshot::default(),
                    Instant::now() + Duration::from_secs(2)
                )
                .is_ok()
        );
    }

    #[test]
    fn adaptive_recovery_idr_is_only_requested_for_a_bitrate_change_during_resync() {
        let mut report = clean_feedback(1);
        let mut decision = AdaptiveBitrateDecisionV1 {
            decision_id: 1,
            report_id: 1,
            target_kbps: 8_000,
            floor_kbps: 1_000,
            ceiling_kbps: 12_000,
            state: AdaptiveBitrateStateV1::Decrease,
            reasons: AdaptiveBitrateReasonFlagsV1::RECEIVER_QUEUE,
            applied: false,
        };
        assert!(!adaptive_recovery_keyframe_required(&report, &decision));
        report.flags = MediaFeedbackFlags::RESYNC_ACTIVE;
        assert!(adaptive_recovery_keyframe_required(&report, &decision));
        decision.state = AdaptiveBitrateStateV1::Hold;
        assert!(!adaptive_recovery_keyframe_required(&report, &decision));
    }

    #[test]
    fn pending_handshakes_are_bounded() {
        let sessions = SessionRegistry::default();
        let permits: Vec<_> = (0..MAX_PENDING_HANDSHAKES)
            .map(|_| sessions.pending_handshakes.try_acquire().unwrap())
            .collect();
        assert!(sessions.pending_handshakes.try_acquire().is_err());
        drop(permits);
        assert!(sessions.pending_handshakes.try_acquire().is_ok());
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
    fn session_substreams_inherit_the_exact_enrollment_grant() {
        let sessions = Arc::new(SessionRegistry::default());
        let grants = InvitationGrants::VIEW.union(InvitationGrants::GAMEPAD);
        let media = sessions.claim(endpoint(1), [3; 16], grants).unwrap();
        let input = sessions.claim_input(endpoint(1), [3; 16]).unwrap();
        let audio = sessions.claim_audio(endpoint(1), [3; 16]).unwrap();
        assert_eq!(input.grants, grants);
        assert_eq!(audio.grants, grants);
        drop(input);
        drop(audio);
        drop(media);
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
    fn audio_claim_requires_the_active_remote_and_nonce() {
        let sessions = Arc::new(SessionRegistry::default());
        let media = sessions
            .claim(endpoint(1), [9; 16], InvitationGrants::ALL)
            .unwrap();
        assert!(sessions.claim_audio(endpoint(2), [9; 16]).is_err());
        assert!(sessions.claim_audio(endpoint(1), [8; 16]).is_err());
        let audio = sessions.claim_audio(endpoint(1), [9; 16]).unwrap();
        drop(media);
        assert!(
            sessions
                .claim(endpoint(2), [0; 16], InvitationGrants::ALL)
                .is_err()
        );
        drop(audio);
        assert!(
            sessions
                .claim(endpoint(2), [0; 16], InvitationGrants::ALL)
                .is_ok()
        );
    }

    #[test]
    fn media_and_audio_leases_share_one_session_clock() {
        let sessions = Arc::new(SessionRegistry::default());
        let media = sessions
            .claim(endpoint(1), [9; 16], InvitationGrants::ALL)
            .unwrap();
        let audio = sessions.claim_audio(endpoint(1), [9; 16]).unwrap();
        assert_eq!(media.session_clock, audio.session_clock);
    }

    #[test]
    fn current_gop_replay_is_complete_and_skips_already_sent_frames() {
        let frame = |sequence, keyframe| EncodedFrame {
            sequence,
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: std::time::Instant::now(),
            keyframe,
            codec_config: keyframe,
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
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: old_observation,
            keyframe,
            codec_config: keyframe,
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
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: old_observation,
            keyframe,
            codec_config: keyframe,
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
            capture_timestamp_micros: 11,
            presentation_timestamp_micros: 11,
            observed_at: observed_now - maximum_replay_age,
            keyframe: false,
            codec_config: false,
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
            capture_timestamp_micros: 11,
            presentation_timestamp_micros: 11,
            observed_at: observed_now - maximum_replay_age - Duration::from_nanos(1),
            keyframe: false,
            codec_config: false,
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
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: observed_now,
            keyframe,
            codec_config: keyframe,
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
