use std::fs;
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, value};

use crate::appliance::LifecycleGuard;
use crate::config::{
    ConfigRevision, HostConfig, LoadedHostConfig, MAX_CONFIG_BYTES, VaapiRateControl, VideoSource,
};
use crate::{appliance, identity, secure_state};

const REQUEST_VERSION: u16 = 1;
const RESULT_VERSION: u16 = 1;
const JOURNAL_VERSION: u16 = 1;
const MAX_REQUEST_BYTES: u64 = 16 * 1024;
const MAX_JOURNAL_BYTES: u64 = 16 * 1024;
const CONFIG_LOCK_FILE: &str = "config-transaction-v1.lock";
const JOURNAL_FILE: &str = "config-transaction-v1.json";
const BASE_FILE: &str = "config-base-v1.toml";
const CANDIDATE_FILE: &str = "config-candidate-v1.toml";

pub type ManagementResult<T> = std::result::Result<T, ManagementError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagementErrorCode {
    UnsupportedSchema,
    InvalidRequest,
    ValidationFailed,
    RevisionConflict,
    LifecycleBusy,
    TransactionBusy,
    TransactionPending,
    TransactionNotFound,
    TransactionConflict,
    HealthNotProven,
    UnsafeStorage,
}

#[derive(Debug)]
pub struct ManagementError {
    code: ManagementErrorCode,
    source: anyhow::Error,
}

impl ManagementError {
    fn new(code: ManagementErrorCode, source: impl Into<anyhow::Error>) -> Self {
        Self {
            code,
            source: source.into(),
        }
    }

    pub fn response(&self) -> ManagementErrorV1 {
        ManagementErrorV1 {
            schema_version: RESULT_VERSION,
            error: ManagementErrorBody { code: self.code },
        }
    }
}

impl std::fmt::Display for ManagementError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "configuration management failed: {}",
            self.source
        )
    }
}

impl std::error::Error for ManagementError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Debug, Serialize)]
pub struct ManagementErrorV1 {
    schema_version: u16,
    error: ManagementErrorBody,
}

