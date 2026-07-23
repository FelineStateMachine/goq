use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use iroh::{Endpoint, SecretKey, endpoint::presets};
use serde::Serialize;
use sigil_protocol::{
    InputEvent, InvitationGrants, KeyframeRequestReasonV3, PointerSurfaceDimensions,
};
use tauri::{
    AppHandle, Emitter,
    ipc::{Channel, Response},
};

use crate::commands::auth::derive_iroh_secret_from_key;
use crate::commands::enrollment::{connection_enrollment, mark_invitation_redeemed};
use crate::commands::state::{AppState, MediaFeedbackSender, development_direct_node_available};
use crate::media::adaptive_feedback::{
    CLIENT_MEDIA_FEEDBACK_CONNECT_TIMEOUT, open_negotiated_feedback_stream,
    run_media_feedback_session,
};
use crate::media::audio_delivery::{
    AudioStartRequest, lock_audio_deliveries, next_audio_generation, try_start_audio,
};
use crate::media::frame_channel::{close_generation_connection, next_media_generation};
use crate::media::input_delivery::{
    CLIENT_INPUT_QUEUE_CAPACITY, InputSession, PointerFeedbackPayload, open_input_session,
    run_input_feedback, run_input_forwarder,
};
use crate::media::media_control::run_media_control_writer_v3;
use crate::media::moq_receiver::open_upstream_moq_media;
use crate::media::network_diagnostics::NetworkSessionDiagnostics;
use crate::media::transport::{
    CLIENT_ENDPOINT_CLOSE_TIMEOUT, MediaTransport, open_negotiated_media_stream,
};
use crate::media::video_delivery::{VideoDeliveryRequest, run_video_delivery};

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

pub(crate) async fn connect_client(
    app: AppHandle,
    state: &AppState,
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

    // Binary WebCodecs delivery is the only video path; fail before any
    // network work so a webview without H.264 WebCodecs support surfaces a
    // clear error instead of a black stream.
    if !state.webcodecs.load(Ordering::SeqCst) {
        return Err(
            "WebCodecs H.264 decoding is unavailable in this webview; Portal cannot stream video"
                .to_string(),
        );
    }

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

    let first_attempt =
        open_negotiated_media_stream(&endpoint, &addr, handshake_nonce, invitation.as_deref())
            .await;
    let (frame_conn, frame_recv, media_control_stream, media_negotiation, media_transport) =
        match first_attempt {
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
    let media_session_id = media_negotiation.session_id;
    let (upstream_moq_media, frame_connection_for_stats) = if media_transport
        == MediaTransport::UpstreamMoq
    {
        let (receiver, diagnostics_connection) =
            match open_upstream_moq_media(&endpoint, &addr, media_session_id).await {
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
    let pointer_surface_dimensions = media_negotiation.pointer_surface_dimensions;
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
    let (adaptive_feedback_stream, adaptive_feedback_error) = match tokio::time::timeout(
        CLIENT_MEDIA_FEEDBACK_CONNECT_TIMEOUT,
        open_negotiated_feedback_stream(&endpoint, &addr, handshake_nonce, media_session_id),
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
    };

    let InputSession {
        connection: input_connection,
        send: input_send,
        recv: input_recv,
        capabilities: input_capabilities,
        availability: input_availability,
    } = open_input_session(&endpoint, &addr, handshake_nonce, media_session_id, grants).await?;

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
    let media_control_requests = {
        let (control_tx, control_rx) = tokio::sync::mpsc::channel(1);
        *state.media_control.lock().await = Some((media_generation, control_tx.clone()));
        tokio::spawn(run_media_control_writer_v3(
            media_control_stream,
            control_rx,
        ));
        if media_transport == MediaTransport::GroupedObjectsV3 {
            let _ = control_tx.try_send((KeyframeRequestReasonV3::Join, None));
        }
        Some(control_tx)
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
        input_capabilities,
        input_send_diagnostics,
    ));

    tokio::spawn(run_video_delivery(VideoDeliveryRequest {
        app,
        endpoint,
        frame_recv,
        frame_connection: frame_connection_for_stats,
        input_connection,
        audio_connection: audio_connection_for_stats,
        network_diagnostics,
        media_control_requests,
        upstream_moq_media,
        frame_events_in_flight,
        frame_channel,
        media_transport,
        media_generation,
        connected_audio_generation,
    }));

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

pub(crate) async fn disconnect_client(state: &AppState) -> Result<bool, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
