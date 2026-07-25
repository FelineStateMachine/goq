use std::sync::Arc;

use serde::Serialize;
use sigil_protocol::{
    ClientControlEnvelopeV2, FocusCommandActionV2, FocusCommandV2, ServerControlEnvelopeV2,
    SessionSnapshotV2, read_server_control_v2, write_client_control_v2,
};
use tauri::{AppHandle, Emitter};

use crate::commands::state::{SessionControlSender, SessionSnapshotState};
use crate::media::network_diagnostics::{NetworkSessionDiagnostics, lock_network_diagnostics};

pub(crate) const SESSION_CONTROL_COMMAND_CAPACITY: usize = 4;

#[derive(Clone, Serialize)]
pub(crate) struct SessionStatePayload {
    pub(crate) native_generation: u64,
    pub(crate) snapshot: SessionSnapshotV2,
}

#[derive(Clone, Serialize)]
pub(crate) struct FocusCommandResultPayload {
    pub(crate) native_generation: u64,
    pub(crate) request_id: u64,
    pub(crate) accepted: bool,
    pub(crate) revision: u64,
    pub(crate) message: Option<String>,
}

pub(crate) async fn install_initial_snapshot(
    app: &AppHandle,
    state: &Arc<tokio::sync::Mutex<Option<SessionSnapshotState>>>,
    native_generation: u64,
    snapshot: SessionSnapshotV2,
) -> Result<(), String> {
    snapshot
        .validate()
        .map_err(|error| format!("Invalid initial session snapshot: {error}"))?;
    let payload = SessionStatePayload {
        native_generation,
        snapshot: snapshot.clone(),
    };
    *state.lock().await = Some((native_generation, snapshot));
    app.emit("session-state", payload)
        .map_err(|error| format!("Failed to publish initial session snapshot: {error}"))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_session_control(
    app: AppHandle,
    native_generation: u64,
    media_generation_id: u64,
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    mut commands: tokio::sync::mpsc::Receiver<ClientControlEnvelopeV2>,
    state: Arc<tokio::sync::Mutex<Option<SessionSnapshotState>>>,
    diagnostics: Arc<std::sync::Mutex<NetworkSessionDiagnostics>>,
) {
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break; };
                if let Err(error) = write_client_control_v2(&mut send, &command).await {
                    eprintln!("[client] control v2 command write failed: {error}");
                    break;
                }
            }
            envelope = read_server_control_v2(&mut recv) => {
                match envelope {
                    Ok(Some(ServerControlEnvelopeV2::Snapshot { snapshot })) => {
                        if snapshot.media.generation_id != media_generation_id {
                            eprintln!("[client] control v2 snapshot changed media generation");
                            break;
                        }
                        let mut guard = state.lock().await;
                        let is_current_generation = guard.as_ref().is_some_and(|(generation, _)| {
                            *generation == native_generation
                        });
                        if !is_current_generation {
                            break;
                        }
                        let current_revision = guard
                            .as_ref()
                            .map(|(_, snapshot)| snapshot.revision)
                            .unwrap_or(0);
                        if snapshot.revision <= current_revision {
                            let _ = lock_network_diagnostics(&diagnostics)
                                .observe_session_snapshot(&snapshot, std::time::Instant::now());
                            continue;
                        }
                        if let Err(error) = lock_network_diagnostics(&diagnostics)
                            .observe_session_snapshot(&snapshot, std::time::Instant::now())
                        {
                            eprintln!("[client] invalid session diagnostics snapshot: {error}");
                            break;
                        }
                        *guard = Some((native_generation, snapshot.clone()));
                        drop(guard);
                        let _ = app.emit(
                            "session-state",
                            SessionStatePayload { native_generation, snapshot },
                        );
                    }
                    Ok(Some(ServerControlEnvelopeV2::FocusResult { result })) => {
                        let _ = app.emit(
                            "focus-command-result",
                            FocusCommandResultPayload {
                                native_generation,
                                request_id: result.request_id,
                                accepted: result.accepted,
                                revision: result.revision,
                                message: result.message,
                            },
                        );
                    }
                    Ok(None) => break,
                    Err(error) => {
                        lock_network_diagnostics(&diagnostics).mark_session_snapshot_invalid();
                        eprintln!("[client] invalid control v2 message: {error}");
                        break;
                    }
                }
            }
        }
    }
}