#[derive(Debug, Serialize)]
struct ManagementErrorBody {
    code: ManagementErrorCode,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigRequestV1 {
    #[serde(rename = "schema_version")]
    _schema_version: u16,
    expected_revision: ConfigRevision,
    settings: ManagedSettingsV1,
}

#[derive(Deserialize)]
struct ConfigRequestEnvelope {
    schema_version: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSettingsV1 {
    resolution: ManagedResolutionV1,
    framerate: u32,
    rate_control: Option<ManagedRateControlV1>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedResolutionV1 {
    Native,
    Fixed { width: u32, height: u32 },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedRateControlV1 {
    Cbr { bitrate_kbps: u32 },
    Cqp { quantizer: u8 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConfigTransactionSummaryV1 {
    transaction: String,
    state: TransactionState,
    base_revision: ConfigRevision,
    candidate_revision: ConfigRevision,
}

#[derive(Debug, Serialize)]
pub struct ConfigShowV1 {
    schema_version: u16,
    revision: ConfigRevision,
    settings: ManagedSettingsV1,
    pending_transaction: Option<ConfigTransactionSummaryV1>,
}

#[derive(Debug, Serialize)]
pub struct ConfigValidateV1 {
    schema_version: u16,
    base_revision: ConfigRevision,
    candidate_revision: ConfigRevision,
    changed: bool,
    settings: ManagedSettingsV1,
}

#[derive(Debug, Serialize)]
pub struct ConfigSetV1 {
    schema_version: u16,
    transaction: Option<String>,
    base_revision: ConfigRevision,
    candidate_revision: ConfigRevision,
    changed: bool,
    restart_required: bool,
}

#[derive(Debug, Serialize)]
pub struct ConfigCommitV1 {
    schema_version: u16,
    operation: &'static str,
    transaction: String,
    revision: ConfigRevision,
}

#[derive(Debug, Serialize)]
pub struct ConfigRollbackV1 {
    schema_version: u16,
    operation: &'static str,
    transaction: String,
    restored_revision: ConfigRevision,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionState {
    Prepared,
    PendingValidation,
    Committing,
    RollingBack,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TransactionJournalV1 {
    version: u16,
    transaction: String,
    state: TransactionState,
    config_path_id: ConfigRevision,
    base_revision: ConfigRevision,
    candidate_revision: ConfigRevision,
    baseline_runtime_instance: Option<String>,
}

enum RecoveryCompletion {
    Committed(TransactionJournalV1),
    RolledBack(TransactionJournalV1),
}

struct RecoveryOutcome {
    journal: Option<TransactionJournalV1>,
    completion: Option<RecoveryCompletion>,
}

pub fn read_request(reader: impl Read) -> ManagementResult<ConfigRequestV1> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_REQUEST_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| ManagementError::new(ManagementErrorCode::InvalidRequest, error))?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_REQUEST_BYTES {
        return Err(ManagementError::new(
            ManagementErrorCode::InvalidRequest,
            anyhow::anyhow!("configuration request is empty or exceeds its fixed bound"),
        ));
    }
    let envelope: ConfigRequestEnvelope = serde_json::from_slice(&bytes)
        .map_err(|error| ManagementError::new(ManagementErrorCode::InvalidRequest, error))?;
    if envelope.schema_version != u64::from(REQUEST_VERSION) {
        return Err(ManagementError::new(
            ManagementErrorCode::UnsupportedSchema,
            anyhow::anyhow!("unsupported configuration request schema"),
        ));
    }
    let request: ConfigRequestV1 = serde_json::from_slice(&bytes)
        .map_err(|error| ManagementError::new(ManagementErrorCode::InvalidRequest, error))?;
    ConfigRevision::parse(request.expected_revision.as_str())
        .map_err(|error| ManagementError::new(ManagementErrorCode::InvalidRequest, error))?;
    Ok(request)
}

pub fn show(config_path: &Path) -> ManagementResult<ConfigShowV1> {
    let loaded = load_config(config_path)?;
    let pending_transaction = inspect_transaction(&loaded, config_path)?;
    Ok(ConfigShowV1 {
        schema_version: RESULT_VERSION,
        revision: loaded.revision,
        settings: project_settings(&loaded.config)?,
        pending_transaction,
    })
}

pub fn validate(
    config_path: &Path,
    request: &ConfigRequestV1,
) -> ManagementResult<ConfigValidateV1> {
    let loaded = load_config(config_path)?;
    require_expected_revision(&loaded, request)?;
    let candidate = build_candidate(&loaded, request)?;
    Ok(ConfigValidateV1 {
        schema_version: RESULT_VERSION,
        base_revision: loaded.revision,
        candidate_revision: ConfigRevision::from_bytes(&candidate.bytes),
        changed: candidate.bytes != loaded.bytes,
        settings: project_settings(&candidate.config)?,
    })
}

pub fn set(config_path: &Path, request: &ConfigRequestV1) -> ManagementResult<ConfigSetV1> {
    let bootstrap = load_config(config_path)?;
    let state_directory = bootstrap.config.state_path.clone();
    bootstrap
        .config
        .ensure_runtime_directory()
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error))?;
    let (_lifecycle, _config_lock) = acquire_management_locks(&state_directory)?;

    let mut loaded = load_config(config_path)?;
    ensure_bootstrap_paths_unchanged(&bootstrap, &loaded)?;
    let recovered = recover(config_path, &mut loaded)?;
    if recovered.journal.is_some() {
        return Err(ManagementError::new(
            ManagementErrorCode::TransactionPending,
            anyhow::anyhow!("a configuration transaction is already pending"),
        ));
    }
    loaded = load_config(config_path)?;
    ensure_bootstrap_paths_unchanged(&bootstrap, &loaded)?;
    require_expected_revision(&loaded, request)?;
    let candidate = build_candidate(&loaded, request)?;
    let candidate_revision = ConfigRevision::from_bytes(&candidate.bytes);
    if candidate.bytes == loaded.bytes {
        return Ok(ConfigSetV1 {
            schema_version: RESULT_VERSION,
            transaction: None,
            base_revision: loaded.revision.clone(),
            candidate_revision,
            changed: false,
            restart_required: false,
        });
    }

    let host = identity::load(&loaded.config.identity_path)
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error))?
        .public();
    let baseline_runtime_instance = appliance::latest_runtime_instance(host)
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error))?;

    write_artifact(&state_directory, BASE_FILE, &loaded.bytes)?;
    write_artifact(&state_directory, CANDIDATE_FILE, &candidate.bytes)?;
    let mut journal = TransactionJournalV1 {
        version: JOURNAL_VERSION,
        transaction: random_transaction_id()?,
        state: TransactionState::Prepared,
        config_path_id: config_path_id(config_path)?,
        base_revision: loaded.revision.clone(),
        candidate_revision: candidate_revision.clone(),
        baseline_runtime_instance,
    };
    write_journal(&state_directory, &journal)?;
    replace_config(config_path, &candidate.bytes)?;
    let installed = load_config(config_path)?;
    if installed.revision != candidate_revision {
        return Err(ManagementError::new(
            ManagementErrorCode::TransactionConflict,
            anyhow::anyhow!("installed configuration revision changed unexpectedly"),
        ));
    }
    journal.state = TransactionState::PendingValidation;
    write_journal(&state_directory, &journal)?;
    Ok(ConfigSetV1 {
        schema_version: RESULT_VERSION,
        transaction: Some(journal.transaction),
        base_revision: loaded.revision,
        candidate_revision,
        changed: true,
        restart_required: true,
    })
}

