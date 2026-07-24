use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use sigil_protocol::{InvitationGrants, MAX_INVITATION_TTL_SECS, SignedInvitation};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::authorization::{
    AuthorizationMutation, AuthorizationStore, EnrolledViewer, grant_names, unix_timestamp_now,
};
use crate::secure_state;
use crate::server::{InputOperations, SessionRegistry};

pub const ADMIN_SOCKET_FILE: &str = "authorization-admin-v1.sock";
const MAX_ADMIN_REQUEST_BYTES: u64 = 16 * 1024;
const MAX_ADMIN_RESPONSE_BYTES: usize = 64 * 1024;
const ADMIN_IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthorizationAdminRequest {
    List,
    InspectRevision,
    CreateInvitation {
        peer_node_id: String,
        grants: u8,
        expires_in_seconds: u64,
    },
    RevokeViewer {
        handle: String,
    },
    ReplaceGrants {
        handle: String,
        grants: Option<u8>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthorizationAdminResult {
    Viewers {
        committed_revision: u64,
        enrollment_epoch: u64,
        viewers: Vec<AdminViewer>,
    },
    Revision {
        committed_revision: u64,
    },
    Invitation {
        token: String,
        expires_at_unix: u64,
        committed_revision: u64,
    },
    Mutation {
        handle: String,
        committed_revision: u64,
        disconnected: bool,
        input_neutralized: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdminViewer {
    pub handle: String,
    pub grants: Vec<String>,
    pub enrolled_at_unix: u64,
    pub authorization_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuthorizationAdminResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<AuthorizationAdminResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub struct AuthorizationAdminServer {
    socket_path: PathBuf,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl AuthorizationAdminServer {
    pub fn start(
        state_directory: &Path,
        store: AuthorizationStore,
        sessions: Arc<SessionRegistry>,
        input_operations: Arc<InputOperations>,
        host_secret: [u8; 32],
    ) -> Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = (
                state_directory,
                store,
                sessions,
                input_operations,
                host_secret,
            );
            anyhow::bail!("authorization administration requires a Unix host");
        }
        #[cfg(unix)]
        {
            let socket_path =
                secure_state::prepare_private_unix_socket(state_directory, ADMIN_SOCKET_FILE)?;
            let listener = tokio::net::UnixListener::bind(&socket_path).with_context(|| {
                format!(
                    "binding authorization administration socket {}",
                    socket_path.display()
                )
            })?;
            secure_state::secure_private_unix_socket(&socket_path)?;
            let (shutdown, mut shutdown_rx) = tokio::sync::oneshot::channel();
            let task_path = socket_path.clone();
            let task = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => break,
                        accepted = listener.accept() => {
                            match accepted {
                                Ok((stream, _address)) => {
                                    if let Err(error) = serve_request(
                                        stream,
                                        &store,
                                        &sessions,
                                        &input_operations,
                                        host_secret,
                                    ).await {
                                        tracing::warn!(%error, "authorization administration request failed");
                                    }
                                }
                                Err(error) => {
                                    tracing::warn!(%error, "authorization administration listener failed");
                                    break;
                                }
                            }
                        }
                    }
                }
                let _ = std::fs::remove_file(&task_path);
            });
            Ok(Self {
                socket_path,
                shutdown: Some(shutdown),
                task: Some(task),
            })
        }
    }

    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for AuthorizationAdminServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(unix)]
async fn serve_request(
    mut stream: tokio::net::UnixStream,
    store: &AuthorizationStore,
    sessions: &SessionRegistry,
    input_operations: &InputOperations,
    host_secret: [u8; 32],
) -> Result<()> {
    let response = match tokio::time::timeout(ADMIN_IO_TIMEOUT, async {
        let mut bytes = Vec::new();
        (&mut stream)
            .take(MAX_ADMIN_REQUEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            !bytes.is_empty() && bytes.len() as u64 <= MAX_ADMIN_REQUEST_BYTES,
            "authorization administration request is empty or oversized"
        );
        let request: AuthorizationAdminRequest = serde_json::from_slice(&bytes)
            .context("parsing authorization administration request")?;
        handle_request(request, store, sessions, input_operations, host_secret).await
    })
    .await
    {
        Ok(Ok(result)) => AuthorizationAdminResponse {
            ok: true,
            result: Some(result),
            error: None,
        },
        Ok(Err(error)) => AuthorizationAdminResponse {
            ok: false,
            result: None,
            error: Some(format!("{error:#}")),
        },
        Err(_) => AuthorizationAdminResponse {
            ok: false,
            result: None,
            error: Some("authorization administration request timed out".to_owned()),
        },
    };
    let encoded = serde_json::to_vec(&response)?;
    ensure!(
        encoded.len() <= MAX_ADMIN_RESPONSE_BYTES,
        "authorization administration response exceeds its fixed bound"
    );
    tokio::time::timeout(ADMIN_IO_TIMEOUT, async {
        stream.write_all(&encoded).await?;
        stream.shutdown().await
    })
    .await
    .context("timed out writing authorization administration response")??;
    Ok(())
}

