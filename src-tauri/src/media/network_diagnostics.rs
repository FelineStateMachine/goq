use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use iroh::endpoint::{Connection, PathId};
use serde::Serialize;

const LATENCY_BUCKETS_MS: usize = 5_001;
pub(crate) const INPUT_ACK_PENDING_CAPACITY: usize = 1_024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PathMode {
    #[default]
    Unknown,
    Direct,
    Relay,
    Custom,
}

impl PathMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Direct => "direct",
            Self::Relay => "relay",
            Self::Custom => "custom",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum NetworkLeg {
    Media,
    Input,
    Audio,
}

#[derive(Clone, Debug, Default)]
struct BoundedLatencyHistogram {
    buckets: Box<[u64]>,
    sample_count: u64,
    overflow_total: u64,
    max_ms: Option<u64>,
}

impl BoundedLatencyHistogram {
    fn new() -> Self {
        Self {
            buckets: vec![0; LATENCY_BUCKETS_MS].into_boxed_slice(),
            ..Self::default()
        }
    }

    fn record(&mut self, duration: Duration) {
        let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        self.sample_count = self.sample_count.saturating_add(1);
        self.max_ms = Some(self.max_ms.unwrap_or_default().max(millis));
        if let Ok(index) = usize::try_from(millis)
            && let Some(bucket) = self.buckets.get_mut(index)
        {
            *bucket = bucket.saturating_add(1);
        } else {
            self.overflow_total = self.overflow_total.saturating_add(1);
        }
    }

    fn percentile(&self, percentile: u64) -> Option<u64> {
        if self.sample_count == 0 {
            return None;
        }
        let rank = self.sample_count.saturating_mul(percentile).div_ceil(100);
        let mut seen = 0_u64;
        for (millis, count) in self.buckets.iter().copied().enumerate() {
            seen = seen.saturating_add(count);
            if seen >= rank {
                return u64::try_from(millis).ok();
            }
        }
        None
    }