pub fn commit(
    config_path: &Path,
    transaction: &str,
    expected_instance: &str,
) -> ManagementResult<ConfigCommitV1> {
    validate_transaction_id(transaction)?;
    validate_transaction_id(expected_instance)?;
    let bootstrap = load_config(config_path)?;
    let state_directory = bootstrap.config.state_path.clone();
    let (_lifecycle, _config_lock) = acquire_management_locks(&state_directory)?;
    let mut loaded = load_config(config_path)?;
    ensure_bootstrap_paths_unchanged(&bootstrap, &loaded)?;
    let recovered = recover(config_path, &mut loaded)?;
    if let Some(RecoveryCompletion::Committed(journal)) = recovered.completion {
        require_transaction(&journal, transaction)?;
        return Ok(ConfigCommitV1 {
            schema_version: RESULT_VERSION,
            operation: "config_commit",
            transaction: journal.transaction,
            revision: journal.candidate_revision,
        });
    }
    let mut journal = recovered.journal.ok_or_else(|| {
        ManagementError::new(
            ManagementErrorCode::TransactionNotFound,
            anyhow::anyhow!("no configuration transaction is pending"),
        )
    })?;
    require_transaction(&journal, transaction)?;
    if journal.state != TransactionState::PendingValidation
        || loaded.revision != journal.candidate_revision
    {
        return Err(transaction_conflict("candidate is not pending validation"));
    }
    if journal.baseline_runtime_instance.as_deref() == Some(expected_instance) {
        return Err(ManagementError::new(
            ManagementErrorCode::HealthNotProven,
            anyhow::anyhow!("candidate validation requires a new daemon instance"),
        ));
    }
    let secret = identity::load(&loaded.config.identity_path)
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error))?;
    appliance::validate_candidate_runtime(
        secret.public(),
        &journal.candidate_revision,
        expected_instance,
    )
    .map_err(|error| ManagementError::new(ManagementErrorCode::HealthNotProven, error))?;

    journal.state = TransactionState::Committing;
    write_journal(&state_directory, &journal)?;
    cleanup_transaction(&state_directory)?;
    Ok(ConfigCommitV1 {
        schema_version: RESULT_VERSION,
        operation: "config_commit",
        transaction: journal.transaction,
        revision: journal.candidate_revision,
    })
}

pub fn rollback(config_path: &Path, transaction: &str) -> ManagementResult<ConfigRollbackV1> {
    validate_transaction_id(transaction)?;
    let bootstrap = load_config(config_path)?;
    let state_directory = bootstrap.config.state_path.clone();
    let (_lifecycle, _config_lock) = acquire_management_locks(&state_directory)?;
    let mut loaded = load_config(config_path)?;
    ensure_bootstrap_paths_unchanged(&bootstrap, &loaded)?;
    let recovered = recover(config_path, &mut loaded)?;
    if let Some(RecoveryCompletion::RolledBack(journal)) = recovered.completion {
        require_transaction(&journal, transaction)?;
        return Ok(ConfigRollbackV1 {
            schema_version: RESULT_VERSION,
            operation: "config_rollback",
            transaction: journal.transaction,
            restored_revision: journal.base_revision,
        });
    }
    let mut journal = recovered.journal.ok_or_else(|| {
        ManagementError::new(
            ManagementErrorCode::TransactionNotFound,
            anyhow::anyhow!("no configuration transaction is pending"),
        )
    })?;
    require_transaction(&journal, transaction)?;
    if journal.state != TransactionState::PendingValidation
        || loaded.revision != journal.candidate_revision
    {
        return Err(transaction_conflict("candidate cannot be rolled back"));
    }
    journal.state = TransactionState::RollingBack;
    write_journal(&state_directory, &journal)?;
    let base = read_artifact(&state_directory, BASE_FILE, &journal.base_revision)?;
    replace_config(config_path, &base)?;
    loaded = load_config(config_path)?;
    if loaded.revision != journal.base_revision {
        return Err(transaction_conflict("base configuration was not restored"));
    }
    cleanup_transaction(&state_directory)?;
    Ok(ConfigRollbackV1 {
        schema_version: RESULT_VERSION,
        operation: "config_rollback",
        transaction: journal.transaction,
        restored_revision: journal.base_revision,
    })
}

pub fn inspect_transaction(
    loaded: &LoadedHostConfig,
    config_path: &Path,
) -> ManagementResult<Option<ConfigTransactionSummaryV1>> {
    let state_directory = &loaded.config.state_path;
    match fs::symlink_metadata(state_directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ManagementError::new(
                ManagementErrorCode::UnsafeStorage,
                error,
            ));
        }
        Ok(_) => {}
    }
    let Some(journal) = read_journal(state_directory)? else {
        return Ok(None);
    };
    validate_journal(&journal, config_path)?;
    if loaded.revision != journal.base_revision && loaded.revision != journal.candidate_revision {
        return Err(transaction_conflict(
            "live configuration conflicts with its transaction",
        ));
    }
    Ok(Some(ConfigTransactionSummaryV1 {
        transaction: journal.transaction,
        state: journal.state,
        base_revision: journal.base_revision,
        candidate_revision: journal.candidate_revision,
    }))
}

