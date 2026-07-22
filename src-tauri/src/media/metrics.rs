use std::collections::VecDeque;
use std::sync::{Mutex as StdMutex, MutexGuard as StdMutexGuard};
use std::time::Duration;

use serde::Serialize;

use super::frame_channel::CLIENT_FRAME_CHANNEL_CAPACITY;
use super::network_diagnostics::NetworkDiagnosticsSnapshot;
#[cfg(test)]
use super::network_diagnostics::PathMode;

const CLIENT_FRAME_RATE_WINDOW: Duration = Duration::from_secs(1);
const CLIENT_FRAME_TIMING_WINDOW: Duration = Duration::from_secs(5);
// Host configuration permits at most 240 fps. Leave a little headroom for
// timer-boundary samples while keeping every rate window strictly bounded.
const CLIENT_FRAME_RATE_SAMPLE_CAPACITY: usize = 256;
const CLIENT_FRAME_TIMING_SAMPLE_CAPACITY: usize = 512;

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
pub(crate) struct ClientMediaMetrics {
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
    pub(crate) fn observe_transport_receive(
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

    pub(crate) fn observe_frontend_send(&mut self, elapsed: Duration) {
        self.frontend_sent_total = self.frontend_sent_total.saturating_add(1);
        self.frontend_send_rate.record(elapsed);
    }

    pub(crate) fn observe_sequence_drop(&mut self, count: u64) {
        self.sequence_dropped_total = self.sequence_dropped_total.saturating_add(count);
    }

    pub(crate) fn observe_transport_object_drop(&mut self, late: bool) {
        self.transport_object_dropped_total = self.transport_object_dropped_total.saturating_add(1);
        if late {
            self.transport_late_object_dropped_total =
                self.transport_late_object_dropped_total.saturating_add(1);
        }
    }

    pub(crate) fn observe_frontend_queue_drop(&mut self) {
        self.frontend_queue_dropped_total = self.frontend_queue_dropped_total.saturating_add(1);
    }

    pub(crate) fn observe_frontend_resync_drop(&mut self) {
        self.frontend_resync_dropped_total = self.frontend_resync_dropped_total.saturating_add(1);
    }

    pub(crate) fn begin_frontend_resync(&mut self, elapsed: Duration) {
        if self.frontend_resync_started_at.is_none() {
            self.frontend_resync_episode_total =
                self.frontend_resync_episode_total.saturating_add(1);
            self.frontend_resync_started_at = Some(elapsed);
        }
    }

    pub(crate) fn finish_frontend_resync(&mut self, elapsed: Duration) {
        let Some(started_at) = self.frontend_resync_started_at.take() else {
            return;
        };
        let duration = elapsed.saturating_sub(started_at);
        self.frontend_resync_completed_duration = self
            .frontend_resync_completed_duration
            .saturating_add(duration);
        self.frontend_resync_max_duration = self.frontend_resync_max_duration.max(duration);
    }

    pub(crate) fn observe_frontend_ipc_send_duration(
        &mut self,
        elapsed: Duration,
        duration: Duration,
    ) {
        self.frontend_ipc_send_durations.record(elapsed, duration);
    }

    pub(crate) fn observe_frontend_queue_depth(&mut self, depth: usize) {
        self.frontend_queue_peak = self.frontend_queue_peak.max(depth);
    }

    pub(crate) fn snapshot(
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

pub(crate) fn lock_client_media_metrics(
    metrics: &StdMutex<ClientMediaMetrics>,
) -> StdMutexGuard<'_, ClientMediaMetrics> {
    metrics
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct FrameStatsPayload {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