async fn handle_request(
    request: AuthorizationAdminRequest,
    store: &AuthorizationStore,
    sessions: &SessionRegistry,
    input_operations: &InputOperations,
    host_secret: [u8; 32],
) -> Result<AuthorizationAdminResult> {
    match request {
        AuthorizationAdminRequest::List => {
            let snapshot = store.list_viewers()?;
            Ok(AuthorizationAdminResult::Viewers {
                committed_revision: snapshot.committed_revision,
                enrollment_epoch: snapshot.epoch,
                viewers: snapshot.viewers.iter().map(AdminViewer::from).collect(),
            })
        }
        AuthorizationAdminRequest::InspectRevision => {
            let snapshot = store.list_viewers()?;
            Ok(AuthorizationAdminResult::Revision {
                committed_revision: snapshot.committed_revision,
            })
        }
        AuthorizationAdminRequest::CreateInvitation {
            peer_node_id,
            grants,
            expires_in_seconds,
        } => {
            let peer = peer_node_id
                .parse::<EndpointId>()
                .context("invitation peer node ID is invalid")?;
            let grants = InvitationGrants::new(grants)?;
            ensure!(
                grants.contains(InvitationGrants::VIEW),
                "invitations must grant view permission"
            );
            ensure!(
                (1..=MAX_INVITATION_TTL_SECS).contains(&expires_in_seconds),
                "invitation TTL is outside the fixed bound"
            );
            let snapshot = store.list_viewers()?;
            let mut nonce = [0_u8; 32];
            getrandom::fill(&mut nonce).context("generating invitation nonce")?;
            let now = unix_timestamp_now()?;
            let host = iroh::SecretKey::from_bytes(&host_secret).public();
            let claims = AuthorizationStore::issue_claims_from_snapshot(
                host,
                snapshot.clone(),
                peer,
                grants,
                expires_in_seconds,
                now,
                nonce,
            )?;
            let expires_at_unix = claims.expires_at_unix;
            let token = SignedInvitation::issue(claims, &host_secret)?.encode();
            Ok(AuthorizationAdminResult::Invitation {
                token,
                expires_at_unix,
                committed_revision: snapshot.committed_revision,
            })
        }
        AuthorizationAdminRequest::RevokeViewer { handle } => {
            let mutation = store.revoke_viewer(&handle, unix_timestamp_now()?)?;
            apply_mutation(mutation, sessions, input_operations)
        }
        AuthorizationAdminRequest::ReplaceGrants { handle, grants } => {
            let grants = grants.map(InvitationGrants::new).transpose()?;
            let mutation = store.replace_viewer_grants(&handle, grants, unix_timestamp_now()?)?;
            apply_mutation(mutation, sessions, input_operations)
        }
    }
}

fn apply_mutation(
    mutation: AuthorizationMutation,
    sessions: &SessionRegistry,
    input_operations: &InputOperations,
) -> Result<AuthorizationAdminResult> {
    let effect = sessions.apply_authorization_mutation(&mutation)?;
    if effect.neutralize_input {
        input_operations.reset()?;
        if !effect.disconnected {
            sessions.complete_authorization_neutralization(
                mutation.peer,
                mutation.authorization_revision,
            )?;
        }
    }
    Ok(AuthorizationAdminResult::Mutation {
        handle: mutation.handle,
        committed_revision: mutation.committed_revision,
        disconnected: effect.disconnected,
        input_neutralized: effect.neutralize_input,
    })
}