fn recover(config_path: &Path, loaded: &mut LoadedHostConfig) -> ManagementResult<RecoveryOutcome> {
    let state_directory = loaded.config.state_path.clone();
    let Some(mut journal) = read_journal(&state_directory)? else {
        remove_artifact_if_present(&state_directory, BASE_FILE)?;
        remove_artifact_if_present(&state_directory, CANDIDATE_FILE)?;
        return Ok(RecoveryOutcome {
            journal: None,
            completion: None,
        });
    };
    validate_journal(&journal, config_path)?;
    match journal.state {
        TransactionState::Prepared => {
            let _base = read_artifact(&state_directory, BASE_FILE, &journal.base_revision)?;
            let candidate = read_artifact(
                &state_directory,
                CANDIDATE_FILE,
                &journal.candidate_revision,
            )?;
            if loaded.revision == journal.base_revision {
                replace_config(config_path, &candidate)?;
                *loaded = load_config(config_path)?;
            }
            if loaded.revision != journal.candidate_revision {
                return Err(transaction_conflict(
                    "prepared transaction has conflicting live bytes",
                ));
            }
            journal.state = TransactionState::PendingValidation;
            write_journal(&state_directory, &journal)?;
            Ok(RecoveryOutcome {
                journal: Some(journal),
                completion: None,
            })
        }
        TransactionState::PendingValidation => {
            let _base = read_artifact(&state_directory, BASE_FILE, &journal.base_revision)?;
            let _candidate = read_artifact(
                &state_directory,
                CANDIDATE_FILE,
                &journal.candidate_revision,
            )?;
            if loaded.revision != journal.candidate_revision {
                return Err(transaction_conflict(
                    "pending transaction has conflicting live bytes",
                ));
            }
            Ok(RecoveryOutcome {
                journal: Some(journal),
                completion: None,
            })
        }
        TransactionState::Committing => {
            if loaded.revision != journal.candidate_revision {
                return Err(transaction_conflict(
                    "committing transaction has conflicting live bytes",
                ));
            }
            cleanup_transaction(&state_directory)?;
            Ok(RecoveryOutcome {
                journal: None,
                completion: Some(RecoveryCompletion::Committed(journal)),
            })
        }
        TransactionState::RollingBack => {
            if loaded.revision == journal.candidate_revision {
                let base = read_artifact(&state_directory, BASE_FILE, &journal.base_revision)?;
                replace_config(config_path, &base)?;
                *loaded = load_config(config_path)?;
            }
            if loaded.revision != journal.base_revision {
                return Err(transaction_conflict(
                    "rolling-back transaction has conflicting live bytes",
                ));
            }
            cleanup_transaction(&state_directory)?;
            Ok(RecoveryOutcome {
                journal: None,
                completion: Some(RecoveryCompletion::RolledBack(journal)),
            })
        }
    }
}

fn load_config(path: &Path) -> ManagementResult<LoadedHostConfig> {
    HostConfig::load_document(path)
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error))
}

pub fn prepare_service(config_path: &Path) -> ManagementResult<(LoadedHostConfig, LifecycleGuard)> {
    let bootstrap = load_config(config_path)?;
    let state_directory = bootstrap.config.state_path.clone();
    bootstrap
        .config
        .ensure_runtime_directory()
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error))?;
    let lifecycle = match LifecycleGuard::try_acquire(&state_directory, true) {
        Ok(lifecycle) => lifecycle,
        Err(error) => return Err(classify_lifecycle_error(&state_directory, error)),
    };
    finish_service_prepare(config_path, &bootstrap, lifecycle)
}

fn finish_service_prepare(
    config_path: &Path,
    bootstrap: &LoadedHostConfig,
    lifecycle: LifecycleGuard,
) -> ManagementResult<(LoadedHostConfig, LifecycleGuard)> {
    let state_directory = bootstrap.config.state_path.clone();
    let _config_lock = classify_config_lock(secure_state::try_open_lifetime_lock(
        &state_directory,
        CONFIG_LOCK_FILE,
    ))?;
    let mut loaded = load_config(config_path)?;
    ensure_bootstrap_paths_unchanged(bootstrap, &loaded)?;
    recover(config_path, &mut loaded)?;
    loaded = load_config(config_path)?;
    ensure_bootstrap_paths_unchanged(bootstrap, &loaded)?;
    Ok((loaded, lifecycle))
}

fn acquire_management_locks(
    state_directory: &Path,
) -> ManagementResult<(LifecycleGuard, fs::File)> {
    let lifecycle = match LifecycleGuard::try_acquire(state_directory, true) {
        Ok(lifecycle) => lifecycle,
        Err(error) => return Err(classify_lifecycle_error(state_directory, error)),
    };
    let config_lock = classify_config_lock(secure_state::try_open_lifetime_lock(
        state_directory,
        CONFIG_LOCK_FILE,
    ))?;
    Ok((lifecycle, config_lock))
}

fn classify_lifecycle_error(
    _state_directory: &Path,
    error: secure_state::LockAcquireError,
) -> ManagementError {
    match error {
        secure_state::LockAcquireError::Unsafe(error) => {
            ManagementError::new(ManagementErrorCode::UnsafeStorage, error)
        }
        secure_state::LockAcquireError::Busy => ManagementError::new(
            ManagementErrorCode::LifecycleBusy,
            anyhow::anyhow!("Sigil lifecycle lock is held"),
        ),
    }
}

