use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use iroh::Endpoint;
use serde::{Deserialize, Serialize};
use sigil_protocol::{
    AdaptiveBitrateDecisionV1, AdaptiveBitrateReasonFlagsV1, AdaptiveBitrateStateV1, Capability,
    MEDIA_FEEDBACK_ALPN_V1, MediaFeedbackFlags, MediaFeedbackReportV1,
    read_adaptive_bitrate_decision_v1, write_media_feedback_report_v1,
};
use tauri::{AppHandle, Emitter, Manager};

use crate::commands::state::{AccumulatedMediaFeedback, AppState};
use crate::media::frame_channel::take_generation_owned_triple;
use crate::media::transport::{connect_error_is_unsupported_alpn, negotiate_v1};

pub(crate) const CLIENT_MEDIA_FEEDBACK_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const CLIENT_MEDIA_FEEDBACK_IO_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_ADAPTIVE_DECISION_DELIVERY_INTERVAL: Duration = Duration::from_secs(1);

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

pub(crate) async fn open_negotiated_feedback_stream(
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

pub(crate) async fn run_media_feedback_session(
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

pub(crate) async fn send_media_feedback(
    state: &AppState,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
