use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use sigil_protocol::InvitationGrants;

use crate::authorization::{AuthorizationInspection, AuthorizationStore};
use crate::config::HostConfig;
use crate::secure_state;
use crate::server::SessionRegistry;

const LIFECYCLE_LOCK_FILE: &str = "daemon-v1.lock";
const GLOBAL_LIFECYCLE_LOCK_FILE: &str = "daemon-global-v1.lock";
const RUNTIME_STATUS_FILE: &str = "daemon-status-v1.json";
const RUNTIME_STATUS_VERSION: u16 = 1;
const MAX_RUNTIME_STATUS_BYTES: u64 = 16 * 1024;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const STALE_AFTER: Duration = Duration::from_secs(10);
const CLOCK_SKEW_TOLERANCE: Duration = Duration::from_secs(5);

const DAEMON_STARTING: u8 = 0;
const DAEMON_READY: u8 = 1;
const DAEMON_STOPPING: u8 = 2;
const DAEMON_DEGRADED: u8 = 3;
const DAEMON_STOPPED: u8 = 4;

pub struct LifecycleGuard {
    _files: Vec<fs::File>,
}

impl LifecycleGuard {
    pub fn acquire(state_directory: &Path, require_global: bool) -> Result<Self> {
        let runtime_directory = configured_runtime_directory()?;
        ensure!(
            !require_global || runtime_directory.is_some(),
            "configured Sigil service requires a valid XDG_RUNTIME_DIR"
        );
        Self::acquire_at(state_directory, runtime_directory.as_deref())
    }