fn classify_config_lock(
    result: std::result::Result<fs::File, secure_state::LockAcquireError>,
) -> ManagementResult<fs::File> {
    result.map_err(|error| match error {
        secure_state::LockAcquireError::Busy => ManagementError::new(
            ManagementErrorCode::TransactionBusy,
            anyhow::anyhow!("another configuration transaction is active"),
        ),
        secure_state::LockAcquireError::Unsafe(error) => {
            ManagementError::new(ManagementErrorCode::UnsafeStorage, error)
        }
    })
}

fn require_expected_revision(
    loaded: &LoadedHostConfig,
    request: &ConfigRequestV1,
) -> ManagementResult<()> {
    ConfigRevision::parse(request.expected_revision.as_str())
        .map_err(|error| ManagementError::new(ManagementErrorCode::InvalidRequest, error))?;
    if loaded.revision != request.expected_revision {
        return Err(ManagementError::new(
            ManagementErrorCode::RevisionConflict,
            anyhow::anyhow!("configuration revision changed"),
        ));
    }
    Ok(())
}

fn build_candidate(
    loaded: &LoadedHostConfig,
    request: &ConfigRequestV1,
) -> ManagementResult<LoadedHostConfig> {
    let source = std::str::from_utf8(&loaded.bytes)
        .map_err(|error| ManagementError::new(ManagementErrorCode::ValidationFailed, error))?;
    let mut document = source
        .parse::<DocumentMut>()
        .map_err(|error| ManagementError::new(ManagementErrorCode::ValidationFailed, error))?;
    document["framerate"] = value(i64::from(request.settings.framerate));
    match request.settings.resolution {
        ManagedResolutionV1::Native => {
            document.remove("width");
            document.remove("height");
        }
        ManagedResolutionV1::Fixed { width, height } => {
            document["width"] = value(i64::from(width));
            document["height"] = value(i64::from(height));
        }
    }
    match (
        loaded.config.source.clone(),
        request.settings.rate_control.clone(),
    ) {
        (VideoSource::TestPattern, None) => {}
        (VideoSource::GamescopePipewire, Some(rate_control)) => {
            let table = document
                .get_mut("gamescope_pipewire")
                .and_then(toml_edit::Item::as_table_mut)
                .ok_or_else(|| {
                    ManagementError::new(
                        ManagementErrorCode::ValidationFailed,
                        anyhow::anyhow!("gamescope configuration table is missing"),
                    )
                })?;
            match rate_control {
                ManagedRateControlV1::Cbr { bitrate_kbps } => {
                    table["rate_control"] = value("cbr");
                    table["bitrate_kbps"] = value(i64::from(bitrate_kbps));
                    table.remove("quantizer");
                }
                ManagedRateControlV1::Cqp { quantizer } => {
                    table["rate_control"] = value("cqp");
                    table["quantizer"] = value(i64::from(quantizer));
                    table.remove("bitrate_kbps");
                }
            }
        }
        _ => {
            return Err(ManagementError::new(
                ManagementErrorCode::ValidationFailed,
                anyhow::anyhow!("rate-control settings do not match the configured source"),
            ));
        }
    }
    let bytes = document.to_string().into_bytes();
    let config = HostConfig::parse(&bytes)
        .map_err(|error| ManagementError::new(ManagementErrorCode::ValidationFailed, error))?;
    if config.identity_path != loaded.config.identity_path
        || config.state_path != loaded.config.state_path
    {
        return Err(ManagementError::new(
            ManagementErrorCode::ValidationFailed,
            anyhow::anyhow!("managed configuration cannot change identity or state paths"),
        ));
    }
    Ok(LoadedHostConfig {
        revision: ConfigRevision::from_bytes(&bytes),
        bytes,
        config,
    })
}

fn project_settings(config: &HostConfig) -> ManagementResult<ManagedSettingsV1> {
    let resolution = match config.configured_dimensions() {
        Some((width, height)) => ManagedResolutionV1::Fixed { width, height },
        None => ManagedResolutionV1::Native,
    };
    let rate_control = match (&config.source, &config.gamescope_pipewire) {
        (VideoSource::TestPattern, None) => None,
        (VideoSource::GamescopePipewire, Some(gamescope)) => Some(match gamescope.rate_control {
            VaapiRateControl::Cbr => ManagedRateControlV1::Cbr {
                bitrate_kbps: gamescope
                    .bitrate_kbps
                    .expect("validated CBR bitrate is present"),
            },
            VaapiRateControl::Cqp => ManagedRateControlV1::Cqp {
                quantizer: gamescope
                    .quantizer
                    .expect("validated CQP quantizer is present"),
            },
        }),
        _ => {
            return Err(ManagementError::new(
                ManagementErrorCode::ValidationFailed,
                anyhow::anyhow!("validated source configuration is inconsistent"),
            ));
        }
    };
    Ok(ManagedSettingsV1 {
        resolution,
        framerate: config.framerate,
        rate_control,
    })
}

fn ensure_bootstrap_paths_unchanged(
    bootstrap: &LoadedHostConfig,
    current: &LoadedHostConfig,
) -> ManagementResult<()> {
    if bootstrap.config.state_path != current.config.state_path
        || bootstrap.config.identity_path != current.config.identity_path
    {
        return Err(transaction_conflict(
            "configuration identity or state path changed while acquiring locks",
        ));
    }
    Ok(())
}

