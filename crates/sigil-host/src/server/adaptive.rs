use super::session::{
    AdaptiveEncoderProposal, MediaV3TelemetrySnapshot, ResolutionEncoderProposal, SessionRegistry,
};
use super::*;

const ADAPTIVE_BITRATE_FLOOR_KBPS: u32 = 1_000;
const SHADOW_BITRATE_CEILING_KBPS: u32 = 12_000;
const ADAPTIVE_BITRATE_QUANTUM_KBPS: u32 = 250;
const ADAPTIVE_BITRATE_CLEAN_WINDOWS: u8 = 10;
const ADAPTIVE_BITRATE_MODERATE_WINDOWS: u8 = 2;
const ADAPTIVE_BITRATE_COOLDOWN_WINDOWS: u8 = 10;
const MOTION_RESOLUTION_MODERATE_WINDOWS: u8 = 2;
const MOTION_RESOLUTION_CLEAN_WINDOWS: u8 = 3;
const MOTION_RESOLUTION_ACTIVITY_FPS: u64 = 10;
const MOTION_RESOLUTION_STILL_SETTLE: Duration = Duration::from_secs(2);
const MOTION_RESOLUTION_UPSCALE_COOLDOWN: Duration = Duration::from_secs(3);
const FEEDBACK_FRESH_INTERVAL_MIN_MS: u16 = 750;
const FEEDBACK_FRESH_INTERVAL_MAX_MS: u16 = 1_500;
const FEEDBACK_FRESH_ARRIVAL_GAP_MIN: Duration = Duration::from_millis(750);
const FEEDBACK_STALE_ARRIVAL_GAP: Duration = Duration::from_millis(2_500);
const FEEDBACK_MIN_READ_INTERVAL: Duration = Duration::from_millis(250);
const FEEDBACK_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
// Mirrors the fixed v1 wire bound. Aggregates clamp here so a stalled
// consumer observes a stale 5-second window instead of a deceptively fresh
// latest report.
const MEDIA_FEEDBACK_INTERVAL_MAX_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FeedbackSeverity {
    Clean,
    InsufficientEvidence,
    Moderate,
    Severe,
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VideoDimensions {
    pub width: u16,
    pub height: u16,
}

impl VideoDimensions {
    fn three_quarter(self) -> Result<Self> {
        ensure!(
            self.width.is_multiple_of(4) && self.height.is_multiple_of(4),
            "native dimensions must be divisible by four for an exact three-quarter tier"
        );
        let reduced = Self {
            width: self.width / 4 * 3,
            height: self.height / 4 * 3,
        };
        ensure!(
            reduced.width >= 64 && reduced.height >= 64,
            "three-quarter dimensions are below the H.264 minimum"
        );
        ensure!(
            reduced.width.is_multiple_of(2) && reduced.height.is_multiple_of(2),
            "three-quarter H.264 dimensions must be even"
        );
        ensure!(
            u32::from(self.width) * u32::from(reduced.height)
                == u32::from(self.height) * u32::from(reduced.width),
            "three-quarter dimensions must preserve the native aspect ratio"
        );
        Ok(reduced)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolutionTier {
    Native,
    Reduced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolutionTrigger {
    Hold,
    Motion,
    Pressure,
    Recovery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MotionResolutionDecision {
    tier: ResolutionTier,
    target: VideoDimensions,
    trigger: ResolutionTrigger,
    changed: bool,
}

/// Pure two-tier resolution policy. Fresh sequence progress from Gamescope's
/// damage-driven stream is the bounded motion proxy while the authenticated
/// feedback path supplies pressure. A downshift is safety-biased and
/// immediate; an upscale requires both settled motion and fresh clean
/// feedback, so stale reports can never restore detail.
#[derive(Clone, Debug)]
pub(crate) struct MotionResolutionPolicy {
    native: VideoDimensions,
    reduced: VideoDimensions,
    tier: ResolutionTier,
    motion_active: bool,
    still_since: Option<Instant>,
    pressure_active: bool,
    moderate_windows: u8,
    clean_windows: u8,
    last_transition_at: Option<Instant>,
    last_motion_sequence: Option<u64>,
}

impl MotionResolutionPolicy {
    pub(crate) fn new(native: VideoDimensions) -> Result<Self> {
        ensure!(
            native.width >= 64 && native.height >= 64,
            "native dimensions are too small"
        );
        ensure!(
            native.width.is_multiple_of(2) && native.height.is_multiple_of(2),
            "native H.264 dimensions must be even"
        );
        Ok(Self {
            native,
            reduced: native.three_quarter()?,
            tier: ResolutionTier::Native,
            motion_active: false,
            still_since: None,
            pressure_active: false,
            moderate_windows: 0,
            clean_windows: 0,
            last_transition_at: None,
            last_motion_sequence: None,
        })
    }

    pub(crate) fn target(&self) -> VideoDimensions {
        match self.tier {
            ResolutionTier::Native => self.native,
            ResolutionTier::Reduced => self.reduced,
        }
    }

    /// Host motion hook. Repeated still observations preserve the original
    /// settle boundary instead of indefinitely postponing recovery.
    #[cfg(test)]
    fn observe_motion(
        &mut self,
        motion_active: bool,
        observed_at: Instant,
    ) -> MotionResolutionDecision {
        self.update_motion(motion_active, observed_at);
        self.evaluate_target(observed_at)
    }

    fn update_motion(&mut self, motion_active: bool, observed_at: Instant) {
        if motion_active {
            self.motion_active = true;
            self.still_since = None;
            return;
        }
        if self.motion_active || self.still_since.is_none() {
            self.still_since = Some(observed_at);
        }
        self.motion_active = false;
    }

    /// Observe one classified feedback window. Only fresh moderate/severe
    /// pressure can initiate a downshift, and only fresh clean windows count
    /// toward restoring native detail.
    #[cfg(test)]
    fn observe_feedback(
        &mut self,
        severity: FeedbackSeverity,
        fresh: bool,
        observed_at: Instant,
    ) -> MotionResolutionDecision {
        self.update_feedback(severity, fresh);
        self.evaluate_target(observed_at)
    }

    /// Gamescope output is damage-driven, so fresh media sequence progress is
    /// a conservative constant-space motion proxy for this first slice. A
    /// future raw-frame observer can call `observe_motion` without changing
    /// pressure hysteresis or encoder actuation.
    fn observe_window(
        &mut self,
        report: &MediaFeedbackReportV1,
        evaluation: &AdaptiveBitrateEvaluation,
        observed_at: Instant,
    ) -> MotionResolutionDecision {
        let previous_sequence = self.last_motion_sequence;
        if let Some(sequence) = report.last_sequence {
            self.last_motion_sequence = Some(
                self.last_motion_sequence
                    .map_or(sequence, |last| last.max(sequence)),
            );
        }
        if evaluation.fresh {
            let motion_active = report.last_sequence.is_some_and(|sequence| {
                let delta = previous_sequence.map_or(0, |last| sequence.saturating_sub(last));
                delta.saturating_mul(1_000)
                    >= MOTION_RESOLUTION_ACTIVITY_FPS.saturating_mul(u64::from(report.interval_ms))
            });
            self.update_motion(motion_active, observed_at);
        }
        let resolution_severity = if evaluation.pressure_severity == FeedbackSeverity::Clean
            && !evaluation.clean_recovery_evidence
        {
            FeedbackSeverity::InsufficientEvidence
        } else {
            evaluation.pressure_severity
        };
        self.update_feedback(resolution_severity, evaluation.fresh);
        self.evaluate_target(observed_at)
    }

    fn update_feedback(&mut self, severity: FeedbackSeverity, fresh: bool) {
        if !fresh
            || matches!(
                severity,
                FeedbackSeverity::InsufficientEvidence | FeedbackSeverity::Stale
            )
        {
            self.moderate_windows = 0;
            self.clean_windows = 0;
            return;
        }

        match severity {
            FeedbackSeverity::Severe => {
                self.pressure_active = true;
                self.moderate_windows = 0;
                self.clean_windows = 0;
            }
            FeedbackSeverity::Moderate => {
                self.clean_windows = 0;
                self.moderate_windows = self.moderate_windows.saturating_add(1);
                if self.moderate_windows >= MOTION_RESOLUTION_MODERATE_WINDOWS {
                    self.moderate_windows = 0;
                    self.pressure_active = true;
                }
            }
            FeedbackSeverity::Clean => {
                self.moderate_windows = 0;
                self.clean_windows = self.clean_windows.saturating_add(1);
                if self.clean_windows >= MOTION_RESOLUTION_CLEAN_WINDOWS {
                    self.pressure_active = false;
                }
            }
            FeedbackSeverity::InsufficientEvidence | FeedbackSeverity::Stale => {}
        }
    }

    fn evaluate_target(&mut self, observed_at: Instant) -> MotionResolutionDecision {
        if self.motion_active {
            self.downshift(observed_at, ResolutionTrigger::Motion)
        } else if self.pressure_active {
            self.downshift(observed_at, ResolutionTrigger::Pressure)
        } else {
            self.maybe_restore(observed_at)
        }
    }

    fn downshift(
        &mut self,
        observed_at: Instant,
        trigger: ResolutionTrigger,
    ) -> MotionResolutionDecision {
        let changed = self.tier != ResolutionTier::Reduced;
        if changed {
            self.tier = ResolutionTier::Reduced;
            self.last_transition_at = Some(observed_at);
        }
        MotionResolutionDecision {
            tier: self.tier,
            target: self.target(),
            trigger,
            changed,
        }
    }

    fn maybe_restore(&mut self, observed_at: Instant) -> MotionResolutionDecision {
        let still_settled = !self.motion_active
            && self.still_since.is_some_and(|still_since| {
                observed_at.saturating_duration_since(still_since) >= MOTION_RESOLUTION_STILL_SETTLE
            });
        let cooldown_complete = self.last_transition_at.is_none_or(|last| {
            observed_at.saturating_duration_since(last) >= MOTION_RESOLUTION_UPSCALE_COOLDOWN
        });
        let clean = self.clean_windows >= MOTION_RESOLUTION_CLEAN_WINDOWS;
        if self.tier == ResolutionTier::Reduced
            && still_settled
            && cooldown_complete
            && clean
            && !self.pressure_active
        {
            self.tier = ResolutionTier::Native;
            self.clean_windows = 0;
            self.last_transition_at = Some(observed_at);
            return MotionResolutionDecision {
                tier: self.tier,
                target: self.target(),
                trigger: ResolutionTrigger::Recovery,
                changed: true,
            };
        }
        self.hold()
    }

    fn hold(&self) -> MotionResolutionDecision {
        MotionResolutionDecision {
            tier: self.tier,
            target: self.target(),
            trigger: ResolutionTrigger::Hold,
            changed: false,
        }
    }
}

fn motion_resolution_policy(width: u32, height: u32) -> Result<Option<MotionResolutionPolicy>> {
    let native = VideoDimensions {
        width: u16::try_from(width).context("native width exceeds protocol")?,
        height: u16::try_from(height).context("native height exceeds protocol")?,
    };
    match MotionResolutionPolicy::new(native) {
        Ok(policy) => Ok(Some(policy)),
        Err(error) => {
            warn!(
                %error,
                width,
                height,
                "motion resolution disabled because this native mode has no exact reduced tier"
            );
            Ok(None)
        }
    }
}

fn motion_resolution_policy_for_encoder(
    control: &EncoderControl,
) -> Result<Option<MotionResolutionPolicy>> {
    let (width, height) = control.initial_dimensions();
    motion_resolution_policy(u32::from(width), u32::from(height))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FeedbackIngressCursor {
    last_report_id: Option<u64>,
    interval_ms: u64,
    transport_dropped: u64,
    frontend_dropped: u64,
    decoder_dropped: u64,
    presenter_dropped: u64,
    resync_reports: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CumulativeFeedbackIngress {
    latest: Option<MediaFeedbackReportV1>,
    interval_ms: u64,
    transport_dropped: u64,
    frontend_dropped: u64,
    decoder_dropped: u64,
    presenter_dropped: u64,
    resync_reports: u64,
}

impl CumulativeFeedbackIngress {
    fn observe(&mut self, report: MediaFeedbackReportV1) {
        self.interval_ms = self.interval_ms.wrapping_add(u64::from(report.interval_ms));
        self.transport_dropped = self
            .transport_dropped
            .wrapping_add(u64::from(report.transport_dropped_delta));
        self.frontend_dropped = self
            .frontend_dropped
            .wrapping_add(u64::from(report.frontend_dropped_delta));
        self.decoder_dropped = self
            .decoder_dropped
            .wrapping_add(u64::from(report.decoder_dropped_delta));
        self.presenter_dropped = self
            .presenter_dropped
            .wrapping_add(u64::from(report.presenter_dropped_delta));
        if report.flags.contains(MediaFeedbackFlags::RESYNC_ACTIVE) {
            self.resync_reports = self.resync_reports.wrapping_add(1);
        }
        self.latest = Some(report);
    }

    fn report_since(
        &self,
        cursor: FeedbackIngressCursor,
    ) -> Result<Option<(MediaFeedbackReportV1, FeedbackIngressCursor)>> {
        let Some(latest) = self.latest else {
            return Ok(None);
        };
        if cursor
            .last_report_id
            .is_some_and(|report_id| latest.report_id <= report_id)
        {
            return Ok(None);
        }
        let mut report = latest;
        report.interval_ms = u16::try_from(
            self.interval_ms
                .wrapping_sub(cursor.interval_ms)
                .min(MEDIA_FEEDBACK_INTERVAL_MAX_MS),
        )
        .expect("feedback interval is clamped to u16");
        report.transport_dropped_delta =
            cumulative_feedback_delta(self.transport_dropped, cursor.transport_dropped);
        report.frontend_dropped_delta =
            cumulative_feedback_delta(self.frontend_dropped, cursor.frontend_dropped);
        report.decoder_dropped_delta =
            cumulative_feedback_delta(self.decoder_dropped, cursor.decoder_dropped);
        report.presenter_dropped_delta =
            cumulative_feedback_delta(self.presenter_dropped, cursor.presenter_dropped);
        report.flags = if self.resync_reports.wrapping_sub(cursor.resync_reports) > 0 {
            MediaFeedbackFlags::RESYNC_ACTIVE
        } else {
            MediaFeedbackFlags::NONE
        };
        report.validate()?;
        Ok(Some((
            report,
            FeedbackIngressCursor {
                last_report_id: Some(latest.report_id),
                interval_ms: self.interval_ms,
                transport_dropped: self.transport_dropped,
                frontend_dropped: self.frontend_dropped,
                decoder_dropped: self.decoder_dropped,
                presenter_dropped: self.presenter_dropped,
                resync_reports: self.resync_reports,
            },
        )))
    }
}

fn cumulative_feedback_delta(total: u64, baseline: u64) -> u32 {
    u32::try_from(total.wrapping_sub(baseline).min(u64::from(u32::MAX)))
        .expect("feedback delta is clamped to u32")
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

#[derive(Clone, Copy, Debug)]
struct AdaptiveBitrateEvaluation {
    decision: AdaptiveBitrateDecisionV1,
    pressure_severity: FeedbackSeverity,
    clean_recovery_evidence: bool,
    fresh: bool,
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

    fn evaluate(
        &mut self,
        report: &MediaFeedbackReportV1,
        telemetry: MediaV3TelemetrySnapshot,
        received_at: Instant,
    ) -> Result<AdaptiveBitrateEvaluation> {
        ensure!(
            self.last_report_id
                .is_none_or(|last| report.report_id > last),
            "feedback report IDs must increase monotonically"
        );
        let (severity, pressure_severity, reasons, stale, trusted_host_pressure) =
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
        Ok(AdaptiveBitrateEvaluation {
            decision,
            pressure_severity,
            clean_recovery_evidence: severity == FeedbackSeverity::Clean,
            fresh: !stale,
        })
    }

    #[cfg(test)]
    fn decide(
        &mut self,
        report: &MediaFeedbackReportV1,
        telemetry: MediaV3TelemetrySnapshot,
        received_at: Instant,
    ) -> Result<AdaptiveBitrateDecisionV1> {
        self.evaluate(report, telemetry, received_at)
            .map(|evaluation| evaluation.decision)
    }

    fn classify(
        &mut self,
        report: &MediaFeedbackReportV1,
        telemetry: MediaV3TelemetrySnapshot,
        received_at: Instant,
    ) -> (
        FeedbackSeverity,
        FeedbackSeverity,
        AdaptiveBitrateReasonFlagsV1,
        bool,
        bool,
    ) {
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
            return (severity, severity, reasons, true, trusted_host_pressure);
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

        let pressure_severity = severity;
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
        (
            severity,
            pressure_severity,
            reasons,
            false,
            trusted_host_pressure,
        )
    }

    #[cfg(test)]
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

#[derive(Clone, Copy, Debug)]
struct AdaptiveCommitPlan {
    decision: AdaptiveBitrateDecisionV1,
    telemetry: MediaV3TelemetrySnapshot,
    resolution_decision: Option<MotionResolutionDecision>,
    force_keyframe: bool,
}

#[derive(Debug)]
struct AdaptiveCommitAcknowledgement {
    plan: AdaptiveCommitPlan,
    generation_ended: bool,
    recovery_acknowledged: Option<bool>,
    resolution_applied: Option<bool>,
    applied_bitrate_kbps: Option<u32>,
    applied_resolution: Option<VideoDimensions>,
}

impl AdaptiveCommitAcknowledgement {
    fn immediate(plan: AdaptiveCommitPlan) -> Self {
        Self {
            plan,
            generation_ended: false,
            recovery_acknowledged: None,
            resolution_applied: None,
            applied_bitrate_kbps: None,
            applied_resolution: None,
        }
    }
}

/// Bounded adaptive-control actuator.
///
/// Feedback evaluation remains synchronous and deterministic, while this
/// coordinator owns the slower encoder acknowledgements. At most one commit
/// task and one latest coalesced plan exist. A queued plan carries the latest
/// desired bitrate and resolution even when its feedback decision is a hold,
/// so a slow or failed predecessor cannot strand an uncommitted target.
struct AdaptiveCommitCoordinator {
    sessions: Arc<SessionRegistry>,
    remote: EndpointId,
    session_id: u64,
    actuation_enabled: bool,
    committed_bitrate_kbps: u32,
    committed_resolution: Option<VideoDimensions>,
    pending: bool,
    queued: Option<AdaptiveCommitPlan>,
    acknowledgements: tokio::task::JoinSet<AdaptiveCommitAcknowledgement>,
}

impl AdaptiveCommitCoordinator {
    fn new(
        sessions: Arc<SessionRegistry>,
        remote: EndpointId,
        session_id: u64,
        actuation_enabled: bool,
        committed_bitrate_kbps: u32,
        committed_resolution: Option<VideoDimensions>,
    ) -> Self {
        Self {
            sessions,
            remote,
            session_id,
            actuation_enabled,
            committed_bitrate_kbps,
            committed_resolution,
            pending: false,
            queued: None,
            acknowledgements: tokio::task::JoinSet::new(),
        }
    }

    fn has_pending(&self) -> bool {
        self.pending
    }

    fn submit(&mut self, plan: AdaptiveCommitPlan) -> Option<AdaptiveCommitAcknowledgement> {
        if self.pending {
            self.queued = Some(plan);
            return None;
        }
        self.start(plan)
    }

    fn start_queued(&mut self) -> Option<AdaptiveCommitAcknowledgement> {
        let plan = self.queued.take()?;
        self.start(plan)
    }

    fn start(&mut self, plan: AdaptiveCommitPlan) -> Option<AdaptiveCommitAcknowledgement> {
        if !self.actuation_enabled {
            return Some(AdaptiveCommitAcknowledgement::immediate(plan));
        }

        let desired_resolution = plan.resolution_decision.map(|change| change.target);
        let changes_resolution =
            desired_resolution.is_some() && desired_resolution != self.committed_resolution;
        let bitrate_proposal = if plan.decision.target_kbps != self.committed_bitrate_kbps {
            match self.sessions.propose_adaptive_encoder_update(
                self.remote,
                self.session_id,
                plan.decision.target_kbps,
                plan.force_keyframe && !changes_resolution,
            ) {
                Ok(Some(proposal)) => Some(proposal),
                Ok(None) => {
                    warn!(
                        remote = %self.remote,
                        session_id = self.session_id,
                        "active adaptive session lost encoder control; retaining committed bitrate"
                    );
                    None
                }
                Err(error) => {
                    warn!(
                        remote = %self.remote,
                        session_id = self.session_id,
                        %error,
                        "adaptive encoder proposal failed; retaining committed bitrate"
                    );
                    None
                }
            }
        } else {
            None
        };

        let resolution_proposal = if changes_resolution {
            let target = desired_resolution.expect("resolution target was checked as present");
            match self
                .sessions
                .propose_resolution_update(self.remote, self.session_id, target)
            {
                Ok(Some(proposal)) => Some(proposal),
                Ok(None) => {
                    warn!(
                        remote = %self.remote,
                        session_id = self.session_id,
                        "active adaptive session lost encoder control; retaining committed resolution"
                    );
                    None
                }
                Err(error) => {
                    warn!(
                        remote = %self.remote,
                        session_id = self.session_id,
                        %error,
                        "adaptive resolution proposal failed; retaining committed resolution"
                    );
                    None
                }
            }
        } else {
            None
        };

        if bitrate_proposal.is_none() && resolution_proposal.is_none() {
            return Some(AdaptiveCommitAcknowledgement::immediate(plan));
        }

        self.pending = true;
        let sessions = Arc::clone(&self.sessions);
        let remote = self.remote;
        let session_id = self.session_id;
        self.acknowledgements.spawn(async move {
            run_adaptive_commit(
                sessions,
                remote,
                session_id,
                plan,
                bitrate_proposal,
                resolution_proposal,
            )
            .await
        });
        None
    }

    fn complete(
        &mut self,
        result: Option<std::result::Result<AdaptiveCommitAcknowledgement, tokio::task::JoinError>>,
    ) -> Result<AdaptiveCommitAcknowledgement> {
        ensure!(
            self.pending,
            "adaptive commit completed without pending work"
        );
        self.pending = false;
        let acknowledgement = result
            .context("adaptive commit task ended without a result")?
            .context("adaptive commit task failed")?;
        if let Some(applied) = acknowledgement.applied_bitrate_kbps {
            self.committed_bitrate_kbps = applied;
        }
        if let Some(applied) = acknowledgement.applied_resolution {
            self.committed_resolution = Some(applied);
        }
        Ok(acknowledgement)
    }

    async fn abort_and_drain(&mut self) {
        self.pending = false;
        self.queued = None;
        self.acknowledgements.abort_all();
        while self.acknowledgements.join_next().await.is_some() {}
    }
}

pub(super) async fn serve_media_feedback(
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

    let (feedback_sender, mut feedback_receiver) =
        tokio::sync::watch::channel(CumulativeFeedbackIngress::default());
    let mut reader_task = tokio::spawn(forward_media_feedback_reports(recv, feedback_sender));
    let mut reader_finished = false;
    let mut receiver_open = true;
    let mut controller = ShadowBitrateController::new(ceiling_kbps, lease.telemetry.snapshot());
    let mut resolution_controller = if encoder_actuation_available {
        lease
            .encoder_control
            .as_ref()
            .map(motion_resolution_policy_for_encoder)
            .transpose()?
            .flatten()
    } else {
        None
    };
    let initial_resolution = resolution_controller
        .as_ref()
        .map(MotionResolutionPolicy::target);
    let mut commit_coordinator = AdaptiveCommitCoordinator::new(
        Arc::clone(sessions),
        remote,
        lease.session_id,
        encoder_actuation_available,
        ceiling_kbps,
        initial_resolution,
    );
    let mut feedback_cursor = FeedbackIngressCursor::default();
    let mut ready_acknowledgement: Option<AdaptiveCommitAcknowledgement> = None;

    let result: Result<()> = loop {
        if let Some(acknowledgement) = ready_acknowledgement.take() {
            if acknowledgement.generation_ended {
                break Ok(());
            }
            write_adaptive_acknowledgement(
                &mut send,
                remote,
                lease.session_id,
                encoder_actuation_available,
                &acknowledgement,
            )
            .await?;
            ready_acknowledgement = commit_coordinator.start_queued();
            continue;
        }

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
            commit_result = commit_coordinator.acknowledgements.join_next(),
                if commit_coordinator.has_pending() =>
            {
                let acknowledgement = commit_coordinator.complete(commit_result)?;
                ready_acknowledgement = Some(acknowledgement);
            }
            changed = feedback_receiver.changed(), if receiver_open => {
                if changed.is_err() {
                    receiver_open = false;
                    if reader_finished {
                        break Ok(());
                    }
                    continue;
                }
                let ingress = *feedback_receiver.borrow_and_update();
                let Some((report, next_cursor)) = ingress.report_since(feedback_cursor)? else {
                    continue;
                };
                // This branch now owns the complete aggregate. Advance the
                // ingress baseline exactly once before evaluation; reports
                // arriving during an encoder wait accumulate beyond it.
                feedback_cursor = next_cursor;
                let telemetry = lease.telemetry.snapshot();
                let observed_at = Instant::now();
                let mut candidate = controller.clone();
                let evaluation = candidate.evaluate(&report, telemetry, observed_at)?;
                let decision = evaluation.decision;
                let mut resolution_candidate = resolution_controller.clone();
                let resolution_decision = resolution_candidate
                    .as_mut()
                    .map(|policy| policy.observe_window(&report, &evaluation, observed_at));
                let force_keyframe =
                    adaptive_recovery_keyframe_required(&report, &decision);
                // Evaluation state advances at feedback cadence. The actuator
                // independently retains the exact committed target and retries
                // the latest desired state after a slow or failed predecessor.
                controller = candidate;
                resolution_controller = resolution_candidate;
                ready_acknowledgement =
                    commit_coordinator.submit(AdaptiveCommitPlan {
                        decision,
                        telemetry,
                        resolution_decision,
                        force_keyframe,
                    });
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
        if reader_finished
            && !receiver_open
            && !commit_coordinator.has_pending()
            && commit_coordinator.queued.is_none()
        {
            break Ok(());
        }
    };

    if !reader_finished {
        reader_task.abort();
        let _ = reader_task.await;
    }
    commit_coordinator.abort_and_drain().await;
    let _ = send.finish();
    drop(lease);
    info!(%remote, "media feedback client released");
    result
}

async fn write_adaptive_acknowledgement(
    send: &mut SendStream,
    remote: EndpointId,
    session_id: u64,
    encoder_actuation_available: bool,
    acknowledgement: &AdaptiveCommitAcknowledgement,
) -> Result<()> {
    let decision = acknowledgement.plan.decision;
    let telemetry = acknowledgement.plan.telemetry;
    let resolution_decision = acknowledgement.plan.resolution_decision;
    tokio::time::timeout(
        FEEDBACK_WRITE_TIMEOUT,
        write_adaptive_bitrate_decision_v1(send, &decision),
    )
    .await
    .context("timed out writing adaptive bitrate decision")??;
    debug!(
        %remote,
        session_id,
        report_id = decision.report_id,
        decision_id = decision.decision_id,
        target_kbps = decision.target_kbps,
        ?decision.state,
        reasons = decision.reasons.bits(),
        applied = decision.applied,
        recovery_acknowledged = ?acknowledgement.recovery_acknowledged,
        resolution_width = resolution_decision.map(|change| change.target.width),
        resolution_height = resolution_decision.map(|change| change.target.height),
        resolution_trigger = ?resolution_decision.map(|change| change.trigger),
        resolution_applied = ?acknowledgement.resolution_applied,
        path_rtt_micros = telemetry.selected_path_rtt_micros,
        path_lost_packets = telemetry.selected_path_lost_packets,
        path_congestion_events = telemetry.selected_path_congestion_events,
        scheduler_cancellations = telemetry.scheduler_cancellations,
        send_failures = telemetry.send_failures,
        mode = if encoder_actuation_available { "active" } else { "shadow" },
        "adaptive bitrate decision"
    );
    Ok(())
}

async fn run_adaptive_commit(
    sessions: Arc<SessionRegistry>,
    remote: EndpointId,
    session_id: u64,
    plan: AdaptiveCommitPlan,
    bitrate_proposal: Option<AdaptiveEncoderProposal>,
    resolution_proposal: Option<ResolutionEncoderProposal>,
) -> AdaptiveCommitAcknowledgement {
    let mut acknowledgement = AdaptiveCommitAcknowledgement::immediate(plan);
    if let Some(proposal) = bitrate_proposal {
        match commit_adaptive_encoder_proposal(&sessions, remote, session_id, &proposal).await {
            AdaptiveEncoderCommit::GenerationEnded => {
                acknowledgement.generation_ended = true;
                return acknowledgement;
            }
            AdaptiveEncoderCommit::NotApplied(error) => {
                warn!(
                    %remote,
                    session_id,
                    %error,
                    "adaptive encoder application failed; retaining committed bitrate"
                );
            }
            AdaptiveEncoderCommit::Applied => {
                acknowledgement.plan.decision.applied = true;
                acknowledgement.applied_bitrate_kbps = Some(proposal.target_kbps);
                if proposal.force_keyframe_revision.is_some() {
                    match await_adaptive_recovery_keyframe(&sessions, remote, session_id, &proposal)
                        .await
                    {
                        None => {
                            acknowledgement.generation_ended = true;
                            return acknowledgement;
                        }
                        Some(Ok(())) => acknowledgement.recovery_acknowledged = Some(true),
                        Some(Err(error)) => {
                            acknowledgement.recovery_acknowledged = Some(false);
                            warn!(
                                %remote,
                                session_id,
                                %error,
                                "adaptive bitrate applied but forced-IDR recovery was not acknowledged"
                            );
                        }
                    }
                }
            }
        }
    }

    if let Some(proposal) = resolution_proposal {
        match commit_resolution_encoder_proposal(&sessions, remote, session_id, &proposal).await {
            AdaptiveEncoderCommit::GenerationEnded => {
                acknowledgement.generation_ended = true;
                return acknowledgement;
            }
            AdaptiveEncoderCommit::NotApplied(error) => {
                acknowledgement.resolution_applied = Some(false);
                warn!(
                    %remote,
                    session_id,
                    target_width = proposal.target.width,
                    target_height = proposal.target.height,
                    %error,
                    "adaptive resolution application failed; retaining committed resolution"
                );
            }
            AdaptiveEncoderCommit::Applied => {
                acknowledgement.resolution_applied = Some(true);
                acknowledgement.applied_resolution = Some(proposal.target);
            }
        }
    }
    acknowledgement
}

async fn forward_media_feedback_reports<R>(
    mut reader: R,
    sender: tokio::sync::watch::Sender<CumulativeFeedbackIngress>,
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
        sender.send_modify(|ingress| ingress.observe(report));
        if sender.is_closed() {
            return Ok(());
        }
        next_read_at = tokio::time::Instant::now() + FEEDBACK_MIN_READ_INTERVAL;
    }
}

fn adaptive_bitrate_ceiling_kbps(config: &HostConfig) -> Result<u32> {
    let configured = match config.source {
        VideoSource::TestPattern => SHADOW_BITRATE_CEILING_KBPS,
        VideoSource::GamescopePipewire => {
            let gamescope = config
                .gamescope_pipewire
                .as_ref()
                .context("Gamescope feedback requires gamescope_pipewire configuration")?;
            match gamescope.rate_control {
                VaapiRateControl::Cbr => gamescope
                    .bitrate_kbps
                    .context("adaptive bitrate shadow mode requires a Gamescope CBR bitrate")?,
                VaapiRateControl::Cqp => SHADOW_BITRATE_CEILING_KBPS,
            }
        }
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

async fn commit_resolution_encoder_proposal(
    sessions: &SessionRegistry,
    remote: EndpointId,
    session_id: u64,
    proposal: &ResolutionEncoderProposal,
) -> AdaptiveEncoderCommit {
    let result = await_while_session_active(
        sessions,
        remote,
        session_id,
        proposal.control.wait_for_resolution_applied(
            proposal.revision,
            proposal.target.width,
            proposal.target.height,
            ENCODER_CONTROL_COMMIT_TIMEOUT,
        ),
    )
    .await;
    let Some(result) = result else {
        return AdaptiveEncoderCommit::GenerationEnded;
    };
    match result {
        Ok(status)
            if status.applied_resolution_revision == Some(proposal.revision)
                && status.applied_width == Some(proposal.target.width)
                && status.applied_height == Some(proposal.target.height) =>
        {
            AdaptiveEncoderCommit::Applied
        }
        Ok(status) => AdaptiveEncoderCommit::NotApplied(anyhow::anyhow!(
            "encoder resolution commit mismatch: revision {:?}, dimensions {:?}x{:?}, expected revision {} at {}x{}",
            status.applied_resolution_revision,
            status.applied_width,
            status.applied_height,
            proposal.revision,
            proposal.target.width,
            proposal.target.height
        )),
        Err(error) => AdaptiveEncoderCommit::NotApplied(error),
    }
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

#[cfg(test)]
mod tests {
    use super::super::{endpoint, moq_test_config};
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

    fn evaluate_commit_plan(
        controller: &mut ShadowBitrateController,
        resolution_controller: &mut Option<MotionResolutionPolicy>,
        report: &MediaFeedbackReportV1,
        observed_at: Instant,
    ) -> AdaptiveCommitPlan {
        let telemetry = MediaV3TelemetrySnapshot::default();
        let evaluation = controller.evaluate(report, telemetry, observed_at).unwrap();
        let resolution_decision = resolution_controller
            .as_mut()
            .map(|policy| policy.observe_window(report, &evaluation, observed_at));
        AdaptiveCommitPlan {
            decision: evaluation.decision,
            telemetry,
            resolution_decision,
            force_keyframe: adaptive_recovery_keyframe_required(report, &evaluation.decision),
        }
    }

    fn native_dimensions() -> VideoDimensions {
        VideoDimensions {
            width: 1_280,
            height: 800,
        }
    }

    #[test]
    fn motion_resolution_tier_is_exact_even_and_same_aspect() {
        let native = native_dimensions();
        let policy = MotionResolutionPolicy::new(native).unwrap();
        assert_eq!(policy.target(), native);
        assert_eq!(
            policy.reduced,
            VideoDimensions {
                width: 960,
                height: 600,
            }
        );
        assert_eq!(
            u32::from(native.width) * u32::from(policy.reduced.height),
            u32::from(native.height) * u32::from(policy.reduced.width)
        );
        assert!(policy.reduced.width.is_multiple_of(2));
        assert!(policy.reduced.height.is_multiple_of(2));

        assert!(
            MotionResolutionPolicy::new(VideoDimensions {
                width: 1_278,
                height: 800,
            })
            .is_err()
        );
        assert!(
            MotionResolutionPolicy::new(VideoDimensions {
                width: 80,
                height: 64,
            })
            .is_err()
        );
        assert!(motion_resolution_policy(1_920, 1_080).unwrap().is_some());
        assert!(motion_resolution_policy(1_366, 768).unwrap().is_none());
    }

    #[test]
    fn feedback_resolution_policy_uses_encoder_generation_dimensions() {
        let full_hd = crate::source::EncoderControlTestHarness::with_dimensions(1_920, 1_080);
        let policy = motion_resolution_policy_for_encoder(&full_hd.control)
            .unwrap()
            .unwrap();
        assert_eq!(
            policy.native,
            VideoDimensions {
                width: 1_920,
                height: 1_080,
            }
        );

        let no_exact_tier = crate::source::EncoderControlTestHarness::with_dimensions(1_366, 768);
        assert!(
            motion_resolution_policy_for_encoder(&no_exact_tier.control)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn motion_downshifts_immediately_and_recovers_only_after_settle_and_clean_windows() {
        let now = Instant::now();
        let mut policy = MotionResolutionPolicy::new(native_dimensions()).unwrap();
        let down = policy.observe_motion(true, now);
        assert_eq!(down.tier, ResolutionTier::Reduced);
        assert_eq!(down.trigger, ResolutionTrigger::Motion);
        assert!(down.changed);

        let still = policy.observe_motion(false, now + Duration::from_secs(1));
        assert!(!still.changed);
        for second in [2, 3] {
            assert!(
                !policy
                    .observe_feedback(
                        FeedbackSeverity::Clean,
                        true,
                        now + Duration::from_secs(second),
                    )
                    .changed
            );
        }
        let recovered =
            policy.observe_feedback(FeedbackSeverity::Clean, true, now + Duration::from_secs(4));
        assert_eq!(recovered.tier, ResolutionTier::Native);
        assert_eq!(recovered.target, native_dimensions());
        assert_eq!(recovered.trigger, ResolutionTrigger::Recovery);
        assert!(recovered.changed);
    }

    #[test]
    fn repeated_still_observations_do_not_postpone_resolution_recovery() {
        let now = Instant::now();
        let mut policy = MotionResolutionPolicy::new(native_dimensions()).unwrap();
        policy.observe_motion(true, now);
        policy.observe_motion(false, now + Duration::from_secs(1));
        policy.observe_feedback(FeedbackSeverity::Clean, true, now + Duration::from_secs(2));
        policy.observe_motion(false, now + Duration::from_millis(2_500));
        policy.observe_feedback(FeedbackSeverity::Clean, true, now + Duration::from_secs(3));
        let recovered =
            policy.observe_feedback(FeedbackSeverity::Clean, true, now + Duration::from_secs(4));
        assert!(recovered.changed);
        assert_eq!(recovered.tier, ResolutionTier::Native);
    }

    #[test]
    fn resolution_pressure_requires_fresh_hysteresis_but_severe_is_immediate() {
        let now = Instant::now();
        let mut moderate = MotionResolutionPolicy::new(native_dimensions()).unwrap();
        moderate.observe_motion(false, now);
        assert!(
            !moderate
                .observe_feedback(
                    FeedbackSeverity::Moderate,
                    true,
                    now + Duration::from_secs(1),
                )
                .changed
        );
        assert!(
            !moderate
                .observe_feedback(
                    FeedbackSeverity::Moderate,
                    false,
                    now + Duration::from_secs(2),
                )
                .changed
        );
        assert!(
            !moderate
                .observe_feedback(
                    FeedbackSeverity::Moderate,
                    true,
                    now + Duration::from_secs(3),
                )
                .changed
        );
        let down = moderate.observe_feedback(
            FeedbackSeverity::Moderate,
            true,
            now + Duration::from_secs(4),
        );
        assert!(down.changed);
        assert_eq!(down.trigger, ResolutionTrigger::Pressure);

        let mut severe = MotionResolutionPolicy::new(native_dimensions()).unwrap();
        let immediate =
            severe.observe_feedback(FeedbackSeverity::Severe, true, now + Duration::from_secs(1));
        assert!(immediate.changed);
        assert_eq!(immediate.tier, ResolutionTier::Reduced);
    }

    #[test]
    fn stale_feedback_cannot_restore_native_resolution() {
        let now = Instant::now();
        let mut policy = MotionResolutionPolicy::new(native_dimensions()).unwrap();
        policy.observe_motion(false, now);
        policy.observe_feedback(FeedbackSeverity::Severe, true, now + Duration::from_secs(1));
        for second in [2, 3, 4, 5] {
            let decision = policy.observe_feedback(
                FeedbackSeverity::Clean,
                false,
                now + Duration::from_secs(second),
            );
            assert_eq!(decision.tier, ResolutionTier::Reduced);
            assert!(!decision.changed);
        }
        assert!(policy.pressure_active);

        for second in [6, 7] {
            assert!(
                !policy
                    .observe_feedback(
                        FeedbackSeverity::Clean,
                        true,
                        now + Duration::from_secs(second),
                    )
                    .changed
            );
        }
        let recovered =
            policy.observe_feedback(FeedbackSeverity::Clean, true, now + Duration::from_secs(8));
        assert!(recovered.changed);
        assert_eq!(recovered.tier, ResolutionTier::Native);
    }

    #[test]
    fn motion_keeps_resolution_reduced_after_pressure_clears() {
        let now = Instant::now();
        let mut policy = MotionResolutionPolicy::new(native_dimensions()).unwrap();
        policy.observe_motion(true, now);
        policy.observe_feedback(FeedbackSeverity::Severe, true, now + Duration::from_secs(1));
        for second in 2..=5 {
            let decision = policy.observe_feedback(
                FeedbackSeverity::Clean,
                true,
                now + Duration::from_secs(second),
            );
            assert_eq!(decision.tier, ResolutionTier::Reduced);
            assert!(!decision.changed);
        }
        assert!(!policy.pressure_active);
        assert!(policy.motion_active);
    }

    #[test]
    fn damage_driven_motion_proxy_seeds_then_downshifts_and_stale_blocks_upscale() {
        let now = Instant::now();
        let telemetry = MediaV3TelemetrySnapshot::default();
        let mut bitrate = ShadowBitrateController::new(12_000, telemetry);
        let mut resolution = MotionResolutionPolicy::new(native_dimensions()).unwrap();

        let mut first = clean_feedback(1);
        first.last_sequence = Some(100);
        let first_evaluation = bitrate.evaluate(&first, telemetry, now).unwrap();
        assert!(!first_evaluation.fresh);
        let seeded = resolution.observe_window(&first, &first_evaluation, now);
        assert_eq!(seeded.tier, ResolutionTier::Native);
        assert!(!seeded.changed);

        let mut second = clean_feedback(2);
        second.last_sequence = Some(110);
        let second_evaluation = bitrate
            .evaluate(&second, telemetry, now + Duration::from_secs(1))
            .unwrap();
        assert!(second_evaluation.fresh);
        let down =
            resolution.observe_window(&second, &second_evaluation, now + Duration::from_secs(1));
        assert_eq!(down.trigger, ResolutionTrigger::Motion);
        assert_eq!(down.tier, ResolutionTier::Reduced);
        assert!(down.changed);

        let mut stale = clean_feedback(3);
        stale.interval_ms = 5_000;
        stale.last_sequence = Some(110);
        let stale_evaluation = bitrate
            .evaluate(&stale, telemetry, now + Duration::from_secs(5))
            .unwrap();
        assert!(!stale_evaluation.fresh);
        let held =
            resolution.observe_window(&stale, &stale_evaluation, now + Duration::from_secs(5));
        assert_eq!(held.tier, ResolutionTier::Reduced);
        assert!(!held.changed);
    }

    #[test]
    fn damage_driven_no_progress_cannot_restore_native_resolution() {
        let now = Instant::now();
        let telemetry = MediaV3TelemetrySnapshot::default();
        let mut bitrate = ShadowBitrateController::new(12_000, telemetry);
        let mut resolution = MotionResolutionPolicy::new(native_dimensions()).unwrap();

        let mut seed = clean_feedback(1);
        seed.last_sequence = Some(100);
        let seed_evaluation = bitrate.evaluate(&seed, telemetry, now).unwrap();
        resolution.observe_window(&seed, &seed_evaluation, now);

        let mut pressure = clean_feedback(2);
        pressure.last_sequence = Some(110);
        pressure.flags = MediaFeedbackFlags::RESYNC_ACTIVE;
        let pressure_evaluation = bitrate
            .evaluate(&pressure, telemetry, now + Duration::from_secs(1))
            .unwrap();
        let downshift = resolution.observe_window(
            &pressure,
            &pressure_evaluation,
            now + Duration::from_secs(1),
        );
        assert_eq!(downshift.tier, ResolutionTier::Reduced);
        assert!(downshift.changed);

        for second in 2..=4 {
            let mut no_progress = clean_feedback(second + 1);
            no_progress.last_sequence = Some(110);
            let evaluation = bitrate
                .evaluate(&no_progress, telemetry, now + Duration::from_secs(second))
                .unwrap();
            assert_eq!(evaluation.pressure_severity, FeedbackSeverity::Clean);
            assert!(!evaluation.clean_recovery_evidence);
            let decision = resolution.observe_window(
                &no_progress,
                &evaluation,
                now + Duration::from_secs(second),
            );
            assert_eq!(decision.tier, ResolutionTier::Reduced);
            assert_eq!(decision.trigger, ResolutionTrigger::Pressure);
            assert!(!decision.changed);
            assert_eq!(resolution.clean_windows, 0);
        }

        for (report_id, second, sequence) in [(6, 5, 111), (7, 6, 112), (8, 7, 113)] {
            let mut low_motion = clean_feedback(report_id);
            low_motion.last_sequence = Some(sequence);
            let evaluation = bitrate
                .evaluate(&low_motion, telemetry, now + Duration::from_secs(second))
                .unwrap();
            assert!(evaluation.clean_recovery_evidence);
            let decision = resolution.observe_window(
                &low_motion,
                &evaluation,
                now + Duration::from_secs(second),
            );
            if report_id < 8 {
                assert_eq!(decision.tier, ResolutionTier::Reduced);
                assert!(!decision.changed);
            } else {
                assert_eq!(decision.tier, ResolutionTier::Native);
                assert_eq!(decision.trigger, ResolutionTrigger::Recovery);
                assert!(decision.changed);
            }
        }
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

    fn gamescope_feedback_test_config(rate_control: VaapiRateControl) -> HostConfig {
        let (bitrate_kbps, quantizer) = match rate_control {
            VaapiRateControl::Cbr => (Some(12_123), None),
            VaapiRateControl::Cqp => (None, Some(24)),
        };
        let mut config = moq_test_config();
        config.source = VideoSource::GamescopePipewire;
        config.width = None;
        config.height = None;
        config.gamescope_pipewire = Some(crate::config::GamescopePipewireConfig {
            node_name: "gamescope".into(),
            media_class: "Video/Source".into(),
            match_properties: std::collections::BTreeMap::new(),
            xwayland_display: None,
            pw_dump_path: "/usr/bin/pw-dump".into(),
            gst_launch_path: "/usr/bin/gst-launch-1.0".into(),
            gst_inspect_path: "/usr/bin/gst-inspect-1.0".into(),
            encoder_backend: GamescopeEncoderBackend::ExternalGstLaunch,
            vaapi_encoder: "vah264enc".into(),
            vaapi_render_node: "/dev/dri/renderD128".into(),
            rate_control,
            bitrate_kbps,
            quantizer,
        });
        config.validate().unwrap();
        config
    }

    #[test]
    fn external_cqp_uses_a_fixed_shadow_bitrate_ceiling() {
        let config = gamescope_feedback_test_config(VaapiRateControl::Cqp);

        assert_eq!(
            adaptive_bitrate_ceiling_kbps(&config).unwrap(),
            SHADOW_BITRATE_CEILING_KBPS
        );
        assert!(!adaptive_bitrate_actuation_enabled(&config));
    }

    #[test]
    fn gamescope_cbr_keeps_its_quantized_configured_ceiling() {
        let config = gamescope_feedback_test_config(VaapiRateControl::Cbr);

        assert_eq!(adaptive_bitrate_ceiling_kbps(&config).unwrap(), 12_000);
    }

    #[test]
    fn malformed_cbr_without_a_bitrate_still_fails_closed() {
        let mut config = gamescope_feedback_test_config(VaapiRateControl::Cbr);
        config.gamescope_pipewire.as_mut().unwrap().bitrate_kbps = None;

        assert_eq!(
            adaptive_bitrate_ceiling_kbps(&config)
                .unwrap_err()
                .to_string(),
            "adaptive bitrate shadow mode requires a Gamescope CBR bitrate"
        );
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

    #[tokio::test]
    async fn stalled_adaptive_apply_coalesces_feedback_without_losing_pressure() {
        use tokio::io::AsyncWriteExt as _;

        let (sender, mut receiver) =
            tokio::sync::watch::channel(CumulativeFeedbackIngress::default());
        let (mut writer, reader) = tokio::io::duplex(512);
        let reader_task = tokio::spawn(forward_media_feedback_reports(reader, sender.clone()));
        let mut cursor = FeedbackIngressCursor::default();

        sigil_protocol::write_media_feedback_report_v1(&mut writer, &clean_feedback(1))
            .await
            .unwrap();
        receiver.changed().await.unwrap();
        let (first, next_cursor) = receiver
            .borrow_and_update()
            .report_since(cursor)
            .unwrap()
            .unwrap();
        assert_eq!(first.report_id, 1);
        cursor = next_cursor;

        // The consumer is now notionally stalled on an encoder commit. Two
        // reports overwrite one watch notification, but their interval,
        // deltas, and resync pressure must remain cumulative.
        let mut pressured = clean_feedback(2);
        pressured.flags = MediaFeedbackFlags::RESYNC_ACTIVE;
        pressured.transport_dropped_delta = 2;
        pressured.decoder_dropped_delta = 1;
        pressured.decode_queue_depth = 3;
        pressured.transport_delivery_p95_ms = Some(180);
        sigil_protocol::write_media_feedback_report_v1(&mut writer, &pressured)
            .await
            .unwrap();

        let mut latest = clean_feedback(3);
        latest.frontend_dropped_delta = 4;
        latest.presenter_dropped_delta = 3;
        latest.frontend_queue_depth = 3;
        latest.decode_queue_depth = 1;
        latest.transport_delivery_p95_ms = Some(40);
        latest.decode_p95_ms = Some(9);
        sigil_protocol::write_media_feedback_report_v1(&mut writer, &latest)
            .await
            .unwrap();
        writer.shutdown().await.unwrap();
        reader_task.await.unwrap().unwrap();

        receiver.changed().await.unwrap();
        let (aggregate, next_cursor) = receiver
            .borrow_and_update()
            .report_since(cursor)
            .unwrap()
            .unwrap();
        assert_eq!(aggregate.report_id, 3);
        assert_eq!(aggregate.interval_ms, 2_000);
        assert_eq!(aggregate.transport_dropped_delta, 2);
        assert_eq!(aggregate.frontend_dropped_delta, 4);
        assert_eq!(aggregate.decoder_dropped_delta, 1);
        assert_eq!(aggregate.presenter_dropped_delta, 3);
        assert!(aggregate.flags.contains(MediaFeedbackFlags::RESYNC_ACTIVE));
        assert_eq!(aggregate.last_sequence, latest.last_sequence);
        assert_eq!(aggregate.frontend_queue_depth, latest.frontend_queue_depth);
        assert_eq!(aggregate.decode_queue_depth, latest.decode_queue_depth);
        assert_eq!(
            aggregate.transport_delivery_p95_ms,
            latest.transport_delivery_p95_ms
        );
        assert_eq!(aggregate.decode_p95_ms, latest.decode_p95_ms);
        cursor = next_cursor;
        assert!(
            receiver.borrow().report_since(cursor).unwrap().is_none(),
            "one cumulative ingress generation must be processed only once"
        );

        for report_id in 4..=9 {
            sender.send_modify(|ingress| ingress.observe(clean_feedback(report_id)));
        }
        receiver.changed().await.unwrap();
        let (clamped, _) = receiver
            .borrow_and_update()
            .report_since(cursor)
            .unwrap()
            .unwrap();
        assert_eq!(clamped.interval_ms, 5_000);
        let mut controller =
            ShadowBitrateController::new(12_000, MediaV3TelemetrySnapshot::default());
        let decision = controller
            .decide(
                &clamped,
                MediaV3TelemetrySnapshot::default(),
                Instant::now(),
            )
            .unwrap();
        assert!(
            decision
                .reasons
                .contains(AdaptiveBitrateReasonFlagsV1::FEEDBACK_STALE),
            "a clamped aggregate must be stale rather than disguised as one fresh report"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn slow_adaptive_commit_does_not_pause_feedback_evaluation() {
        let sessions = Arc::new(SessionRegistry::default());
        let remote = endpoint(1);
        let media = sessions
            .claim(remote, [4; 16], InvitationGrants::VIEW)
            .unwrap();
        let harness = crate::source::EncoderControlTestHarness::new();
        sessions
            .install_encoder_control(remote, media.session_id, Some(harness.control.clone()))
            .unwrap();
        let mut coordinator = AdaptiveCommitCoordinator::new(
            Arc::clone(&sessions),
            remote,
            media.session_id,
            true,
            12_000,
            Some(native_dimensions()),
        );
        let mut controller =
            ShadowBitrateController::new(12_000, MediaV3TelemetrySnapshot::default());
        let mut resolution_controller = None;
        let observed_at = Instant::now();

        let seed = evaluate_commit_plan(
            &mut controller,
            &mut resolution_controller,
            &clean_feedback(1),
            observed_at,
        );
        assert_eq!(
            coordinator.submit(seed).unwrap().plan.decision.decision_id,
            1
        );

        let mut severe = clean_feedback(2);
        severe.decoder_dropped_delta = 1;
        let first = evaluate_commit_plan(
            &mut controller,
            &mut resolution_controller,
            &severe,
            observed_at + Duration::from_secs(1),
        );
        assert_eq!(first.decision.state, AdaptiveBitrateStateV1::Decrease);
        assert!(coordinator.submit(first).is_none());
        assert!(coordinator.has_pending());

        tokio::time::advance(Duration::from_secs(1)).await;
        let evaluation_tick = tokio::time::Instant::now();
        let mut newer_severe = clean_feedback(3);
        newer_severe.decoder_dropped_delta = 1;
        let queued = evaluate_commit_plan(
            &mut controller,
            &mut resolution_controller,
            &newer_severe,
            observed_at + Duration::from_secs(2),
        );
        assert_eq!(queued.decision.decision_id, 3);
        assert_eq!(queued.decision.state, AdaptiveBitrateStateV1::Decrease);
        assert!(coordinator.submit(queued).is_none());
        assert_eq!(tokio::time::Instant::now(), evaluation_tick);
        assert_eq!(coordinator.acknowledgements.len(), 1);
        assert_eq!(
            coordinator.queued.unwrap().decision.decision_id,
            queued.decision.decision_id
        );

        // Revision one is acknowledged only after another feedback window was
        // evaluated. The decision retains that exact readback result.
        harness.status.send_modify(|status| {
            status.applied_bitrate_revision = Some(1);
            status.applied_bitrate_kbps = Some(first.decision.target_kbps);
        });
        let commit_result = coordinator.acknowledgements.join_next().await;
        let completed = coordinator.complete(commit_result).unwrap();
        assert_eq!(completed.plan.decision.decision_id, 2);
        assert!(completed.plan.decision.applied);
        assert_eq!(
            completed.applied_bitrate_kbps,
            Some(first.decision.target_kbps)
        );

        assert!(coordinator.start_queued().is_none());
        assert!(coordinator.has_pending());
        assert_eq!(coordinator.acknowledgements.len(), 1);

        let session_id = media.session_id;
        drop(media);
        assert!(!sessions.is_active(remote, session_id));
        let session_end_result = coordinator.acknowledgements.join_next().await;
        let session_end = coordinator.complete(session_end_result).unwrap();
        assert!(session_end.generation_ended);
        coordinator.abort_and_drain().await;
        assert!(!coordinator.has_pending());
        assert!(coordinator.queued.is_none());
        assert!(coordinator.acknowledgements.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn timing_out_commits_keep_uniform_hysteresis_and_bounded_work() {
        let sessions = Arc::new(SessionRegistry::default());
        let remote = endpoint(1);
        let media = sessions
            .claim(remote, [5; 16], InvitationGrants::VIEW)
            .unwrap();
        let harness = crate::source::EncoderControlTestHarness::new();
        sessions
            .install_encoder_control(remote, media.session_id, Some(harness.control.clone()))
            .unwrap();
        let mut coordinator = AdaptiveCommitCoordinator::new(
            Arc::clone(&sessions),
            remote,
            media.session_id,
            true,
            12_000,
            Some(native_dimensions()),
        );
        let mut controller =
            ShadowBitrateController::new(12_000, MediaV3TelemetrySnapshot::default());
        let mut resolution_controller =
            Some(MotionResolutionPolicy::new(native_dimensions()).unwrap());
        let observed_at = Instant::now();
        let virtual_started_at = tokio::time::Instant::now();
        let mut evaluation_ticks = Vec::new();
        let mut decision_ids = Vec::new();
        let mut states = Vec::new();
        let mut completed = Vec::new();

        for report_id in 1..=9 {
            evaluation_ticks.push(tokio::time::Instant::now());
            let mut report = clean_feedback(report_id);
            if report_id > 1 {
                report.frontend_queue_depth = 2;
            }
            let plan = evaluate_commit_plan(
                &mut controller,
                &mut resolution_controller,
                &report,
                observed_at + Duration::from_secs(report_id - 1),
            );
            decision_ids.push(plan.decision.decision_id);
            states.push(plan.decision.state);
            if let Some(immediate) = coordinator.submit(plan) {
                completed.push(immediate);
            }
            assert!(
                coordinator.acknowledgements.len() <= 1,
                "the coordinator must own at most one commit task"
            );
            assert!(
                coordinator.queued.iter().count() <= 1,
                "the coordinator must retain only the latest plan"
            );

            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            if let Some(result) = coordinator.acknowledgements.try_join_next() {
                completed.push(coordinator.complete(Some(result)).unwrap());
                if let Some(immediate) = coordinator.start_queued() {
                    completed.push(immediate);
                }
            }
        }

        assert_eq!(
            decision_ids,
            (1..=9).collect::<Vec<_>>(),
            "coalescing must not pause or reuse decision IDs"
        );
        assert_eq!(
            states,
            [
                AdaptiveBitrateStateV1::Hold,
                AdaptiveBitrateStateV1::Hold,
                AdaptiveBitrateStateV1::Decrease,
                AdaptiveBitrateStateV1::Hold,
                AdaptiveBitrateStateV1::Decrease,
                AdaptiveBitrateStateV1::Hold,
                AdaptiveBitrateStateV1::Decrease,
                AdaptiveBitrateStateV1::Hold,
                AdaptiveBitrateStateV1::Decrease,
            ],
            "moderate-pressure hysteresis must advance once per feedback window"
        );
        for (index, tick) in evaluation_ticks.into_iter().enumerate() {
            assert_eq!(
                tick.duration_since(virtual_started_at),
                Duration::from_secs(index as u64),
                "feedback evaluation cadence must remain uniform during commit timeouts"
            );
        }
        assert!(
            completed.iter().any(|acknowledgement| {
                acknowledgement.plan.decision.decision_id == 3
                    && !acknowledgement.plan.decision.applied
                    && acknowledgement.resolution_applied == Some(false)
            }),
            "the first bitrate and resolution timeout must retain exact negative acknowledgement"
        );
        assert!(coordinator.has_pending());
        assert_eq!(coordinator.acknowledgements.len(), 1);
        assert!(coordinator.queued.is_some());

        coordinator.abort_and_drain().await;
        assert!(!coordinator.has_pending());
        assert!(coordinator.queued.is_none());
        assert!(coordinator.acknowledgements.is_empty());

        let session_id = media.session_id;
        drop(media);
        assert!(!sessions.is_active(remote, session_id));
    }
}
