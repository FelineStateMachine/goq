use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use sigil_protocol::InvitationGrants;

use crate::authorization::{AuthorizationInspection, AuthorizationStore};
use crate::config::{ConfigRevision, HostConfig};
use crate::config_management::ConfigTransactionSummaryV1;
use crate::secure_state;
use crate::server::SessionRegistry;

const LIFECYCLE_LOCK_FILE: &str = "daemon-v1.lock";
const GLOBAL_LIFECYCLE_LOCK_FILE: &str = "daemon-global-v1.lock";
const RUNTIME_STATUS_FILE: &str = "daemon-status-v1.json";
const RUNTIME_STATUS_VERSION: u16 = 2;
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
        Self::try_acquire(state_directory, require_global).map_err(anyhow::Error::new)
    }

    pub(crate) fn try_acquire(
        state_directory: &Path,
        require_global: bool,
    ) -> std::result::Result<Self, secure_state::LockAcquireError> {
        let runtime_directory =
            configured_runtime_directory().map_err(secure_state::LockAcquireError::Unsafe)?;
        if require_global && runtime_directory.is_none() {
            return Err(secure_state::LockAcquireError::Unsafe(anyhow::anyhow!(
                "configured Sigil service requires a valid XDG_RUNTIME_DIR"
            )));
        }
        Self::try_acquire_at(state_directory, runtime_directory.as_deref())
    }

    #[cfg(test)]
    fn acquire_at(state_directory: &Path, runtime_directory: Option<&Path>) -> Result<Self> {
        Self::try_acquire_at(state_directory, runtime_directory).map_err(anyhow::Error::new)
    }

    pub(crate) fn try_acquire_at(
        state_directory: &Path,
        runtime_directory: Option<&Path>,
    ) -> std::result::Result<Self, secure_state::LockAcquireError> {
        let mut files = vec![secure_state::try_open_lifetime_lock(
            state_directory,
            LIFECYCLE_LOCK_FILE,
        )?];
        if let Some(runtime_directory) = runtime_directory {
            secure_state::ensure_private_directory(runtime_directory)
                .map_err(secure_state::LockAcquireError::Unsafe)?;
            files.push(secure_state::try_open_lifetime_lock(
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

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeStatusV2 {
    version: u16,
    instance_id: String,
    host_node_id: String,
    loaded_config_revision: Option<ConfigRevision>,
    reached_ready: bool,
    updated_at_unix_ms: u64,
    uptime_ms: u64,
    daemon: DaemonState,
    session_active: bool,
    last_error: Option<RuntimeErrorV1>,
}

#[derive(Debug, Serialize)]
pub struct ApplianceStatusV2 {
    schema_version: u16,
    sigil_version: &'static str,
    overall: OverallState,
    config: ConfigStatus,
    identity: IdentityStatus,
    enrollment: EnrollmentStatus,
    runtime: RuntimeStatus,
}

#[derive(Debug, Serialize)]
pub struct EnrollmentResetV1 {
    schema_version: u16,
    operation: &'static str,
    host_fingerprint: String,
    had_enrollment: bool,
    previous_epoch: u64,
    current_epoch: u64,
    invitations_invalidated: bool,
}

#[derive(Debug, Serialize)]
struct ConfigStatus {
    state: &'static str,
    revision: ConfigRevision,
    pending_transaction: Option<ConfigTransactionSummaryV1>,
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
    instance_id: Option<String>,
    loaded_config_revision: Option<ConfigRevision>,
    reached_ready: bool,
    last_error: Option<RuntimeErrorV1>,
}

pub struct RuntimePublisher {
    state_directory: PathBuf,
    instance_id: String,
    host: EndpointId,
    loaded_config_revision: Option<ConfigRevision>,
    started: Instant,
    daemon_state: Arc<AtomicU8>,
    reached_ready: Arc<AtomicBool>,
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
        loaded_config_revision: Option<ConfigRevision>,
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
        ensure!(
            !require_runtime || loaded_config_revision.is_some(),
            "configured Sigil service requires a loaded config revision"
        );
        Self::start_at(&state_directory, host, sessions, loaded_config_revision).map(Some)
    }

    fn start_at(
        state_directory: &Path,
        host: EndpointId,
        sessions: Arc<SessionRegistry>,
        loaded_config_revision: Option<ConfigRevision>,
    ) -> Result<Self> {
        let instance_id = random_instance_id()?;
        let daemon_state = Arc::new(AtomicU8::new(DAEMON_STARTING));
        let reached_ready = Arc::new(AtomicBool::new(false));
        let last_error = Arc::new(Mutex::new(None));
        let write_lock = Arc::new(tokio::sync::Mutex::new(()));
        let mut publisher = Self {
            state_directory: state_directory.to_path_buf(),
            instance_id,
            host,
            loaded_config_revision,
            started: Instant::now(),
            daemon_state,
            reached_ready,
            last_error,
            sessions,
            write_lock,
            task: None,
        };
        publisher.write_snapshot()?;

        let state_directory = publisher.state_directory.clone();
        let instance_id = publisher.instance_id.clone();
        let host = publisher.host;
        let loaded_config_revision = publisher.loaded_config_revision.clone();
        let started = publisher.started;
        let daemon_state = Arc::clone(&publisher.daemon_state);
        let reached_ready = Arc::clone(&publisher.reached_ready);
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
                let loaded_config_revision = loaded_config_revision.clone();
                let daemon = decode_daemon_state(daemon_state.load(Ordering::Relaxed));
                let reached_ready = reached_ready.load(Ordering::Relaxed);
                let session_active = sessions.has_session();
                let last_error = last_error
                    .lock()
                    .expect("runtime status error state poisoned")
                    .clone();
                match tokio::task::spawn_blocking(move || {
                    write_runtime_snapshot(RuntimeSnapshotInput {
                        state_directory,
                        instance_id,
                        host,
                        loaded_config_revision,
                        started,
                        daemon,
                        reached_ready,
                        session_active,
                        last_error,
                    })
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
        self.reached_ready.store(true, Ordering::Relaxed);
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
        let loaded_config_revision = self.loaded_config_revision.clone();
        let started = self.started;
        let reached_ready = self.reached_ready.load(Ordering::Relaxed);
        let session_active = self.sessions.has_session();
        let last_error = self
            .last_error
            .lock()
            .expect("runtime status error state poisoned")
            .clone();
        tokio::task::spawn_blocking(move || {
            write_runtime_snapshot(RuntimeSnapshotInput {
                state_directory,
                instance_id,
                host,
                loaded_config_revision,
                started,
                daemon: DaemonState::Stopped,
                reached_ready,
                session_active,
                last_error,
            })
        })
        .await
        .context("joining stopped runtime status write")?
    }

    fn write_snapshot(&self) -> Result<()> {
        write_runtime_snapshot(RuntimeSnapshotInput {
            state_directory: self.state_directory.clone(),
            instance_id: self.instance_id.clone(),
            host: self.host,
            loaded_config_revision: self.loaded_config_revision.clone(),
            started: self.started,
            daemon: decode_daemon_state(self.daemon_state.load(Ordering::Relaxed)),
            reached_ready: self.reached_ready.load(Ordering::Relaxed),
            session_active: self.sessions.has_session(),
            last_error: self
                .last_error
                .lock()
                .expect("runtime status error state poisoned")
                .clone(),
        })
    }

    async fn write_snapshot_async(&self) -> Result<()> {
        let write_lock = Arc::clone(&self.write_lock);
        let _write_guard = write_lock.lock().await;
        let state_directory = self.state_directory.clone();
        let instance_id = self.instance_id.clone();
        let host = self.host;
        let loaded_config_revision = self.loaded_config_revision.clone();
        let started = self.started;
        let daemon = decode_daemon_state(self.daemon_state.load(Ordering::Relaxed));
        let reached_ready = self.reached_ready.load(Ordering::Relaxed);
        let session_active = self.sessions.has_session();
        let last_error = self
            .last_error
            .lock()
            .expect("runtime status error state poisoned")
            .clone();
        tokio::task::spawn_blocking(move || {
            write_runtime_snapshot(RuntimeSnapshotInput {
                state_directory,
                instance_id,
                host,
                loaded_config_revision,
                started,
                daemon,
                reached_ready,
                session_active,
                last_error,
            })
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

pub fn collect_status(config_path: &Path) -> Result<ApplianceStatusV2> {
    let loaded = HostConfig::load_document(config_path)?;
    let config = &loaded.config;
    let secret = crate::identity::load(&config.identity_path)?;
    let host = secret.public();
    let authorization = AuthorizationStore::inspect_existing(&config.state_path, host)?;
    let pending_transaction = crate::config_management::inspect_transaction(&loaded, config_path)
        .map_err(anyhow::Error::from)?;
    let runtime = match configured_runtime_directory()? {
        Some(runtime_directory) => {
            read_runtime_status(&runtime_directory, host, unix_timestamp_millis()?)?
        }
        None => unavailable_runtime(),
    };
    Ok(assemble_status(
        host,
        authorization,
        runtime,
        loaded.revision,
        pending_transaction,
    ))
}

pub fn status_json(status: &ApplianceStatusV2, schema_version: u16) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(status)?;
    match schema_version {
        1 => {
            value["schema_version"] = serde_json::json!(1);
            let config = value["config"]
                .as_object_mut()
                .context("appliance config status is not an object")?;
            config.remove("revision");
            config.remove("pending_transaction");
            let runtime = value["runtime"]
                .as_object_mut()
                .context("appliance runtime status is not an object")?;
            runtime.remove("instance_id");
            runtime.remove("loaded_config_revision");
            runtime.remove("reached_ready");
        }
        2 => {}
        _ => anyhow::bail!("unsupported appliance status schema"),
    }
    Ok(value)
}

pub fn reset_enrollment(
    config_path: &Path,
    expected_host_fingerprint: &str,
) -> Result<EnrollmentResetV1> {
    let config = HostConfig::load(config_path)?;
    config.ensure_runtime_directory()?;
    let _lifecycle = LifecycleGuard::acquire(&config.state_path, true)?;
    let secret = crate::identity::load(&config.identity_path)?;
    let host = secret.public();
    let host_fingerprint = fingerprint(host);
    ensure!(
        expected_host_fingerprint == host_fingerprint,
        "expected host fingerprint does not match this Sigil appliance"
    );

    let store = AuthorizationStore::open(config.state_path, host)?;
    let outcome = store.revoke_with_outcome(crate::authorization::unix_timestamp_now()?)?;
    ensure!(
        outcome.current_epoch
            == outcome
                .previous_epoch
                .checked_add(1)
                .context("authorization epoch exhausted")?,
        "enrollment reset did not advance the authorization epoch"
    );

    Ok(EnrollmentResetV1 {
        schema_version: 1,
        operation: "enrollment_reset",
        host_fingerprint,
        had_enrollment: outcome.had_enrollment,
        previous_epoch: outcome.previous_epoch,
        current_epoch: outcome.current_epoch,
        invitations_invalidated: true,
    })
}

fn assemble_status(
    host: EndpointId,
    authorization: AuthorizationInspection,
    runtime: RuntimeStatus,
    config_revision: ConfigRevision,
    pending_transaction: Option<ConfigTransactionSummaryV1>,
) -> ApplianceStatusV2 {
    let snapshot = authorization.snapshot;
    let grants = snapshot.grants.map(grant_list).unwrap_or_default();
    let enrollment_active = snapshot.peer.is_some();
    let overall = match (runtime.daemon, runtime.session) {
        (DaemonState::Ready, SessionState::Active) => OverallState::Active,
        (DaemonState::Ready, _) => OverallState::Ready,
        (DaemonState::Degraded, _) => OverallState::Degraded,
        _ => OverallState::Unavailable,
    };
    ApplianceStatusV2 {
        schema_version: 2,
        sigil_version: env!("CARGO_PKG_VERSION"),
        overall,
        config: ConfigStatus {
            state: "valid",
            revision: config_revision,
            pending_transaction,
        },
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
    let Some(persisted) = read_persisted_runtime(state_directory)? else {
        return Ok(absent_runtime());
    };
    ensure!(
        persisted.host_node_id.parse::<EndpointId>().ok() == Some(expected_host),
        "Sigil runtime status belongs to a different host identity"
    );
    ensure!(
        persisted.instance_id.len() == 32
            && persisted
                .instance_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "invalid Sigil runtime instance ID"
    );
    if let Some(revision) = &persisted.loaded_config_revision {
        ConfigRevision::parse(revision.as_str())?;
    }
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
        instance_id: Some(persisted.instance_id),
        loaded_config_revision: persisted.loaded_config_revision,
        reached_ready: persisted.reached_ready,
        last_error: persisted.last_error,
    })
}

fn read_persisted_runtime(state_directory: &Path) -> Result<Option<RuntimeStatusV2>> {
    let Some(bytes) = secure_state::read_bounded(
        state_directory,
        RUNTIME_STATUS_FILE,
        MAX_RUNTIME_STATUS_BYTES,
    )?
    else {
        return Ok(None);
    };
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).context("parsing Sigil runtime status")?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .context("Sigil runtime status version is missing")?;
    match version {
        1 => {
            let legacy: RuntimeStatusV1 =
                serde_json::from_value(value).context("parsing legacy Sigil runtime status")?;
            Ok(Some(RuntimeStatusV2 {
                version: legacy.version,
                instance_id: legacy.instance_id,
                host_node_id: legacy.host_node_id,
                loaded_config_revision: None,
                reached_ready: false,
                updated_at_unix_ms: legacy.updated_at_unix_ms,
                uptime_ms: legacy.uptime_ms,
                daemon: legacy.daemon,
                session_active: legacy.session_active,
                last_error: legacy.last_error,
            }))
        }
        2 => {
            let current: RuntimeStatusV2 =
                serde_json::from_value(value).context("parsing Sigil runtime status v2")?;
            ensure!(
                current.version == RUNTIME_STATUS_VERSION,
                "unsupported Sigil runtime status version"
            );
            Ok(Some(current))
        }
        _ => anyhow::bail!("unsupported Sigil runtime status version"),
    }
}

pub(crate) fn latest_runtime_instance(expected_host: EndpointId) -> Result<Option<String>> {
    let state_directory = configured_runtime_directory()?
        .context("configured Sigil service requires a valid XDG_RUNTIME_DIR")?;
    match fs::symlink_metadata(&state_directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspecting Sigil runtime state directory"),
        Ok(_) => {}
    }
    let Some(persisted) = read_persisted_runtime(&state_directory)? else {
        return Ok(None);
    };
    ensure!(
        persisted.host_node_id.parse::<EndpointId>().ok() == Some(expected_host),
        "Sigil runtime status belongs to a different host identity"
    );
    ensure!(
        persisted.instance_id.len() == 32
            && persisted
                .instance_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "invalid Sigil runtime instance ID"
    );
    Ok(Some(persisted.instance_id))
}

fn stale_runtime(persisted: RuntimeStatusV2, heartbeat_age_ms: Option<u64>) -> RuntimeStatus {
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
        instance_id: None,
        loaded_config_revision: None,
        reached_ready: false,
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
        instance_id: None,
        loaded_config_revision: None,
        reached_ready: false,
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
        instance_id: None,
        loaded_config_revision: None,
        reached_ready: false,
        last_error: None,
    }
}

pub(crate) fn validate_candidate_runtime(
    expected_host: EndpointId,
    expected_revision: &ConfigRevision,
    expected_instance: &str,
) -> Result<()> {
    let state_directory = configured_runtime_directory()?
        .context("configured Sigil service requires a valid XDG_RUNTIME_DIR")?;
    validate_candidate_runtime_at(
        &state_directory,
        expected_host,
        expected_revision,
        expected_instance,
        unix_timestamp_millis()?,
    )
}

fn validate_candidate_runtime_at(
    state_directory: &Path,
    expected_host: EndpointId,
    expected_revision: &ConfigRevision,
    expected_instance: &str,
    now_unix_ms: u64,
) -> Result<()> {
    let status = read_runtime_status(state_directory, expected_host, now_unix_ms)?;
    ensure!(
        status.state == RuntimeState::Fresh,
        "runtime evidence is not fresh"
    );
    ensure!(
        status.daemon == DaemonState::Stopped,
        "candidate daemon has not stopped cleanly"
    );
    ensure!(
        status.instance_id.as_deref() == Some(expected_instance),
        "runtime evidence belongs to a different daemon instance"
    );
    ensure!(
        status.loaded_config_revision.as_ref() == Some(expected_revision),
        "runtime evidence belongs to a different configuration revision"
    );
    ensure!(status.reached_ready, "candidate daemon never reached ready");
    ensure!(
        status.last_error.is_none(),
        "candidate daemon reported an error"
    );
    Ok(())
}

struct RuntimeSnapshotInput {
    state_directory: PathBuf,
    instance_id: String,
    host: EndpointId,
    loaded_config_revision: Option<ConfigRevision>,
    started: Instant,
    daemon: DaemonState,
    reached_ready: bool,
    session_active: bool,
    last_error: Option<RuntimeErrorV1>,
}

fn write_runtime_snapshot(input: RuntimeSnapshotInput) -> Result<()> {
    let status = RuntimeStatusV2 {
        version: RUNTIME_STATUS_VERSION,
        instance_id: input.instance_id,
        host_node_id: input.host.to_string(),
        loaded_config_revision: input.loaded_config_revision,
        reached_ready: input.reached_ready,
        updated_at_unix_ms: unix_timestamp_millis()?,
        uptime_ms: u64::try_from(input.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        daemon: input.daemon,
        session_active: input.session_active,
        last_error: input.last_error,
    };
    let bytes = serde_json::to_vec(&status)?;
    secure_state::atomic_write(
        &input.state_directory,
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

pub(crate) fn fingerprint(id: EndpointId) -> String {
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
            version: 1,
            instance_id: "0123456789abcdef0123456789abcdef".to_owned(),
            host_node_id: host.to_string(),
            updated_at_unix_ms,
            uptime_ms: 1_000,
            daemon: DaemonState::Ready,
            session_active: true,
            last_error: None,
        }
    }

    fn current_runtime_document(
        updated_at_unix_ms: u64,
        revision: ConfigRevision,
    ) -> RuntimeStatusV2 {
        let host = SecretKey::from_bytes(&[7; 32]).public();
        RuntimeStatusV2 {
            version: RUNTIME_STATUS_VERSION,
            instance_id: "0123456789abcdef0123456789abcdef".to_owned(),
            host_node_id: host.to_string(),
            loaded_config_revision: Some(revision),
            reached_ready: true,
            updated_at_unix_ms,
            uptime_ms: 1_000,
            daemon: DaemonState::Stopped,
            session_active: false,
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
        unknown.version = 99;
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
    fn candidate_health_requires_exact_fresh_stopped_ready_instance_and_revision() {
        let directory = private_directory();
        let host = SecretKey::from_bytes(&[7; 32]).public();
        let revision = ConfigRevision::from_bytes(b"candidate");
        let instance = "0123456789abcdef0123456789abcdef";
        let valid = current_runtime_document(20_000, revision.clone());
        let write = |document: &RuntimeStatusV2| {
            secure_state::atomic_write(
                directory.path(),
                RUNTIME_STATUS_FILE,
                &serde_json::to_vec(document).unwrap(),
                MAX_RUNTIME_STATUS_BYTES,
            )
            .unwrap();
        };
        write(&valid);
        validate_candidate_runtime_at(directory.path(), host, &revision, instance, 20_000).unwrap();

        let mut invalid = valid.clone();
        invalid.reached_ready = false;
        write(&invalid);
        assert!(
            validate_candidate_runtime_at(directory.path(), host, &revision, instance, 20_000)
                .is_err()
        );

        let mut invalid = valid.clone();
        invalid.daemon = DaemonState::Ready;
        write(&invalid);
        assert!(
            validate_candidate_runtime_at(directory.path(), host, &revision, instance, 20_000)
                .is_err()
        );

        write(&valid);
        assert!(
            validate_candidate_runtime_at(
                directory.path(),
                host,
                &ConfigRevision::from_bytes(b"other"),
                instance,
                20_000,
            )
            .is_err()
        );
        assert!(
            validate_candidate_runtime_at(
                directory.path(),
                host,
                &revision,
                "fedcba9876543210fedcba9876543210",
                20_000,
            )
            .is_err()
        );
        assert!(
            validate_candidate_runtime_at(directory.path(), host, &revision, instance, 40_001)
                .is_err()
        );
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
                instance_id: Some("0123456789abcdef0123456789abcdef".to_owned()),
                loaded_config_revision: Some(ConfigRevision::from_bytes(b"config")),
                reached_ready: true,
                last_error: None,
            },
            ConfigRevision::from_bytes(b"config"),
            None,
        );
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains(&host.to_string()));
        assert!(!json.contains(&peer.to_string()));
        assert!(json.contains(&fingerprint(host)));
        assert!(json.contains(&fingerprint(peer)));
        assert!(json.contains("\"grants\":[\"view\",\"pointer_keyboard\",\"gamepad\"]"));
    }

    #[test]
    fn public_status_v1_shape_remains_compatible_and_v2_is_explicit() {
        let host = SecretKey::from_bytes(&[7; 32]).public();
        let revision = ConfigRevision::from_bytes(b"config");
        let status = assemble_status(
            host,
            AuthorizationInspection {
                snapshot: crate::authorization::AuthorizationSnapshot {
                    epoch: 0,
                    peer: None,
                    grants: None,
                    enrolled_at_unix: None,
                },
                storage_present: false,
            },
            absent_runtime(),
            revision,
            None,
        );
        let legacy = status_json(&status, 1).unwrap();
        assert_eq!(legacy["schema_version"], 1);
        assert!(legacy["config"].get("revision").is_none());
        assert!(legacy["config"].get("pending_transaction").is_none());
        assert!(legacy["runtime"].get("instance_id").is_none());
        assert!(legacy["runtime"].get("loaded_config_revision").is_none());
        assert!(legacy["runtime"].get("reached_ready").is_none());

        let current = status_json(&status, 2).unwrap();
        assert_eq!(current["schema_version"], 2);
        assert!(current["config"].get("revision").is_some());
        assert!(current["runtime"].get("reached_ready").is_some());
    }

    #[tokio::test]
    async fn publisher_writes_transitions_and_retains_an_explicit_stopped_state() {
        let directory = private_directory();
        let sessions = Arc::new(SessionRegistry::default());
        let host = SecretKey::from_bytes(&[7; 32]).public();
        let revision = ConfigRevision::from_bytes(b"config");
        let publisher =
            RuntimePublisher::start_at(directory.path(), host, sessions, Some(revision.clone()))
                .unwrap();
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
        assert_eq!(status.loaded_config_revision, Some(revision));
        assert!(status.reached_ready);
    }
}