fn replace_config(config_path: &Path, bytes: &[u8]) -> ManagementResult<()> {
    let parent = config_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file_name = config_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ManagementError::new(
                ManagementErrorCode::UnsafeStorage,
                anyhow::anyhow!("config filename is invalid"),
            )
        })?;
    secure_state::atomic_write_exact(parent, file_name, bytes, MAX_CONFIG_BYTES)
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error))
}

fn write_artifact(state_directory: &Path, file_name: &str, bytes: &[u8]) -> ManagementResult<()> {
    secure_state::atomic_write_exact(state_directory, file_name, bytes, MAX_CONFIG_BYTES)
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error))
}

fn read_artifact(
    state_directory: &Path,
    file_name: &str,
    expected_revision: &ConfigRevision,
) -> ManagementResult<Vec<u8>> {
    let bytes = secure_state::read_bounded(state_directory, file_name, MAX_CONFIG_BYTES)
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error))?
        .ok_or_else(|| transaction_conflict("configuration recovery artifact is missing"))?;
    if ConfigRevision::from_bytes(&bytes) != *expected_revision {
        return Err(transaction_conflict(
            "configuration recovery artifact revision does not match its journal",
        ));
    }
    Ok(bytes)
}

fn write_journal(state_directory: &Path, journal: &TransactionJournalV1) -> ManagementResult<()> {
    let bytes = serde_json::to_vec(journal)
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error))?;
    secure_state::atomic_write(state_directory, JOURNAL_FILE, &bytes, MAX_JOURNAL_BYTES)
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error))
}

fn read_journal(state_directory: &Path) -> ManagementResult<Option<TransactionJournalV1>> {
    let Some(bytes) = secure_state::read_bounded(state_directory, JOURNAL_FILE, MAX_JOURNAL_BYTES)
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error))?
    else {
        return Ok(None);
    };
    let journal: TransactionJournalV1 = serde_json::from_slice(&bytes)
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error))?;
    Ok(Some(journal))
}

fn validate_journal(journal: &TransactionJournalV1, config_path: &Path) -> ManagementResult<()> {
    if journal.version != JOURNAL_VERSION {
        return Err(ManagementError::new(
            ManagementErrorCode::UnsupportedSchema,
            anyhow::anyhow!("unsupported configuration transaction journal"),
        ));
    }
    validate_stored_id(&journal.transaction)?;
    ConfigRevision::parse(journal.base_revision.as_str())
        .and_then(|_| ConfigRevision::parse(journal.candidate_revision.as_str()))
        .and_then(|_| ConfigRevision::parse(journal.config_path_id.as_str()))
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error))?;
    if journal.config_path_id != config_path_id(config_path)? {
        return Err(transaction_conflict(
            "configuration transaction belongs to a different config file",
        ));
    }
    if journal.base_revision == journal.candidate_revision {
        return Err(ManagementError::new(
            ManagementErrorCode::UnsafeStorage,
            anyhow::anyhow!("transaction revisions must differ"),
        ));
    }
    if let Some(instance) = &journal.baseline_runtime_instance {
        validate_stored_id(instance)?;
    }
    Ok(())
}

fn config_path_id(path: &Path) -> ManagementResult<ConfigRevision> {
    let canonical = path
        .canonicalize()
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error))?;
    let value = canonical.to_str().ok_or_else(|| {
        ManagementError::new(
            ManagementErrorCode::UnsafeStorage,
            anyhow::anyhow!("configuration path is not valid UTF-8"),
        )
    })?;
    Ok(ConfigRevision::from_bytes(value.as_bytes()))
}

fn require_transaction(journal: &TransactionJournalV1, expected: &str) -> ManagementResult<()> {
    if journal.transaction != expected {
        return Err(transaction_conflict(
            "configuration transaction ID does not match",
        ));
    }
    Ok(())
}

fn validate_transaction_id(value: &str) -> ManagementResult<()> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ManagementError::new(
            ManagementErrorCode::InvalidRequest,
            anyhow::anyhow!("transaction identifier is invalid"),
        ));
    }
    Ok(())
}

fn validate_stored_id(value: &str) -> ManagementResult<()> {
    validate_transaction_id(value)
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error.source))
}

fn random_transaction_id() -> ManagementResult<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error))?;
    let mut value = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut value, "{byte:02x}").expect("writing into a String cannot fail");
    }
    Ok(value)
}

fn cleanup_transaction(state_directory: &Path) -> ManagementResult<()> {
    remove_artifact_if_present(state_directory, BASE_FILE)?;
    remove_artifact_if_present(state_directory, CANDIDATE_FILE)?;
    remove_artifact_if_present(state_directory, JOURNAL_FILE)?;
    Ok(())
}

fn remove_artifact_if_present(state_directory: &Path, file_name: &str) -> ManagementResult<()> {
    secure_state::remove_file_if_exists(state_directory, file_name)
        .map(|_| ())
        .map_err(|error| ManagementError::new(ManagementErrorCode::UnsafeStorage, error))
}

