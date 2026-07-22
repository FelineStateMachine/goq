use super::auth::derive_iroh_secret_from_key;
use super::enrollment::{connection_enrollment, mark_invitation_redeemed};
use super::state::{AppState, FRAME_ALPN, MediaFeedbackSender, development_direct_node_available};
pub use crate::media::adaptive_feedback::ClientMediaFeedbackReport;
use crate::media::adaptive_feedback::{
    CLIENT_MEDIA_FEEDBACK_CONNECT_TIMEOUT, open_negotiated_feedback_stream,
    run_media_feedback_session, send_media_feedback,
};
use crate::media::audio_delivery::{
    self, AudioStartRequest, lock_audio_deliveries, next_audio_generation, try_start_audio,
};
pub use crate::media::frame_channel::FramePayload;
use crate::media::frame_channel::{
    FrameEnvelopeMetadata, byte_to_codec, close_generation_connection, emit_frame_error,
    encode_frame_envelope, next_media_generation, release_frame_channel_slot,
    release_frame_channel_slot_for_generation, try_reserve_frame_channel_slot,
    validate_legacy_media_header,
};
use crate::media::input_delivery::{
    CLIENT_INPUT_QUEUE_CAPACITY, InputSession, open_input_session, run_input_feedback,
    run_input_forwarder, send_input,
};
#[allow(unused_imports)]
pub use crate::media::input_delivery::{PointerFeedbackPayload, PointerFeedbackTerminalReason};
use crate::media::media_control::{
    request_keyframe, run_media_control_writer_v3, try_queue_media_keyframe_request,
};
use crate::media::metrics::{ClientMediaMetrics, lock_client_media_metrics};
use crate::media::moq_receiver::{
    MoqMediaReadOutcome, open_upstream_moq_media, retire_upstream_moq_generation,
};
use crate::media::network_diagnostics::{
    NetworkLeg, NetworkSessionDiagnostics, lock_network_diagnostics,
};
use crate::media::object_receiver::{
    MediaObjectReadOutcome, MediaObjectReadOutcomeV3, MediaObjectReceiver, MediaObjectReceiverV3,
    MediaObjectSequence, MediaObjectSequenceDecision, MediaObjectSequenceDecisionV3,
    MediaObjectSequenceV3,
};
use crate::media::transport::{
    CLIENT_ENDPOINT_CLOSE_TIMEOUT, MediaTransport, open_negotiated_media_stream,
};
use base64::Engine;
use iroh::{Endpoint, SecretKey, endpoint::presets};
use openh264::{formats::YUVSource, nal_units};
use serde::Serialize;
use sigil_protocol::{
    FrameFlags, InputEvent, InvitationGrants, KeyframeRequestReasonV3, MediaCodec,
    PointerSurfaceDimensions, read_media_frame,
};
use std::io::Cursor;
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tauri::{
    AppHandle, Emitter, State,
    ipc::{Channel, Response},
};

// ─── Client commands ──────────────────────────────────────────────────────────

const LEGACY_MEDIA_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const CLIENT_FRAME_STATS_INTERVAL: Duration = Duration::from_millis(250);

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

    let InputSession {
        connection: input_connection,
        send: input_send,
        recv: input_recv,
        capabilities: input_capabilities,
        availability: input_availability,
    } = open_input_session(
        &endpoint,
        &addr,
        handshake_nonce,
        media_session_id,
        grants,
        use_v1,
    )
    .await?;

    let network_diagnostics = Arc::new(StdMutex::new(NetworkSessionDiagnostics::new(
        Instant::now(),
        input_availability.input_ack,
    )));

    if input_availability.pointer_position_feedback || input_availability.input_ack {
        let feedback_diagnostics = Arc::clone(&network_diagnostics);
        let pointer_feedback_enabled = input_availability.pointer_position_feedback;
        let input_ack_enabled = input_availability.input_ack;
        tokio::spawn(run_input_feedback(
            input_recv,
            feedback_diagnostics,
            pointer_feedback_enabled,
            input_ack_enabled,
            pointer_channel,
        ));
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
    let input_send_diagnostics = Arc::clone(&network_diagnostics);
    tokio::spawn(run_input_forwarder(
        input_send,
        rx,
        use_v1,
        input_capabilities,
        input_send_diagnostics,
    ));

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
                                    receiver.last_frame_sequence(),
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
                                        media_object_sequence_v3.last_sequence(),
                                    );
                                }
                            }
                            MediaObjectReadOutcomeV3::Malformed(error) => {
                                emit_frame_error(&app, media_generation, error);
                                break 'frames;
                            }
                            MediaObjectReadOutcomeV3::Object { object, .. } => {
                                let was_waiting = media_object_sequence_v3.waiting_for_keyframe();
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
                                                media_object_sequence_v3.last_sequence(),
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

    #[test]
    fn media_generations_are_nonzero_monotonic_and_checked_for_overflow() {
        let counter = AtomicU64::new(0);
        assert_eq!(next_media_generation(&counter).unwrap(), 1);
        assert_eq!(next_media_generation(&counter).unwrap(), 2);

        let exhausted = AtomicU64::new(u64::MAX);
        assert!(next_media_generation(&exhausted).is_err());
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
    fn sequence_checks_reject_duplicates_regressions_and_overflow() {
        assert_eq!(sequence_gap(41, 42).unwrap(), 0);
        assert_eq!(sequence_gap(41, 45).unwrap(), 3);
        assert!(sequence_gap(41, 41).is_err());
        assert!(sequence_gap(41, 40).is_err());
        assert!(sequence_gap(u64::MAX, 0).is_err());
    }
}

#[tauri::command]
pub async fn iroh_client_send_input(
    state: State<'_, AppState>,
    event: InputEvent,
) -> Result<bool, String> {
    send_input(&state, event).await
}

#[tauri::command]
pub async fn iroh_client_request_keyframe(
    state: State<'_, AppState>,
    generation: u64,
    reason: String,
) -> Result<bool, String> {
    request_keyframe(&state, generation, reason).await
}

#[tauri::command]
pub async fn iroh_client_send_media_feedback(
    state: State<'_, AppState>,
    generation: u64,
    report: ClientMediaFeedbackReport,
) -> Result<bool, String> {
    send_media_feedback(&state, generation, report).await
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
    audio_delivery::iroh_client_ack_audio(state, generation, delivery_id)
}

#[tauri::command]
pub async fn iroh_client_stop_audio(
    state: State<'_, AppState>,
    expected_generation: u64,
) -> Result<bool, String> {
    audio_delivery::iroh_client_stop_audio(state, expected_generation).await
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