pub(crate) async fn send_focus_command(
    sender: &tokio::sync::Mutex<Option<(u64, SessionControlSender)>>,
    native_generation: u64,
    request_id: u64,
    action: FocusCommandActionV2,
    expected_revision: u64,
    expected_focus_generation: Option<u64>,
    expected_proposal_id: Option<u64>,
) -> Result<bool, String> {
    let sender = sender
        .lock()
        .await
        .as_ref()
        .filter(|(generation, _)| *generation == native_generation)
        .map(|(_, sender)| sender.clone())
        .ok_or_else(|| "No matching control v2 session is active".to_string())?;
    let command = ClientControlEnvelopeV2::Focus {
        command: FocusCommandV2 {
            request_id,
            action,
            slot: sigil_protocol::ControllerSlot::ZERO,
            expected_revision,
            expected_focus_generation,
            expected_proposal_id,
        },
    };
    command
        .validate()
        .map_err(|error| format!("Invalid focus command: {error}"))?;
    match sender.try_send(command) {
        Ok(()) => Ok(true),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(false),
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            Err("Control v2 command channel closed".to_string())
        }
    }
}

#[cfg(test)]
pub(crate) async fn latest_snapshot(
    state: &tokio::sync::Mutex<Option<SessionSnapshotState>>,
    native_generation: u64,
) -> Option<SessionSnapshotV2> {
    state
        .lock()
        .await
        .as_ref()
        .filter(|(generation, _)| *generation == native_generation)
        .map(|(_, snapshot)| snapshot.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_protocol::{
        ControllerSlot, FocusStateV2, FocusTransitionReasonV2, MediaGenerationDescriptorV2,
        ViewerPresenceId, ViewerPresenceV2,
    };

    fn snapshot(revision: u64) -> SessionSnapshotV2 {
        let presence = ViewerPresenceId::new("viewer-1").unwrap();
        SessionSnapshotV2 {
            revision,
            self_presence_id: presence.clone(),
            viewers: vec![ViewerPresenceV2 {
                presence_id: presence,
                session_id: 7,
                input_capable: true,
                you: true,
            }],
            focus: FocusStateV2::Vacant {
                slot: ControllerSlot::ZERO,
            },
            focus_proposal: None,
            self_is_focus_owner: false,
            transition_reason: FocusTransitionReasonV2::Initial,
            media: MediaGenerationDescriptorV2 {
                generation_id: 7,
                broadcast_name: sigil_protocol::media_moq_broadcast_name(7).unwrap(),
            },
        }
    }

    #[tokio::test]
    async fn latest_snapshot_is_owned_by_native_generation() {
        let state = tokio::sync::Mutex::new(Some((3, snapshot(2))));
        assert_eq!(latest_snapshot(&state, 3).await.unwrap().revision, 2);
        assert!(latest_snapshot(&state, 4).await.is_none());
    }

    #[tokio::test]
    async fn focus_commands_are_bounded_and_revision_bound() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let state = tokio::sync::Mutex::new(Some((5, sender)));
        assert!(
            send_focus_command(&state, 5, 1, FocusCommandActionV2::Request, 2, None, None)
                .await
                .unwrap()
        );
        assert!(
            !send_focus_command(&state, 5, 2, FocusCommandActionV2::Request, 2, None, None)
                .await
                .unwrap()
        );
        assert!(matches!(
            receiver.recv().await,
            Some(ClientControlEnvelopeV2::Focus { .. })
        ));
        assert!(
            send_focus_command(&state, 4, 3, FocusCommandActionV2::Request, 2, None, None)
                .await
                .is_err()
        );
    }
}