fn transaction_conflict(message: &'static str) -> ManagementError {
    ManagementError::new(
        ManagementErrorCode::TransactionConflict,
        anyhow::anyhow!(message),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _directory: tempfile::TempDir,
        config_path: std::path::PathBuf,
        state_directory: std::path::PathBuf,
        base: LoadedHostConfig,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
            }
            let state_directory = directory.path().join("state");
            secure_state::ensure_private_directory(&state_directory).unwrap();
            let config_path = directory.path().join("host.toml");
            let identity_path = directory.path().join("host.key");
            let bytes = format!(
                "# retained operator comment\nidentity_path = {:?}\nstate_path = {:?}\nsource = \"test-pattern\"\nwidth = 1280\nheight = 800\nframerate = 60\ncodec = \"h264\"\n",
                identity_path, state_directory
            )
            .into_bytes();
            fs::write(&config_path, bytes).unwrap();
            let base = HostConfig::load_document(&config_path).unwrap();
            Self {
                _directory: directory,
                config_path,
                state_directory,
                base,
            }
        }

        fn request(&self, framerate: u32) -> ConfigRequestV1 {
            ConfigRequestV1 {
                _schema_version: REQUEST_VERSION,
                expected_revision: self.base.revision.clone(),
                settings: ManagedSettingsV1 {
                    resolution: ManagedResolutionV1::Native,
                    framerate,
                    rate_control: None,
                },
            }
        }

        fn prepared(&self, state: TransactionState) -> (TransactionJournalV1, LoadedHostConfig) {
            let candidate = build_candidate(&self.base, &self.request(72)).unwrap();
            write_artifact(&self.state_directory, BASE_FILE, &self.base.bytes).unwrap();
            write_artifact(&self.state_directory, CANDIDATE_FILE, &candidate.bytes).unwrap();
            let journal = TransactionJournalV1 {
                version: JOURNAL_VERSION,
                transaction: "0123456789abcdef0123456789abcdef".to_owned(),
                state,
                config_path_id: config_path_id(&self.config_path).unwrap(),
                base_revision: self.base.revision.clone(),
                candidate_revision: candidate.revision.clone(),
                baseline_runtime_instance: None,
            };
            write_journal(&self.state_directory, &journal).unwrap();
            (journal, candidate)
        }
    }

    #[test]
    fn bounded_request_rejects_unknown_fields_and_oversize_input() {
        let unknown = br#"{"schema_version":1,"expected_revision":"sha256:0000000000000000000000000000000000000000000000000000000000000000","settings":{"resolution":{"mode":"native"},"framerate":60,"rate_control":null},"unexpected":true}"#;
        assert_eq!(
            read_request(&unknown[..]).unwrap_err().code,
            ManagementErrorCode::InvalidRequest
        );
        assert_eq!(
            read_request(&vec![b' '; MAX_REQUEST_BYTES as usize + 1][..])
                .unwrap_err()
                .code,
            ManagementErrorCode::InvalidRequest
        );
        let future = br#"{"schema_version":2,"future_shape":{"anything":true}}"#;
        assert_eq!(
            read_request(&future[..]).unwrap_err().code,
            ManagementErrorCode::UnsupportedSchema
        );
        let duplicate = br#"{"schema_version":1,"schema_version":2,"expected_revision":"sha256:0000000000000000000000000000000000000000000000000000000000000000","settings":{"resolution":{"mode":"native"},"framerate":60,"rate_control":null}}"#;
        assert_eq!(
            read_request(&duplicate[..]).unwrap_err().code,
            ManagementErrorCode::InvalidRequest
        );
        let duplicate_expected = br#"{"schema_version":1,"expected_revision":"sha256:0000000000000000000000000000000000000000000000000000000000000000","expected_revision":"sha256:1111111111111111111111111111111111111111111111111111111111111111","settings":{"resolution":{"mode":"native"},"framerate":60,"rate_control":null}}"#;
        assert_eq!(
            read_request(&duplicate_expected[..]).unwrap_err().code,
            ManagementErrorCode::InvalidRequest
        );
    }

    #[test]
    fn candidate_preserves_comments_and_immutable_paths() {
        let fixture = Fixture::new();
        let candidate = build_candidate(&fixture.base, &fixture.request(72)).unwrap();
        let text = std::str::from_utf8(&candidate.bytes).unwrap();
        assert!(text.contains("# retained operator comment"));
        assert!(!text.contains("width ="));
        assert!(!text.contains("height ="));
        assert!(text.contains("framerate = 72"));
        assert_eq!(
            candidate.config.identity_path,
            fixture.base.config.identity_path
        );
        assert_eq!(candidate.config.state_path, fixture.base.config.state_path);
    }

    #[test]
    fn prepared_recovery_forward_applies_the_exact_candidate() {
        let fixture = Fixture::new();
        let (journal, candidate) = fixture.prepared(TransactionState::Prepared);
        let mut loaded = fixture.base.clone();
        let outcome = recover(&fixture.config_path, &mut loaded).unwrap();
        assert_eq!(loaded.bytes, candidate.bytes);
        assert_eq!(loaded.revision, journal.candidate_revision);
        assert_eq!(
            outcome.journal.unwrap().state,
            TransactionState::PendingValidation
        );
    }

    #[test]
    fn configured_service_recovers_before_using_its_definitive_reload() {
        let fixture = Fixture::new();
        let (_journal, candidate) = fixture.prepared(TransactionState::Prepared);
        let runtime = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let lifecycle =
            LifecycleGuard::try_acquire_at(&fixture.state_directory, Some(runtime.path())).unwrap();
        let (loaded, _lifecycle) =
            finish_service_prepare(&fixture.config_path, &fixture.base, lifecycle).unwrap();
        assert_eq!(loaded.revision, candidate.revision);
        assert_eq!(loaded.bytes, candidate.bytes);
        assert_eq!(
            read_journal(&fixture.state_directory)
                .unwrap()
                .unwrap()
                .state,
            TransactionState::PendingValidation
        );
    }

    #[test]
    fn pending_recovery_rejects_untracked_live_bytes() {
        let fixture = Fixture::new();
        fixture.prepared(TransactionState::PendingValidation);
        let conflicting = fixture
            .base
            .bytes
            .replace_range_owned(b"framerate = 60", b"framerate = 90");
        replace_config(&fixture.config_path, &conflicting).unwrap();
        let mut loaded = load_config(&fixture.config_path).unwrap();
        let Err(error) = recover(&fixture.config_path, &mut loaded) else {
            panic!("untracked live bytes must fail closed");
        };
        assert_eq!(error.code, ManagementErrorCode::TransactionConflict);
    }

    #[test]
    fn rolling_back_recovery_finishes_after_base_restore_and_partial_cleanup() {
        let fixture = Fixture::new();
        let (journal, _candidate) = fixture.prepared(TransactionState::RollingBack);
        replace_config(&fixture.config_path, &fixture.base.bytes).unwrap();
        secure_state::remove_file_if_exists(&fixture.state_directory, BASE_FILE).unwrap();
        let mut loaded = load_config(&fixture.config_path).unwrap();
        let outcome = recover(&fixture.config_path, &mut loaded).unwrap();
        assert_eq!(loaded.revision, fixture.base.revision);
        assert!(matches!(
            outcome.completion,
            Some(RecoveryCompletion::RolledBack(completed))
                if completed.transaction == journal.transaction
        ));
        assert!(read_journal(&fixture.state_directory).unwrap().is_none());
        assert!(
            secure_state::read_bounded(&fixture.state_directory, CANDIDATE_FILE, MAX_CONFIG_BYTES)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn committing_recovery_finishes_cleanup_without_artifacts() {
        let fixture = Fixture::new();
        let (journal, candidate) = fixture.prepared(TransactionState::Committing);
        replace_config(&fixture.config_path, &candidate.bytes).unwrap();
        secure_state::remove_file_if_exists(&fixture.state_directory, BASE_FILE).unwrap();
        secure_state::remove_file_if_exists(&fixture.state_directory, CANDIDATE_FILE).unwrap();
        let mut loaded = load_config(&fixture.config_path).unwrap();
        let outcome = recover(&fixture.config_path, &mut loaded).unwrap();
        assert_eq!(loaded.revision, candidate.revision);
        assert!(matches!(
            outcome.completion,
            Some(RecoveryCompletion::Committed(completed))
                if completed.transaction == journal.transaction
        ));
        assert!(read_journal(&fixture.state_directory).unwrap().is_none());
    }

    #[test]
    fn malformed_journal_identifier_is_unsafe_storage_not_caller_input() {
        let fixture = Fixture::new();
        let (mut journal, _candidate) = fixture.prepared(TransactionState::Prepared);
        journal.transaction = "NOT-A-TRANSACTION-ID".to_owned();
        write_journal(&fixture.state_directory, &journal).unwrap();
        let mut loaded = fixture.base.clone();
        let Err(error) = recover(&fixture.config_path, &mut loaded) else {
            panic!("malformed protected journal must fail closed");
        };
        assert_eq!(error.code, ManagementErrorCode::UnsafeStorage);
    }

    #[test]
    fn lock_failures_do_not_probe_or_invert_lock_order() {
        let fixture = Fixture::new();
        assert_eq!(
            classify_lifecycle_error(
                &fixture.state_directory,
                secure_state::LockAcquireError::Busy,
            )
            .code,
            ManagementErrorCode::LifecycleBusy
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&fixture.state_directory, fs::Permissions::from_mode(0o755))
                .unwrap();
            assert_eq!(
                classify_config_lock(secure_state::try_open_lifetime_lock(
                    &fixture.state_directory,
                    CONFIG_LOCK_FILE,
                ))
                .unwrap_err()
                .code,
                ManagementErrorCode::UnsafeStorage
            );
        }
    }

    trait ReplaceRangeOwned {
        fn replace_range_owned(&self, from: &[u8], to: &[u8]) -> Vec<u8>;
    }

    impl ReplaceRangeOwned for Vec<u8> {
        fn replace_range_owned(&self, from: &[u8], to: &[u8]) -> Vec<u8> {
            let offset = self
                .windows(from.len())
                .position(|window| window == from)
                .unwrap();
            let mut result = self.clone();
            result.splice(offset..offset + from.len(), to.iter().copied());
            result
        }
    }
}