    fn acquire_at(state_directory: &Path, runtime_directory: Option<&Path>) -> Result<Self> {
        let mut files = vec![secure_state::open_lifetime_lock(
            state_directory,
            LIFECYCLE_LOCK_FILE,
        )?];
        if let Some(runtime_directory) = runtime_directory {
            secure_state::ensure_private_directory(runtime_directory)?;
            files.push(secure_state::open_lifetime_lock(
                runtime_directory,
                GLOBAL_LIFECYCLE_LOCK_FILE,
            )?);
        }
        Ok(Self { _files: files })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DaemonState {
    Starting,
    Ready,
    Stopping,
    Degraded,
    Stopped,
    Offline,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeState {
    Fresh,
    Absent,
    Stale,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionState {
    Active,
    Inactive,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum OverallState {
    Ready,
    Active,
    Degraded,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeErrorCode {
    AuthorizationState,
    Preflight,
    EndpointBind,
    RouterShutdown,
    ShutdownSignal,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeErrorV1 {
    code: RuntimeErrorCode,
    occurred_at_unix_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeStatusV1 {
    version: u16,
    instance_id: String,
    host_node_id: String,
    updated_at_unix_ms: u64,
    uptime_ms: u64,
    daemon: DaemonState,
    session_active: bool,
    last_error: Option<RuntimeErrorV1>,
}

#[derive(Debug, Serialize)]
pub struct ApplianceStatusV1 {
    schema_version: u16,
    sigil_version: &'static str,
    overall: OverallState,
    config: ConfigStatus,
    identity: IdentityStatus,
    enrollment: EnrollmentStatus,
    runtime: RuntimeStatus,
}

#[derive(Debug, Serialize)]
struct ConfigStatus {
    state: &'static str,
}

#[derive(Debug, Serialize)]
struct IdentityStatus {
    state: &'static str,
    host_fingerprint: String,
}

#[derive(Debug, Serialize)]
struct EnrollmentStatus {
    state: &'static str,
    storage: &'static str,
    peer_fingerprint: Option<String>,
    grants: Vec<&'static str>,
    epoch: u64,
    enrolled_at_unix: Option<u64>,
}

#[derive(Debug, Serialize)]
struct RuntimeStatus {
    state: RuntimeState,
    daemon: DaemonState,
    uptime_ms: Option<u64>,
    heartbeat_age_ms: Option<u64>,
    session: SessionState,
    last_error: Option<RuntimeErrorV1>,
}

pub struct RuntimePublisher {
    state_directory: PathBuf,
    instance_id: String,
    host: EndpointId,
    started: Instant,
    daemon_state: Arc<AtomicU8>,
    last_error: Arc<Mutex<Option<RuntimeErrorV1>>>,
    sessions: Arc<SessionRegistry>,
    write_lock: Arc<tokio::sync::Mutex<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl RuntimePublisher {
    pub fn start(
        host: EndpointId,
        sessions: Arc<SessionRegistry>,
        require_runtime: bool,
    ) -> Result<Option<Self>> {
        let Some(state_directory) = configured_runtime_directory()? else {
            ensure!(
                !require_runtime,
                "configured Sigil service requires a valid XDG_RUNTIME_DIR"
            );
            tracing::warn!("XDG_RUNTIME_DIR is unavailable; appliance heartbeat is disabled");
            return Ok(None);
        };
        secure_state::ensure_private_directory(&state_directory)?;
        Self::start_at(&state_directory, host, sessions).map(Some)
    }

    fn start_at(
        state_directory: &Path,
        host: EndpointId,
        sessions: Arc<SessionRegistry>,
    ) -> Result<Self> {
        let instance_id = random_instance_id()?;
        let daemon_state = Arc::new(AtomicU8::new(DAEMON_STARTING));
        let last_error = Arc::new(Mutex::new(None));
        let write_lock = Arc::new(tokio::sync::Mutex::new(()));
        let mut publisher = Self {
            state_directory: state_directory.to_path_buf(),
            instance_id,
            host,
            started: Instant::now(),
            daemon_state,
            last_error,
            sessions,
            write_lock,
            task: None,
        };
        publisher.write_snapshot()?;

        let state_directory = publisher.state_directory.clone();
        let instance_id = publisher.instance_id.clone();
        let host = publisher.host;
        let started = publisher.started;
        let daemon_state = Arc::clone(&publisher.daemon_state);
        let last_error = Arc::clone(&publisher.last_error);
        let sessions = Arc::clone(&publisher.sessions);
        let write_lock = Arc::clone(&publisher.write_lock);
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await;
            loop {
                interval.tick().await;
                let _write_guard = write_lock.lock().await;
                let state_directory = state_directory.clone();
                let instance_id = instance_id.clone();
                let daemon = decode_daemon_state(daemon_state.load(Ordering::Relaxed));
                let session_active = sessions.has_session();
                let last_error = last_error
                    .lock()
                    .expect("runtime status error state poisoned")
                    .clone();
                match tokio::task::spawn_blocking(move || {
                    write_runtime_snapshot(
                        &state_directory,
                        &instance_id,
                        host,
                        started,
                        daemon,
                        session_active,
                        last_error,
                    )
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "writing bounded Sigil runtime status failed");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Sigil runtime status writer task failed");
                        break;
                    }
                }
            }
        });
        publisher.task = Some(task);
        Ok(publisher)
    }

    pub async fn mark_ready(&self) -> Result<()> {
        self.daemon_state.store(DAEMON_READY, Ordering::Relaxed);
        self.write_snapshot_async().await
    }

    pub async fn mark_stopping(&self) -> Result<()> {
        self.daemon_state.store(DAEMON_STOPPING, Ordering::Relaxed);
        self.write_snapshot_async().await
    }

    pub async fn mark_degraded(&self, code: RuntimeErrorCode) -> Result<()> {
        self.daemon_state.store(DAEMON_DEGRADED, Ordering::Relaxed);
        *self
            .last_error
            .lock()
            .expect("runtime status error state poisoned") = Some(RuntimeErrorV1 {
            code,
            occurred_at_unix_ms: unix_timestamp_millis()?,
        });
        self.write_snapshot_async().await
    }

    pub async fn finish_clean(mut self) -> Result<()> {
        let write_lock = Arc::clone(&self.write_lock);
        let _write_guard = write_lock.lock().await;
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        self.daemon_state.store(DAEMON_STOPPED, Ordering::Relaxed);
        let state_directory = self.state_directory.clone();
        let instance_id = self.instance_id.clone();
        let host = self.host;
        let started = self.started;
        let session_active = self.sessions.has_session();
        let last_error = self
            .last_error
            .lock()
            .expect("runtime status error state poisoned")
            .clone();
        tokio::task::spawn_blocking(move || {
            write_runtime_snapshot(
                &state_directory,
                &instance_id,
                host,
                started,
                DaemonState::Stopped,
                session_active,
                last_error,
            )
        })
        .await
        .context("joining stopped runtime status write")?
    }

    fn write_snapshot(&self) -> Result<()> {
        write_runtime_snapshot(
            &self.state_directory,
            &self.instance_id,
            self.host,
            self.started,
            decode_daemon_state(self.daemon_state.load(Ordering::Relaxed)),
            self.sessions.has_session(),
            self.last_error
                .lock()
                .expect("runtime status error state poisoned")
                .clone(),
        )
    }

    async fn write_snapshot_async(&self) -> Result<()> {
        let write_lock = Arc::clone(&self.write_lock);
        let _write_guard = write_lock.lock().await;
        let state_directory = self.state_directory.clone();
        let instance_id = self.instance_id.clone();
        let host = self.host;
        let started = self.started;
        let daemon = decode_daemon_state(self.daemon_state.load(Ordering::Relaxed));
        let session_active = self.sessions.has_session();
        let last_error = self
            .last_error
            .lock()
            .expect("runtime status error state poisoned")
            .clone();
        tokio::task::spawn_blocking(move || {
            write_runtime_snapshot(
                &state_directory,
                &instance_id,
                host,
                started,
                daemon,
                session_active,
                last_error,
            )
        })
        .await
        .context("joining runtime status write")?
    }
}

impl Drop for RuntimePublisher {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn configured_runtime_directory() -> Result<Option<PathBuf>> {
    let Some(root) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return Ok(None);
    };
    let root = PathBuf::from(root);
    ensure!(root.is_absolute(), "XDG_RUNTIME_DIR must be absolute");
    secure_state::validate_private_directory(&root)
        .context("validating XDG_RUNTIME_DIR for Sigil appliance status")?;
    Ok(Some(root.join("sigil-spark")))
}

pub fn collect_status(config_path: &Path) -> Result<ApplianceStatusV1> {
    let config = HostConfig::load(config_path)?;
    let secret = crate::identity::load(&config.identity_path)?;
    let host = secret.public();
    let authorization = AuthorizationStore::inspect_existing(&config.state_path, host)?;
    let runtime = match configured_runtime_directory()? {
        Some(runtime_directory) => {
            read_runtime_status(&runtime_directory, host, unix_timestamp_millis()?)?
        }
        None => unavailable_runtime(),
    };
    Ok(assemble_status(host, authorization, runtime))
}

fn assemble_status(
    host: EndpointId,
    authorization: AuthorizationInspection,
    runtime: RuntimeStatus,
) -> ApplianceStatusV1 {
    let snapshot = authorization.snapshot;
    let grants = snapshot.grants.map(grant_list).unwrap_or_default();
    let enrollment_active = snapshot.peer.is_some();
    let overall = match (runtime.daemon, runtime.session) {
        (DaemonState::Ready, SessionState::Active) => OverallState::Active,
        (DaemonState::Ready, _) => OverallState::Ready,
        (DaemonState::Degraded, _) => OverallState::Degraded,
        _ => OverallState::Unavailable,
    };
    ApplianceStatusV1 {
        schema_version: 1,
        sigil_version: env!("CARGO_PKG_VERSION"),
        overall,
        config: ConfigStatus { state: "valid" },
        identity: IdentityStatus {
            state: "ready",
            host_fingerprint: fingerprint(host),
        },
        enrollment: EnrollmentStatus {
            state: if enrollment_active { "active" } else { "none" },
            storage: if authorization.storage_present {
                "present"
            } else {
                "absent"
            },
            peer_fingerprint: snapshot.peer.map(fingerprint),
            grants,
            epoch: snapshot.epoch,
            enrolled_at_unix: snapshot.enrolled_at_unix,
        },
        runtime,
    }
}

fn read_runtime_status(
    state_directory: &Path,
    expected_host: EndpointId,
    now_unix_ms: u64,
) -> Result<RuntimeStatus> {
    match fs::symlink_metadata(state_directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(absent_runtime());
        }
        Err(error) => return Err(error).context("inspecting Sigil runtime state directory"),
        Ok(_) => {}
    }
    let Some(bytes) = secure_state::read_bounded(
        state_directory,
        RUNTIME_STATUS_FILE,
        MAX_RUNTIME_STATUS_BYTES,
    )?
    else {
        return Ok(absent_runtime());
    };
    let persisted: RuntimeStatusV1 =
        serde_json::from_slice(&bytes).context("parsing Sigil runtime status")?;
    ensure!(
        persisted.version == RUNTIME_STATUS_VERSION,
        "unsupported Sigil runtime status version"
    );
    ensure!(
        persisted.host_node_id.parse::<EndpointId>().ok() == Some(expected_host),
        "Sigil runtime status belongs to a different host identity"
    );
    ensure!(
        persisted.instance_id.len() == 32
            && persisted
                .instance_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "invalid Sigil runtime instance ID"
    );
    if let Some(error) = &persisted.last_error {
        ensure!(
            error.occurred_at_unix_ms <= persisted.updated_at_unix_ms,
            "Sigil runtime error timestamp follows its status update"
        );
    }
    let age_ms = match now_unix_ms.checked_sub(persisted.updated_at_unix_ms) {
        Some(age_ms) => age_ms,
        None if persisted.updated_at_unix_ms - now_unix_ms
            <= CLOCK_SKEW_TOLERANCE.as_millis() as u64 =>
        {
            0
        }
        None => return Ok(stale_runtime(persisted, None)),
    };
    if age_ms > STALE_AFTER.as_millis() as u64 {
        return Ok(stale_runtime(persisted, Some(age_ms)));
    }
    ensure!(
        !matches!(
            persisted.daemon,
            DaemonState::Offline | DaemonState::Unknown
        ),
        "runtime publisher wrote an invalid daemon state"
    );
    Ok(RuntimeStatus {
        state: RuntimeState::Fresh,
        daemon: persisted.daemon,
        uptime_ms: (!matches!(persisted.daemon, DaemonState::Stopped))
            .then_some(persisted.uptime_ms.saturating_add(age_ms)),
        heartbeat_age_ms: Some(age_ms),
        session: if persisted.daemon == DaemonState::Stopped {
            SessionState::Inactive
        } else if persisted.session_active {
            SessionState::Active
        } else {
            SessionState::Inactive
        },
        last_error: persisted.last_error,
    })
}

fn stale_runtime(persisted: RuntimeStatusV1, heartbeat_age_ms: Option<u64>) -> RuntimeStatus {
    let stopped = persisted.daemon == DaemonState::Stopped;
    RuntimeStatus {
        state: RuntimeState::Stale,
        daemon: if stopped {
            DaemonState::Stopped
        } else {
            DaemonState::Unknown
        },
        uptime_ms: None,
        heartbeat_age_ms,
        session: if stopped {
            SessionState::Inactive
        } else {
            SessionState::Unknown
        },
        last_error: persisted.last_error,
    }
}

fn absent_runtime() -> RuntimeStatus {
    RuntimeStatus {
        state: RuntimeState::Absent,
        daemon: DaemonState::Offline,
        uptime_ms: None,
        heartbeat_age_ms: None,
        session: SessionState::Unknown,
        last_error: None,
    }
}

fn unavailable_runtime() -> RuntimeStatus {
    RuntimeStatus {
        state: RuntimeState::Unavailable,
        daemon: DaemonState::Unknown,
        uptime_ms: None,
        heartbeat_age_ms: None,
        session: SessionState::Unknown,
        last_error: None,
    }
}

fn write_runtime_snapshot(
    state_directory: &Path,
    instance_id: &str,
    host: EndpointId,
    started: Instant,
    daemon: DaemonState,
    session_active: bool,
    last_error: Option<RuntimeErrorV1>,
) -> Result<()> {
    let status = RuntimeStatusV1 {
        version: RUNTIME_STATUS_VERSION,
        instance_id: instance_id.to_owned(),
        host_node_id: host.to_string(),
        updated_at_unix_ms: unix_timestamp_millis()?,
        uptime_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        daemon,
        session_active,
        last_error,
    };
    let bytes = serde_json::to_vec(&status)?;
    secure_state::atomic_write(
        state_directory,
        RUNTIME_STATUS_FILE,
        &bytes,
        MAX_RUNTIME_STATUS_BYTES,
    )
}

fn decode_daemon_state(value: u8) -> DaemonState {
    match value {
        DAEMON_READY => DaemonState::Ready,
        DAEMON_STOPPING => DaemonState::Stopping,
        DAEMON_DEGRADED => DaemonState::Degraded,
        DAEMON_STOPPED => DaemonState::Stopped,
        _ => DaemonState::Starting,
    }
}

fn random_instance_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("generating runtime instance ID")?;
    let mut output = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing into a String cannot fail");
    }
    Ok(output)
}

fn unix_timestamp_millis() -> Result<u64> {
    Ok(u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_millis(),
    )
    .unwrap_or(u64::MAX))
}

fn fingerprint(id: EndpointId) -> String {
    let full = id.to_string();
    debug_assert_eq!(full.len(), 64);
    format!("{}…{}", &full[..8], &full[full.len() - 8..])
}

fn grant_list(grants: InvitationGrants) -> Vec<&'static str> {
    let mut names = vec!["view"];
    if grants.contains(InvitationGrants::POINTER_KEYBOARD) {
        names.push("pointer_keyboard");
    }
    if grants.contains(InvitationGrants::GAMEPAD) {
        names.push("gamepad");
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn private_directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn runtime_document(updated_at_unix_ms: u64) -> RuntimeStatusV1 {
        let host = SecretKey::from_bytes(&[7; 32]).public();
        RuntimeStatusV1 {
            version: RUNTIME_STATUS_VERSION,
            instance_id: "0123456789abcdef0123456789abcdef".to_owned(),
            host_node_id: host.to_string(),
            updated_at_unix_ms,
            uptime_ms: 1_000,
            daemon: DaemonState::Ready,
            session_active: true,
            last_error: None,
        }
    }

    #[test]
    fn lifecycle_lock_is_global_across_distinct_state_directories() {
        let first_state = private_directory();
        let second_state = private_directory();
        let runtime = private_directory();
        let first = LifecycleGuard::acquire_at(first_state.path(), Some(runtime.path())).unwrap();
        assert!(LifecycleGuard::acquire_at(second_state.path(), Some(runtime.path())).is_err());
        drop(first);
        LifecycleGuard::acquire_at(second_state.path(), Some(runtime.path())).unwrap();
    }

    #[test]
    fn missing_runtime_is_offline_without_creating_state() {
        let directory = private_directory();
        let missing = directory.path().join("missing");
        let host = SecretKey::from_bytes(&[7; 32]).public();
        let status = read_runtime_status(&missing, host, 20_000).unwrap();
        assert_eq!(status.state, RuntimeState::Absent);
        assert_eq!(status.daemon, DaemonState::Offline);
        assert_eq!(status.session, SessionState::Unknown);
        assert!(!missing.exists());
    }

    #[test]
    fn runtime_freshness_suppresses_stale_session_claims() {
        let directory = private_directory();
        let fresh = serde_json::to_vec(&runtime_document(19_000)).unwrap();
        secure_state::atomic_write(
            directory.path(),
            RUNTIME_STATUS_FILE,
            &fresh,
            MAX_RUNTIME_STATUS_BYTES,
        )
        .unwrap();
        let host = SecretKey::from_bytes(&[7; 32]).public();
        let status = read_runtime_status(directory.path(), host, 20_000).unwrap();
        assert_eq!(status.state, RuntimeState::Fresh);
        assert_eq!(status.session, SessionState::Active);
        assert_eq!(status.uptime_ms, Some(2_000));

        let status = read_runtime_status(directory.path(), host, 30_001).unwrap();
        assert_eq!(status.state, RuntimeState::Stale);
        assert_eq!(status.daemon, DaemonState::Unknown);
        assert_eq!(status.session, SessionState::Unknown);
        assert_eq!(status.uptime_ms, None);
    }

    #[test]
    fn clock_skew_is_bounded_and_unknown_or_cross_host_documents_fail() {
        let directory = private_directory();
        let host = SecretKey::from_bytes(&[7; 32]).public();
        let future = serde_json::to_vec(&runtime_document(20_001)).unwrap();
        secure_state::atomic_write(
            directory.path(),
            RUNTIME_STATUS_FILE,
            &future,
            MAX_RUNTIME_STATUS_BYTES,
        )
        .unwrap();
        assert_eq!(
            read_runtime_status(directory.path(), host, 20_000)
                .unwrap()
                .state,
            RuntimeState::Fresh
        );

        let far_future = serde_json::to_vec(&runtime_document(26_000)).unwrap();
        secure_state::atomic_write(
            directory.path(),
            RUNTIME_STATUS_FILE,
            &far_future,
            MAX_RUNTIME_STATUS_BYTES,
        )
        .unwrap();
        assert_eq!(
            read_runtime_status(directory.path(), host, 20_000)
                .unwrap()
                .state,
            RuntimeState::Stale
        );

        let mut unknown = runtime_document(20_000);
        unknown.version = 2;
        let unknown = serde_json::to_vec(&unknown).unwrap();
        secure_state::atomic_write(
            directory.path(),
            RUNTIME_STATUS_FILE,
            &unknown,
            MAX_RUNTIME_STATUS_BYTES,
        )
        .unwrap();
        assert!(read_runtime_status(directory.path(), host, 20_000).is_err());

        let other_host = SecretKey::from_bytes(&[8; 32]).public();
        let cross_host = serde_json::to_vec(&runtime_document(20_000)).unwrap();
        secure_state::atomic_write(
            directory.path(),
            RUNTIME_STATUS_FILE,
            &cross_host,
            MAX_RUNTIME_STATUS_BYTES,
        )
        .unwrap();
        assert!(read_runtime_status(directory.path(), other_host, 20_000).is_err());
    }

    #[test]
    fn assembled_status_redacts_endpoint_ids_and_orders_grants() {
        let host = SecretKey::from_bytes(&[7; 32]).public();
        let peer = SecretKey::from_bytes(&[9; 32]).public();
        let status = assemble_status(
            host,
            AuthorizationInspection {
                snapshot: crate::authorization::AuthorizationSnapshot {
                    epoch: 4,
                    peer: Some(peer),
                    grants: Some(InvitationGrants::ALL),
                    enrolled_at_unix: Some(123),
                },
                storage_present: true,
            },
            RuntimeStatus {
                state: RuntimeState::Fresh,
                daemon: DaemonState::Ready,
                uptime_ms: Some(50),
                heartbeat_age_ms: Some(1),
                session: SessionState::Active,
                last_error: None,
            },
        );
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains(&host.to_string()));
        assert!(!json.contains(&peer.to_string()));
        assert!(json.contains(&fingerprint(host)));
        assert!(json.contains(&fingerprint(peer)));
        assert!(json.contains("\"grants\":[\"view\",\"pointer_keyboard\",\"gamepad\"]"));
    }

    #[tokio::test]
    async fn publisher_writes_transitions_and_retains_an_explicit_stopped_state() {
        let directory = private_directory();
        let sessions = Arc::new(SessionRegistry::default());
        let host = SecretKey::from_bytes(&[7; 32]).public();
        let publisher = RuntimePublisher::start_at(directory.path(), host, sessions).unwrap();
        publisher.mark_ready().await.unwrap();
        publisher.write_snapshot().unwrap();
        let status =
            read_runtime_status(directory.path(), host, unix_timestamp_millis().unwrap()).unwrap();
        assert_eq!(status.daemon, DaemonState::Ready);
        publisher.mark_stopping().await.unwrap();
        publisher.finish_clean().await.unwrap();
        let status =
            read_runtime_status(directory.path(), host, unix_timestamp_millis().unwrap()).unwrap();
        assert_eq!(status.daemon, DaemonState::Stopped);
        assert_eq!(status.session, SessionState::Inactive);
    }
}