impl From<&EnrolledViewer> for AdminViewer {
    fn from(viewer: &EnrolledViewer) -> Self {
        Self {
            handle: viewer.handle.clone(),
            grants: grant_names(viewer.grants)
                .split(',')
                .map(str::to_owned)
                .collect(),
            enrolled_at_unix: viewer.enrolled_at_unix,
            authorization_revision: viewer.authorization_revision,
        }
    }
}

#[cfg(unix)]
pub async fn request(
    state_directory: &Path,
    request: &AuthorizationAdminRequest,
) -> Result<AuthorizationAdminResult> {
    secure_state::validate_private_directory(state_directory)?;
    let socket_path = state_directory.join(ADMIN_SOCKET_FILE);
    let metadata = std::fs::symlink_metadata(&socket_path)
        .with_context(|| format!("inspecting administration socket {}", socket_path.display()))?;
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    ensure!(
        metadata.file_type().is_socket()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "authorization administration socket is unsafe"
    );
    let encoded = serde_json::to_vec(request)?;
    ensure!(
        encoded.len() as u64 <= MAX_ADMIN_REQUEST_BYTES,
        "authorization administration request exceeds its fixed bound"
    );
    let mut stream = tokio::time::timeout(
        ADMIN_IO_TIMEOUT,
        tokio::net::UnixStream::connect(&socket_path),
    )
    .await
    .context("timed out connecting to authorization administration socket")??;
    tokio::time::timeout(ADMIN_IO_TIMEOUT, async {
        stream.write_all(&encoded).await?;
        stream.shutdown().await
    })
    .await
    .context("timed out writing authorization administration request")??;
    let mut response = Vec::new();
    tokio::time::timeout(
        ADMIN_IO_TIMEOUT,
        (&mut stream)
            .take(MAX_ADMIN_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut response),
    )
    .await
    .context("timed out reading authorization administration response")??;
    ensure!(
        !response.is_empty() && response.len() <= MAX_ADMIN_RESPONSE_BYTES,
        "authorization administration response is empty or oversized"
    );
    let response: AuthorizationAdminResponse = serde_json::from_slice(&response)
        .context("parsing authorization administration response")?;
    if response.ok {
        response
            .result
            .context("successful administration response has no result")
    } else {
        anyhow::bail!(
            "{}",
            response
                .error
                .unwrap_or_else(|| "authorization administration request failed".to_owned())
        )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::config::{HostConfig, InputMode, VideoSource};
    use crate::input::InputBackend;
    use sigil_protocol::{ControllerSlot, FocusCommandActionV2, FocusCommandV2};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn disabled_input_backend(state_path: &Path) -> InputBackend {
        InputBackend::initialize(&HostConfig {
            identity_path: state_path.join("unused-identity"),
            state_path: state_path.to_path_buf(),
            source: VideoSource::TestPattern,
            width: Some(1280),
            height: Some(800),
            framerate: 60,
            codec: "h264".to_owned(),
            input_mode: InputMode::Disabled,
            uinput: None,
            ffmpeg_path: PathBuf::from("ffmpeg"),
            gamescope_pipewire: None,
            audio: None,
        })
        .unwrap()
    }

    async fn issue_and_redeem(
        directory: &Path,
        store: &AuthorizationStore,
        peer: EndpointId,
        grants: InvitationGrants,
    ) -> crate::authorization::AuthorizedViewer {
        let result = request(
            directory,
            &AuthorizationAdminRequest::CreateInvitation {
                peer_node_id: peer.to_string(),
                grants: grants.bits(),
                expires_in_seconds: 600,
            },
        )
        .await
        .unwrap();
        let AuthorizationAdminResult::Invitation { token, .. } = result else {
            panic!("unexpected invitation response");
        };
        store
            .authorize_or_redeem_viewer(peer, Some(&token), unix_timestamp_now().unwrap())
            .unwrap()
    }

    #[tokio::test]
    async fn socket_administers_multiple_viewers_and_persists_mutations() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let host = iroh::SecretKey::from_bytes(&[7; 32]);
        let peer_one = iroh::SecretKey::from_bytes(&[8; 32]).public();
        let peer_two = iroh::SecretKey::from_bytes(&[9; 32]).public();
        let store = AuthorizationStore::open(directory.path(), host.public()).unwrap();
        let sessions = Arc::new(SessionRegistry::default());
        let operations = Arc::new(InputOperations::new(disabled_input_backend(
            directory.path(),
        )));
        let server = AuthorizationAdminServer::start(
            directory.path(),
            store.clone(),
            Arc::clone(&sessions),
            operations,
            host.to_bytes(),
        )
        .unwrap();

        let first =
            issue_and_redeem(directory.path(), &store, peer_one, InvitationGrants::ALL).await;
        let second =
            issue_and_redeem(directory.path(), &store, peer_two, InvitationGrants::VIEW).await;
        assert_ne!(first.handle, second.handle);

        let AuthorizationAdminResult::Viewers {
            viewers,
            committed_revision,
            ..
        } = request(directory.path(), &AuthorizationAdminRequest::List)
            .await
            .unwrap()
        else {
            panic!("unexpected list response");
        };
        assert_eq!(viewers.len(), 2);
        assert!(committed_revision >= 3);
        assert!(
            viewers
                .iter()
                .all(|viewer| !viewer.handle.contains(&peer_one.to_string()))
        );

        request(
            directory.path(),
            &AuthorizationAdminRequest::ReplaceGrants {
                handle: first.handle.clone(),
                grants: Some(InvitationGrants::VIEW.bits()),
            },
        )
        .await
        .unwrap();
        assert!(
            sessions
                .claim_v2_authorized(peer_one, [9; 16], first.clone())
                .is_err(),
            "an admission decision from before the committed mutation must be stale"
        );
        request(
            directory.path(),
            &AuthorizationAdminRequest::RevokeViewer {
                handle: second.handle,
            },
        )
        .await
        .unwrap();

        server.shutdown().await;
        let snapshot = store.list_viewers().unwrap();
        assert_eq!(snapshot.viewers.len(), 1);
        assert_eq!(snapshot.viewers[0].handle, first.handle);
        assert_eq!(snapshot.viewers[0].grants, InvitationGrants::VIEW);
        assert!(directory.path().join("authorization-v2.json").is_file());
        assert!(!directory.path().join(ADMIN_SOCKET_FILE).exists());
    }

    #[tokio::test]
    async fn committed_reduction_defocuses_then_view_revocation_disconnects() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let host = iroh::SecretKey::from_bytes(&[17; 32]);
        let peer = iroh::SecretKey::from_bytes(&[18; 32]).public();
        let store = AuthorizationStore::open(directory.path(), host.public()).unwrap();
        let sessions = Arc::new(SessionRegistry::default());
        let operations = Arc::new(InputOperations::new(disabled_input_backend(
            directory.path(),
        )));
        let server = AuthorizationAdminServer::start(
            directory.path(),
            store.clone(),
            Arc::clone(&sessions),
            Arc::clone(&operations),
            host.to_bytes(),
        )
        .unwrap();
        let authorized =
            issue_and_redeem(directory.path(), &store, peer, InvitationGrants::ALL).await;
        let handle = authorized.handle.clone();
        let media = sessions
            .claim_v2_authorized(peer, [3; 16], authorized)
            .unwrap();
        let granted = sessions
            .apply_focus_command(
                peer,
                media.session_id,
                &FocusCommandV2 {
                    request_id: 1,
                    action: FocusCommandActionV2::Request,
                    slot: ControllerSlot::ZERO,
                    expected_revision: 1,
                    expected_focus_generation: None,
                },
            )
            .unwrap();
        let focus_generation = granted.snapshot.self_focus_generation().unwrap();

        let AuthorizationAdminResult::Mutation {
            input_neutralized,
            disconnected,
            ..
        } = request(
            directory.path(),
            &AuthorizationAdminRequest::ReplaceGrants {
                handle: handle.clone(),
                grants: Some(InvitationGrants::VIEW.bits()),
            },
        )
        .await
        .unwrap()
        else {
            panic!("unexpected grant mutation response");
        };
        assert!(input_neutralized);
        assert!(!disconnected);
        assert!(!sessions.is_v2_focus_owner(
            peer,
            media.session_id,
            ControllerSlot::ZERO,
            focus_generation
        ));
        assert!(sessions.is_active(peer, media.session_id));

        let AuthorizationAdminResult::Mutation { disconnected, .. } = request(
            directory.path(),
            &AuthorizationAdminRequest::RevokeViewer { handle },
        )
        .await
        .unwrap() else {
            panic!("unexpected revoke response");
        };
        assert!(disconnected);
        assert!(!sessions.is_active(peer, media.session_id));
        server.shutdown().await;
    }
}
