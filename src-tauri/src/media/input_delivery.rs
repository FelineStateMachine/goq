use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use iroh::Endpoint;
use serde::Serialize;
use sigil_protocol::{
    Capability, INPUT_ALPN_V1, InputEvent, InvitationGrants, PointerPosition,
    RELATIVE_POINTER_DELTA_MAX, RELATIVE_POINTER_DELTA_MIN, read_input_ack, write_input_event,
};
use tauri::ipc::Channel;

use crate::commands::state::AppState;
use crate::media::network_diagnostics::{NetworkSessionDiagnostics, lock_network_diagnostics};
use crate::media::transport::{NegotiatedV1Stream, negotiate_v1};
use crate::platform_capabilities::relative_pointer_capture_enabled;

pub(crate) const CLIENT_INPUT_QUEUE_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InputAvailability {
    pub(crate) relative_pointer: bool,
    pub(crate) pointer_position_feedback: bool,
    pub(crate) absolute_pointer: bool,
    pub(crate) keyboard: bool,
    pub(crate) text: bool,
    pub(crate) gamepad: bool,
    pub(crate) input_ack: bool,
    pub(crate) control: bool,
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

fn input_capability_offers_for(
    grants: InvitationGrants,
    relative_pointer_capture: bool,
) -> Vec<Vec<Capability>> {
    // Keep the relative compatibility ladder ordered from newest to oldest.
    // When local relative capture is unavailable, deliberately collapse it to
    // only the inherited absolute-pointer offer.
    let mut base = Vec::with_capacity(if relative_pointer_capture { 4 } else { 1 });
    if relative_pointer_capture {
        base.extend([
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
        ]);
    }
    base.push(vec![
        Capability::AbsolutePointer,
        Capability::Keyboard,
        Capability::Text,
        Capability::Gamepad,
    ]);
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

fn input_capability_offers(grants: InvitationGrants) -> Vec<Vec<Capability>> {
    input_capability_offers_for(grants, relative_pointer_capture_enabled())
}

fn mask_unavailable_relative_pointer_capabilities(
    mut capabilities: Vec<Capability>,
    relative_pointer_capture: bool,
) -> Vec<Capability> {
    if !relative_pointer_capture {
        capabilities.retain(|capability| {
            !matches!(
                capability,
                Capability::RelativePointer
                    | Capability::PointerPositionFeedback
                    | Capability::PointerVisibilityFeedback
            )
        });
    }
    capabilities
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
    diagnostics: Option<&Arc<StdMutex<NetworkSessionDiagnostics>>>,
) -> Result<(), String> {
    if let Some(diagnostics) = diagnostics {
        lock_network_diagnostics(diagnostics).begin_input_send(Instant::now());
    }
    write_input_event(stream, event)
        .await
        .map_err(|error| error.to_string())
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

pub(crate) struct InputSession {
    pub(crate) connection: iroh::endpoint::Connection,
    pub(crate) send: iroh::endpoint::SendStream,
    pub(crate) recv: iroh::endpoint::RecvStream,
    pub(crate) capabilities: Vec<Capability>,
    pub(crate) availability: InputAvailability,
}

pub(crate) async fn open_input_session(
    endpoint: &Endpoint,
    address: &iroh::EndpointAddr,
    nonce: [u8; 16],
    media_session_id: u64,
    grants: InvitationGrants,
) -> Result<InputSession, String> {
    let mut errors = Vec::new();
    let mut accepted = None;
    // Older hosts reject unknown capability enum values. Try all four
    // pointer feature levels with ACK first, then repeat the exact legacy
    // offers without ACK so lack of diagnostics never forces absolute
    // pointer input prematurely.
    for capabilities in input_capability_offers(grants) {
        match open_negotiated_input_stream(endpoint, address, nonce, capabilities).await {
            Ok(result) => {
                accepted = Some(result);
                break;
            }
            Err(error) => errors.push(error),
        }
    }
    let (connection, send, recv, input_negotiation) = accepted
        .ok_or_else(|| format!("All input capability offers failed: {}", errors.join("; ")))?;
    if input_negotiation.session_id != media_session_id {
        return Err("Host returned mismatched media and input sessions".to_string());
    }
    let capabilities = input_negotiation.capabilities;
    // A malformed or future host must not be able to re-enable a local input
    // path that this Portal build deliberately withheld from its offers.
    let capabilities = mask_unavailable_relative_pointer_capabilities(
        capabilities,
        relative_pointer_capture_enabled(),
    );
    let availability = InputAvailability::from_capabilities(&capabilities);
    Ok(InputSession {
        connection,
        send,
        recv,
        capabilities,
        availability,
    })
}

pub(crate) async fn run_input_feedback(
    mut input_feedback: iroh::endpoint::RecvStream,
    feedback_diagnostics: Arc<StdMutex<NetworkSessionDiagnostics>>,
    mut pointer_feedback_enabled: bool,
    input_ack_enabled: bool,
    pointer_channel: Channel<PointerFeedbackPayload>,
) {
    let terminal_reason = loop {
        let response = match read_input_ack(&mut input_feedback).await {
            Ok(Some(response)) => response,
            Ok(None) => {
                lock_network_diagnostics(&feedback_diagnostics).mark_input_feedback_closed();
                break PointerFeedbackTerminalReason::Eof;
            }
            Err(error) => {
                lock_network_diagnostics(&feedback_diagnostics).mark_input_feedback_malformed();
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
}

pub(crate) async fn run_input_forwarder(
    mut input_stream: iroh::endpoint::SendStream,
    rx: tokio::sync::mpsc::Receiver<InputEvent>,
    input_capabilities: Vec<Capability>,
    input_send_diagnostics: Arc<StdMutex<NetworkSessionDiagnostics>>,
) {
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
        if let Err(error) =
            write_client_input_event(&mut input_stream, &event, Some(&input_send_diagnostics)).await
        {
            eprintln!("[client] input stream write failed: {error}; disconnecting");
            break;
        }
    }
    while let Some(event) = pending_relative.take() {
        if let Err(error) =
            write_client_input_event(&mut input_stream, &event, Some(&input_send_diagnostics)).await
        {
            eprintln!("[client] final relative input write failed: {error}");
            break;
        }
    }
    let _ = input_stream.finish();
}

pub(crate) async fn send_input(state: &AppState, event: InputEvent) -> Result<bool, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let offers = input_capability_offers_for(InvitationGrants::ALL, true);
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
    fn disabled_relative_capture_preserves_only_non_relative_input_offers() {
        let offers = input_capability_offers_for(InvitationGrants::ALL, false);
        assert_eq!(
            offers,
            [
                vec![
                    Capability::AbsolutePointer,
                    Capability::Keyboard,
                    Capability::Text,
                    Capability::Gamepad,
                    Capability::InputAck,
                ],
                vec![
                    Capability::AbsolutePointer,
                    Capability::Keyboard,
                    Capability::Text,
                    Capability::Gamepad,
                ],
            ]
        );
        assert!(offers.iter().flatten().all(|capability| !matches!(
            capability,
            Capability::RelativePointer
                | Capability::PointerPositionFeedback
                | Capability::PointerVisibilityFeedback
        )));
    }

    #[test]
    fn compiled_pointer_policy_controls_the_actual_negotiation_offers() {
        let offers = input_capability_offers(InvitationGrants::ALL);
        let offers_relative_pointer = offers
            .iter()
            .flatten()
            .any(|capability| *capability == Capability::RelativePointer);
        assert_eq!(offers_relative_pointer, relative_pointer_capture_enabled());
        assert!(
            offers
                .iter()
                .all(|offer| offer.contains(&Capability::AbsolutePointer))
        );
    }

    #[test]
    fn negotiated_capability_mask_fails_closed_without_losing_other_input() {
        let accepted = vec![
            Capability::RelativePointer,
            Capability::PointerPositionFeedback,
            Capability::PointerVisibilityFeedback,
            Capability::AbsolutePointer,
            Capability::Keyboard,
            Capability::Text,
            Capability::Gamepad,
            Capability::InputAck,
        ];
        let masked = mask_unavailable_relative_pointer_capabilities(accepted.clone(), false);
        assert_eq!(
            masked,
            [
                Capability::AbsolutePointer,
                Capability::Keyboard,
                Capability::Text,
                Capability::Gamepad,
                Capability::InputAck,
            ]
        );
        assert_eq!(
            mask_unavailable_relative_pointer_capabilities(accepted.clone(), true),
            accepted
        );
        assert_eq!(
            InputAvailability::from_capabilities(&masked),
            InputAvailability {
                relative_pointer: false,
                pointer_position_feedback: false,
                absolute_pointer: true,
                keyboard: true,
                text: true,
                gamepad: true,
                input_ack: true,
                control: true,
            }
        );
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