    fn summary(&self) -> LatencySummary {
        LatencySummary {
            sample_count: self.sample_count,
            p50_ms: self.percentile(50),
            p95_ms: self.percentile(95),
            max_ms: self.max_ms,
            overflow_total: self.overflow_total,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct LatencySummary {
    sample_count: u64,
    p50_ms: Option<u64>,
    p95_ms: Option<u64>,
    max_ms: Option<u64>,
    overflow_total: u64,
}

#[derive(Clone, Debug)]
struct LegObservation {
    selected_path_id: Option<PathId>,
    mode: PathMode,
    rtt: Option<Duration>,
    cwnd_bytes: Option<u64>,
    mtu: Option<u16>,
    congestion_events: Option<u64>,
    black_holes_detected: Option<u64>,
    tx_datagrams: u64,
    tx_bytes: u64,
    rx_datagrams: u64,
    rx_bytes: u64,
    lost_packets: u64,
    lost_bytes: u64,
}

impl LegObservation {
    fn from_connection(connection: &Connection) -> Self {
        let paths = connection.paths();
        let selected = paths.iter().find(|path| path.is_selected());
        let (selected_path_id, mode, rtt, cwnd_bytes, mtu, congestion_events, black_holes_detected) =
            if let Some(path) = selected {
                let stats = path.stats();
                let mode = classify_path_mode(path.is_ip(), path.is_relay());
                (
                    Some(path.id()),
                    mode,
                    Some(stats.rtt),
                    Some(stats.cwnd),
                    Some(stats.current_mtu),
                    Some(stats.congestion_events),
                    Some(stats.black_holes_detected),
                )
            } else {
                (None, PathMode::Unknown, None, None, None, None, None)
            };
        let stats = connection.stats();
        Self {
            selected_path_id,
            mode,
            rtt,
            cwnd_bytes,
            mtu,
            congestion_events,
            black_holes_detected,
            tx_datagrams: stats.udp_tx.datagrams,
            tx_bytes: stats.udp_tx.bytes,
            rx_datagrams: stats.udp_rx.datagrams,
            rx_bytes: stats.udp_rx.bytes,
            lost_packets: stats.lost_packets,
            lost_bytes: stats.lost_bytes,
        }
    }
}

const fn classify_path_mode(is_ip: bool, is_relay: bool) -> PathMode {
    match (is_ip, is_relay) {
        (true, false) => PathMode::Direct,
        (false, true) => PathMode::Relay,
        _ => PathMode::Custom,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ConnectionCounters {
    tx_datagrams: u64,
    tx_bytes: u64,
    rx_datagrams: u64,
    rx_bytes: u64,
    lost_packets: u64,
    lost_bytes: u64,
}

impl ConnectionCounters {
    fn from_observation(observation: &LegObservation) -> Self {
        Self {
            tx_datagrams: observation.tx_datagrams,
            tx_bytes: observation.tx_bytes,
            rx_datagrams: observation.rx_datagrams,
            rx_bytes: observation.rx_bytes,
            lost_packets: observation.lost_packets,
            lost_bytes: observation.lost_bytes,
        }
    }

    fn regressed_from(self, previous: Self) -> bool {
        self.tx_datagrams < previous.tx_datagrams
            || self.tx_bytes < previous.tx_bytes
            || self.rx_datagrams < previous.rx_datagrams
            || self.rx_bytes < previous.rx_bytes
            || self.lost_packets < previous.lost_packets
            || self.lost_bytes < previous.lost_bytes
    }
}

#[derive(Clone, Debug)]
struct LegDiagnostics {
    mode: PathMode,
    selected_path_id: Option<PathId>,
    sample_count: u64,
    direct_samples: u64,
    relay_samples: u64,
    custom_samples: u64,
    unknown_samples: u64,
    path_epoch: u64,
    path_switch_total: u64,
    mode_transition_total: u64,
    rtt: BoundedLatencyHistogram,
    rtt_current_ms: Option<f64>,
    counters: ConnectionCounters,
    has_counters: bool,
    cwnd_bytes: Option<u64>,
    mtu: Option<u16>,
    congestion_events: Option<u64>,
    black_holes_detected: Option<u64>,
    counter_regression_total: u64,
}

impl Default for LegDiagnostics {
    fn default() -> Self {
        Self {
            mode: PathMode::Unknown,
            selected_path_id: None,
            sample_count: 0,
            direct_samples: 0,
            relay_samples: 0,
            custom_samples: 0,
            unknown_samples: 0,
            path_epoch: 0,
            path_switch_total: 0,
            mode_transition_total: 0,
            rtt: BoundedLatencyHistogram::new(),
            rtt_current_ms: None,
            counters: ConnectionCounters::default(),
            has_counters: false,
            cwnd_bytes: None,
            mtu: None,
            congestion_events: None,
            black_holes_detected: None,
            counter_regression_total: 0,
        }
    }
}

impl LegDiagnostics {
    fn observe(&mut self, observation: LegObservation) {
        let previous_mode = self.mode;
        let previous_path_id = self.selected_path_id;
        self.sample_count = self.sample_count.saturating_add(1);
        match observation.mode {
            PathMode::Direct => self.direct_samples = self.direct_samples.saturating_add(1),
            PathMode::Relay => self.relay_samples = self.relay_samples.saturating_add(1),
            PathMode::Custom => self.custom_samples = self.custom_samples.saturating_add(1),
            PathMode::Unknown => self.unknown_samples = self.unknown_samples.saturating_add(1),
        }
        if observation.selected_path_id != previous_path_id {
            if previous_path_id.is_some() {
                self.path_switch_total = self.path_switch_total.saturating_add(1);
            }
            if observation.selected_path_id.is_some() {
                self.path_epoch = self.path_epoch.saturating_add(1);
            }
        }
        if self.sample_count > 1 && previous_mode != observation.mode {
            self.mode_transition_total = self.mode_transition_total.saturating_add(1);
        }
        if let Some(rtt) = observation.rtt {
            self.rtt.record(rtt);
        }
        self.rtt_current_ms = observation.rtt.map(|rtt| rtt.as_secs_f64() * 1_000.0);
        let counters = ConnectionCounters::from_observation(&observation);
        if self.has_counters && counters.regressed_from(self.counters) {
            self.counter_regression_total = self.counter_regression_total.saturating_add(1);
        }
        self.mode = observation.mode;
        self.selected_path_id = observation.selected_path_id;
        self.counters = counters;
        self.has_counters = true;
        self.cwnd_bytes = observation.cwnd_bytes;
        self.mtu = observation.mtu;
        self.congestion_events = observation.congestion_events;
        self.black_holes_detected = observation.black_holes_detected;
    }

    fn snapshot(&self) -> NetworkLegSnapshot {
        let rtt = self.rtt.summary();
        NetworkLegSnapshot {
            mode: self.mode,
            sample_count: self.sample_count,
            direct_samples: self.direct_samples,
            relay_samples: self.relay_samples,
            custom_samples: self.custom_samples,
            unknown_samples: self.unknown_samples,
            path_epoch: self.path_epoch,
            path_switch_total: self.path_switch_total,
            mode_transition_total: self.mode_transition_total,
            rtt_sample_count: rtt.sample_count,
            rtt_current_ms: self.rtt_current_ms,
            rtt_p50_ms: rtt.p50_ms,
            rtt_p95_ms: rtt.p95_ms,
            rtt_max_ms: rtt.max_ms,
            rtt_overflow_total: rtt.overflow_total,
            tx_datagrams: self.counters.tx_datagrams,
            tx_bytes: self.counters.tx_bytes,
            rx_datagrams: self.counters.rx_datagrams,
            rx_bytes: self.counters.rx_bytes,
            lost_packets: self.counters.lost_packets,
            lost_bytes: self.counters.lost_bytes,
            cwnd_bytes: self.cwnd_bytes,
            mtu: self.mtu,
            congestion_events: self.congestion_events,
            black_holes_detected: self.black_holes_detected,
            counter_regression_total: self.counter_regression_total,
            complete: self.counter_regression_total == 0 && rtt.overflow_total == 0,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct NetworkLegSnapshot {
    pub(crate) mode: PathMode,
    sample_count: u64,
    direct_samples: u64,
    relay_samples: u64,
    custom_samples: u64,
    unknown_samples: u64,
    path_epoch: u64,
    path_switch_total: u64,
    mode_transition_total: u64,
    rtt_sample_count: u64,
    pub(crate) rtt_current_ms: Option<f64>,
    pub(crate) rtt_p50_ms: Option<u64>,
    rtt_p95_ms: Option<u64>,
    rtt_max_ms: Option<u64>,
    rtt_overflow_total: u64,
    tx_datagrams: u64,
    tx_bytes: u64,
    rx_datagrams: u64,
    rx_bytes: u64,
    lost_packets: u64,
    lost_bytes: u64,
    cwnd_bytes: Option<u64>,
    mtu: Option<u16>,
    congestion_events: Option<u64>,
    black_holes_detected: Option<u64>,
    counter_regression_total: u64,
    complete: bool,
}

#[derive(Clone, Debug)]
struct InputAckDiagnostics {
    negotiated: bool,
    sent_total: u64,
    acknowledged_total: u64,
    last_ack: Option<u64>,
    pending: VecDeque<(u64, Instant)>,
    duplicate_total: u64,
    untracked_total: u64,
    malformed: bool,
    closed: bool,
    latency: BoundedLatencyHistogram,
}

impl InputAckDiagnostics {
    fn new(negotiated: bool) -> Self {
        Self {
            negotiated,
            sent_total: 0,
            acknowledged_total: 0,
            last_ack: None,
            pending: VecDeque::with_capacity(INPUT_ACK_PENDING_CAPACITY),
            duplicate_total: 0,
            untracked_total: 0,
            malformed: false,
            closed: false,
            latency: BoundedLatencyHistogram::new(),
        }
    }

    fn begin_send(&mut self, now: Instant) -> Option<u64> {
        if !self.negotiated {
            return None;
        }
        self.sent_total = self.sent_total.saturating_add(1);
        if self.pending.len() == INPUT_ACK_PENDING_CAPACITY {
            self.pending.pop_front();
            self.untracked_total = self.untracked_total.saturating_add(1);
        }
        self.pending.push_back((self.sent_total, now));
        Some(self.sent_total)
    }

    fn observe_ack(&mut self, sequence: u64, now: Instant) -> Result<(), String> {
        if !self.negotiated {
            self.malformed = true;
            return Err("Host sent an input acknowledgement that was not negotiated".to_string());
        }
        if self.last_ack == Some(sequence) {
            self.duplicate_total = self.duplicate_total.saturating_add(1);
            return Ok(());
        }
        if self.last_ack.is_some_and(|last| sequence < last) || sequence > self.sent_total {
            self.malformed = true;
            return Err(format!(
                "Host sent invalid input acknowledgement {sequence} after {} local events",
                self.sent_total
            ));
        }
        if sequence == 0 && self.last_ack.is_none() {
            self.last_ack = Some(0);
            return Ok(());
        }
        while self
            .pending
            .front()
            .is_some_and(|(pending, _)| *pending <= sequence)
        {
            let (_, sent_at) = self.pending.pop_front().expect("front checked");
            self.latency.record(now.saturating_duration_since(sent_at));
        }
        self.acknowledged_total = sequence;
        self.last_ack = Some(sequence);
        Ok(())
    }

    fn snapshot(&self) -> InputAckSnapshot {
        let latency = self.latency.summary();
        InputAckSnapshot {
            negotiated: self.negotiated,
            sent_total: self.sent_total,
            acknowledged_total: self.acknowledged_total,
            pending_count: self.pending.len(),
            pending_capacity: INPUT_ACK_PENDING_CAPACITY,
            duplicate_total: self.duplicate_total,
            untracked_total: self.untracked_total,
            malformed: self.malformed,
            closed: self.closed,
            latency_sample_count: latency.sample_count,
            latency_p50_ms: latency.p50_ms,
            latency_p95_ms: latency.p95_ms,
            latency_max_ms: latency.max_ms,
            latency_overflow_total: latency.overflow_total,
            complete: !self.malformed
                && !self.closed
                && self.untracked_total == 0
                && latency.overflow_total == 0,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct InputAckSnapshot {
    negotiated: bool,
    sent_total: u64,
    acknowledged_total: u64,
    pending_count: usize,
    pending_capacity: usize,
    duplicate_total: u64,
    untracked_total: u64,
    malformed: bool,
    closed: bool,
    latency_sample_count: u64,
    latency_p50_ms: Option<u64>,
    latency_p95_ms: Option<u64>,
    latency_max_ms: Option<u64>,
    latency_overflow_total: u64,
    complete: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct NetworkSessionDiagnostics {
    started_at: Instant,
    media: LegDiagnostics,
    input: LegDiagnostics,
    audio: Option<LegDiagnostics>,
    input_ack: InputAckDiagnostics,
}

impl NetworkSessionDiagnostics {
    pub(crate) fn new(started_at: Instant, input_ack_negotiated: bool) -> Self {
        Self {
            started_at,
            media: LegDiagnostics::default(),
            input: LegDiagnostics::default(),
            audio: None,
            input_ack: InputAckDiagnostics::new(input_ack_negotiated),
        }
    }

    pub(crate) fn observe_connection(&mut self, leg: NetworkLeg, connection: &Connection) {
        let diagnostics = match leg {
            NetworkLeg::Media => &mut self.media,
            NetworkLeg::Input => &mut self.input,
            NetworkLeg::Audio => self.audio.get_or_insert_with(LegDiagnostics::default),
        };
        diagnostics.observe(LegObservation::from_connection(connection));
    }

    pub(crate) fn begin_input_send(&mut self, now: Instant) {
        let _ = self.input_ack.begin_send(now);
    }

    pub(crate) fn observe_input_ack(&mut self, sequence: u64, now: Instant) -> Result<(), String> {
        self.input_ack.observe_ack(sequence, now)
    }

    pub(crate) fn mark_input_feedback_closed(&mut self) {
        if self.input_ack.negotiated {
            self.input_ack.closed = true;
        }
    }

    pub(crate) fn mark_input_feedback_malformed(&mut self) {
        if self.input_ack.negotiated {
            self.input_ack.malformed = true;
        }
    }

    pub(crate) fn snapshot(&self, now: Instant) -> NetworkDiagnosticsSnapshot {
        NetworkDiagnosticsSnapshot {
            version: 1,
            session_elapsed_ms: u64::try_from(
                now.saturating_duration_since(self.started_at).as_millis(),
            )
            .unwrap_or(u64::MAX),
            media: self.media.snapshot(),
            input: self.input.snapshot(),
            audio: self.audio.as_ref().map(LegDiagnostics::snapshot),
            input_ack: self.input_ack.snapshot(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct NetworkDiagnosticsSnapshot {
    version: u8,
    session_elapsed_ms: u64,
    pub(crate) media: NetworkLegSnapshot,
    input: NetworkLegSnapshot,
    audio: Option<NetworkLegSnapshot>,
    input_ack: InputAckSnapshot,
}

#[cfg(test)]
impl NetworkDiagnosticsSnapshot {
    pub(crate) fn test_fixture(mode: PathMode, rtt: Option<Duration>) -> Self {
        let mut diagnostics = NetworkSessionDiagnostics::new(Instant::now(), false);
        diagnostics.media.mode = mode;
        diagnostics.media.sample_count = 1;
        match mode {
            PathMode::Direct => diagnostics.media.direct_samples = 1,
            PathMode::Relay => diagnostics.media.relay_samples = 1,
            PathMode::Custom => diagnostics.media.custom_samples = 1,
            PathMode::Unknown => diagnostics.media.unknown_samples = 1,
        }
        if let Some(rtt) = rtt {
            diagnostics.media.rtt.record(rtt);
            diagnostics.media.rtt_current_ms = Some(rtt.as_secs_f64() * 1_000.0);
        }
        diagnostics.snapshot(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(mode: PathMode, counters: u64) -> LegObservation {
        LegObservation {
            selected_path_id: Some(PathId::ZERO),
            mode,
            rtt: Some(Duration::from_millis(10 + counters)),
            cwnd_bytes: Some(64_000),
            mtu: Some(1_200),
            congestion_events: Some(0),
            black_holes_detected: Some(0),
            tx_datagrams: counters,
            tx_bytes: counters * 100,
            rx_datagrams: counters,
            rx_bytes: counters * 100,
            lost_packets: 0,
            lost_bytes: 0,
        }
    }

    #[test]
    fn path_mode_adapter_is_fail_closed_for_ambiguous_paths() {
        assert_eq!(classify_path_mode(true, false), PathMode::Direct);
        assert_eq!(classify_path_mode(false, true), PathMode::Relay);
        assert_eq!(classify_path_mode(false, false), PathMode::Custom);
        assert_eq!(classify_path_mode(true, true), PathMode::Custom);
    }

    #[test]
    fn path_residency_and_transitions_remain_bounded() {
        let mut diagnostics = LegDiagnostics::default();
        diagnostics.observe(observation(PathMode::Direct, 1));
        diagnostics.observe(observation(PathMode::Direct, 2));
        diagnostics.observe(observation(PathMode::Relay, 3));
        let snapshot = diagnostics.snapshot();
        assert_eq!(snapshot.sample_count, 3);
        assert_eq!(snapshot.direct_samples, 2);
        assert_eq!(snapshot.relay_samples, 1);
        assert_eq!(snapshot.mode_transition_total, 1);
        assert!(snapshot.complete);
    }

    #[test]
    fn same_mode_path_switch_advances_epoch_without_mode_transition() {
        let mut diagnostics = LegDiagnostics::default();
        diagnostics.observe(observation(PathMode::Direct, 1));
        let mut replacement = observation(PathMode::Direct, 2);
        replacement.selected_path_id = Some(PathId::MAX);
        diagnostics.observe(replacement);
        let snapshot = diagnostics.snapshot();
        assert_eq!(snapshot.path_epoch, 2);
        assert_eq!(snapshot.path_switch_total, 1);
        assert_eq!(snapshot.mode_transition_total, 0);
    }

    #[test]
    fn current_rtt_preserves_sub_millisecond_precision() {
        let mut diagnostics = LegDiagnostics::default();
        let mut sample = observation(PathMode::Direct, 1);
        sample.rtt = Some(Duration::from_micros(800));
        diagnostics.observe(sample);
        let current = diagnostics.snapshot().rtt_current_ms.unwrap();
        assert!((current - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn counter_regression_marks_the_leg_incomplete() {
        let mut diagnostics = LegDiagnostics::default();
        diagnostics.observe(observation(PathMode::Direct, 5));
        diagnostics.observe(observation(PathMode::Direct, 4));
        let snapshot = diagnostics.snapshot();
        assert_eq!(snapshot.counter_regression_total, 1);
        assert!(!snapshot.complete);
    }

    #[test]
    fn latency_overflow_is_visible_instead_of_clamped() {
        let mut histogram = BoundedLatencyHistogram::new();
        histogram.record(Duration::from_millis(5_001));
        let summary = histogram.summary();
        assert_eq!(summary.sample_count, 1);
        assert_eq!(summary.p95_ms, None);
        assert_eq!(summary.max_ms, Some(5_001));
        assert_eq!(summary.overflow_total, 1);
    }

    #[test]
    fn cumulative_ack_accounts_for_each_pending_event() {
        let started = Instant::now();
        let mut diagnostics = InputAckDiagnostics::new(true);
        diagnostics.begin_send(started);
        diagnostics.begin_send(started + Duration::from_millis(1));
        diagnostics.begin_send(started + Duration::from_millis(2));
        diagnostics
            .observe_ack(3, started + Duration::from_millis(12))
            .unwrap();
        let snapshot = diagnostics.snapshot();
        assert_eq!(snapshot.acknowledged_total, 3);
        assert_eq!(snapshot.pending_count, 0);
        assert_eq!(snapshot.latency_sample_count, 3);
        assert_eq!(snapshot.latency_p50_ms, Some(11));
    }

    #[test]
    fn initial_and_duplicate_ack_do_not_invent_latency() {
        let now = Instant::now();
        let mut diagnostics = InputAckDiagnostics::new(true);
        diagnostics.observe_ack(0, now).unwrap();
        diagnostics.observe_ack(0, now).unwrap();
        let snapshot = diagnostics.snapshot();
        assert_eq!(snapshot.duplicate_total, 1);
        assert_eq!(snapshot.latency_sample_count, 0);
    }

    #[test]
    fn lower_and_future_ack_fail_closed() {
        let now = Instant::now();
        let mut diagnostics = InputAckDiagnostics::new(true);
        diagnostics.begin_send(now);
        diagnostics.observe_ack(1, now).unwrap();
        assert!(diagnostics.observe_ack(0, now).is_err());
        assert!(diagnostics.observe_ack(2, now).is_err());
        assert!(diagnostics.snapshot().malformed);
    }

    #[test]
    fn pending_overflow_drops_telemetry_not_input() {
        let now = Instant::now();
        let mut diagnostics = InputAckDiagnostics::new(true);
        for _ in 0..=INPUT_ACK_PENDING_CAPACITY {
            diagnostics.begin_send(now);
        }
        let snapshot = diagnostics.snapshot();
        assert_eq!(snapshot.sent_total, (INPUT_ACK_PENDING_CAPACITY + 1) as u64);
        assert_eq!(snapshot.pending_count, INPUT_ACK_PENDING_CAPACITY);
        assert_eq!(snapshot.untracked_total, 1);
        assert!(!snapshot.complete);
    }

    #[test]
    fn serialized_snapshot_omits_path_and_peer_identity_material() {
        let now = Instant::now();
        let snapshot = NetworkSessionDiagnostics::new(now, true).snapshot(now);
        let json = serde_json::to_string(&snapshot).unwrap();
        for forbidden in [
            "path_id",
            "remote_addr",
            "local_addr",
            "node_id",
            "relay_url",
        ] {
            assert!(
                !json.contains(forbidden),
                "serialized forbidden field {forbidden}"
            );
        }
    }
}
