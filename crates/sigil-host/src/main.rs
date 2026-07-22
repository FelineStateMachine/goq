mod appliance;
mod audio;
mod authorization;
mod clock;
mod config;
mod config_management;
mod cursor;
mod identity;
mod input;
mod moq_catalog;
mod secure_state;
mod server;
mod source;

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use iroh::endpoint::{QuicTransportConfig, presets};
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointId};
use moq_net::Origin;
use sigil_protocol::{
    AUDIO_ALPN_V1, CONTROL_ALPN_V1, INPUT_ALPN_V1, InvitationGrants, MAX_INVITATION_TTL_SECS,
    MEDIA_ALPN_V1, MEDIA_ALPN_V2, MEDIA_ALPN_V3, MEDIA_FEEDBACK_ALPN_V1, SignedInvitation,
};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::authorization::{
    AuthorizationPolicy, AuthorizationStore, grant_names, unix_timestamp_now,
};
use crate::config::{ConfigRevision, HostConfig, InputMode, VideoSource};
use crate::cursor::PointerPositionTracker;
use crate::input::InputBackend;
use crate::server::{
    AudioHandler, AuthorizedMoqHandler, ControlHandler, InputHandler, MediaFeedbackHandler,
    MediaHandler, MediaV2Handler, MediaV3Handler, SessionRegistry,
};

const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECTION_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Parser)]
#[command(name = "sigil", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect or manage the local Sigil appliance service.
    Appliance {
        #[command(subcommand)]
        command: ApplianceCommand,
    },
    /// Create or inspect the persistent Iroh host identity.
    Identity {
        #[command(subcommand)]
        command: IdentityCommand,
    },
    /// Validate host configuration without starting network services.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Run a bounded capture probe.
    Capture {
        #[command(subcommand)]
        command: CaptureCommand,
    },
    /// Create a short-lived, peer-bound Portal enrollment invitation.
    Invitation {
        #[command(subcommand)]
        command: InvitationCommand,
    },
    /// Inspect or revoke the one enrolled Portal peer.
    Enrollment {
        #[command(subcommand)]
        command: EnrollmentCommand,
    },
    /// Run the headless host daemon in the foreground.
    Serve(ServeArgs),
}

#[derive(Debug, Subcommand)]
enum ApplianceCommand {
    /// Print a versioned, redacted daemon and enrollment snapshot.
    Status {
        #[arg(long)]
        config: PathBuf,
        /// Emit the stable machine-readable schema.
        #[arg(long, required = true)]
        json: bool,
        /// Public status schema. Version 1 remains the compatibility default.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=2))]
        schema_version: u16,
    },
    /// Revoke the enrolled Portal and invalidate every outstanding invitation.
    EnrollmentReset {
        #[arg(long)]
        config: PathBuf,
        /// Redacted host fingerprint from `appliance status`.
        #[arg(long)]
        expected_host_fingerprint: String,
        /// Emit the stable machine-readable schema.
        #[arg(long, required = true)]
        json: bool,
    },
    /// Inspect or transactionally update the validated host configuration.
    Config {
        #[command(subcommand)]
        command: ApplianceConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ApplianceConfigCommand {
    /// Show only the controller-editable configuration projection.
    Show {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, required = true)]
        json: bool,
    },
    /// Validate a bounded JSON request from stdin without writing.
    Validate {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, required = true)]
        json: bool,
    },
    /// Stage and atomically install a validated candidate while Sigil is stopped.
    Set {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, required = true)]
        json: bool,
    },
    /// Commit a candidate proven by one exact stopped daemon instance.
    Commit {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        transaction: String,
        #[arg(long)]
        expected_instance: String,
        #[arg(long, required = true)]
        json: bool,
    },
    /// Restore the byte-exact prior configuration while Sigil is stopped.
    Rollback {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        transaction: String,
        #[arg(long, required = true)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum IdentityCommand {
    /// Create a new identity using create-new semantics.
    Init {
        #[arg(long)]
        output: PathBuf,
    },
    /// Print the public node ID for an existing identity.
    Show {
        #[arg(long)]
        identity: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Parse, validate, and check the referenced identity.
    Check {
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum CaptureCommand {
    /// Consume a finite number of frames without starting iroh.
    Probe(CaptureProbeArgs),
}

#[derive(Debug, Subcommand)]
enum InvitationCommand {
    /// Create a signed invitation file using create-new semantics.
    Create {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        peer: EndpointId,
        #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..=MAX_INVITATION_TTL_SECS))]
        expires_in_seconds: u64,
        #[arg(long)]
        pointer_keyboard: bool,
        #[arg(long)]
        gamepad: bool,
        #[arg(long)]
        output: PathBuf,
        /// Also print the short-lived invitation as a goq:// deep link.
        #[arg(long)]
        print_deep_link: bool,
    },
}

#[derive(Debug, Subcommand)]
enum EnrollmentCommand {
    /// Print the current enrollment without exposing secret material.
    Show {
        #[arg(long)]
        config: PathBuf,
    },
    /// Remove the enrolled peer and invalidate every outstanding invitation.
    Revoke {
        #[arg(long)]
        config: PathBuf,
        /// Redacted host fingerprint from `appliance status`.
        #[arg(long)]
        expected_host_fingerprint: String,
        /// Emit the stable machine-readable schema.
        #[arg(long, required = true)]
        json: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SourceArg {
    TestPattern,
    GamescopePipewire,
}

impl From<SourceArg> for VideoSource {
    fn from(value: SourceArg) -> Self {
        match value {
            SourceArg::TestPattern => Self::TestPattern,
            SourceArg::GamescopePipewire => Self::GamescopePipewire,
        }
    }
}

#[derive(Debug, Args)]
struct CaptureProbeArgs {
    #[arg(long, value_enum)]
    source: SourceArg,
    #[arg(long, default_value_t = 300)]
    frames: u32,
    #[arg(long, value_parser = parse_size)]
    expect_size: Option<(u32, u32)>,
    /// Fail unless the bounded proof sustains at least this encoded-frame rate.
    #[arg(long)]
    minimum_fps: Option<f64>,
    /// Strict host configuration; required for gamescope-pipewire.
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("serve-source")
        .required(true)
        .args(["config", "identity"])
))]
struct ServeArgs {
    /// Strict TOML host configuration.
    #[arg(long, conflicts_with_all = ["identity", "source", "state_path"])]
    config: Option<PathBuf>,
    /// Host identity for direct proof-mode configuration.
    #[arg(long, requires = "source")]
    identity: Option<PathBuf>,
    /// Video source for direct proof-mode configuration.
    #[arg(long, value_enum, requires = "identity")]
    source: Option<SourceArg>,
    /// Writable state directory for direct proof mode.
    #[arg(long)]
    state_path: Option<PathBuf>,
    #[arg(long, default_value_t = 1280)]
    width: u32,
    #[arg(long, default_value_t = 800)]
    height: u32,
    #[arg(long, default_value_t = 60)]
    framerate: u32,
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: PathBuf,
    /// Exit after this many seconds; intended for bounded automation.
    #[arg(long)]
    max_runtime_seconds: Option<u64>,
}

enum CliFailure {
    General(anyhow::Error),
    Management(config_management::ManagementError),
}

impl From<anyhow::Error> for CliFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::General(error)
    }
}

type CliResult<T> = std::result::Result<T, CliFailure>;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    server::install_panic_hook();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliFailure::Management(error)) => {
            let response = serde_json::to_string(&error.response())
                .expect("serializing a fixed management error cannot fail");
            eprintln!("{response}");
            ExitCode::FAILURE
        }
        Err(CliFailure::General(error)) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> CliResult<()> {
    match Cli::parse().command {
        Command::Appliance { command } => appliance_command(command),
        Command::Identity { command } => identity_command(command).map_err(Into::into),
        Command::Config { command } => config_command(command).await.map_err(Into::into),
        Command::Capture { command } => capture_command(command).await.map_err(Into::into),
        Command::Invitation { command } => invitation_command(command).map_err(Into::into),
        Command::Enrollment { command } => enrollment_command(command).map_err(Into::into),
        Command::Serve(args) => serve_command(args).await.map_err(Into::into),
    }
}

fn appliance_command(command: ApplianceCommand) -> CliResult<()> {
    match command {
        ApplianceCommand::Status {
            config,
            json: _,
            schema_version,
        } => {
            let status = appliance::collect_status(&config).map_err(CliFailure::from)?;
            println!("{}", appliance::status_json(&status, schema_version)?);
        }
        ApplianceCommand::EnrollmentReset {
            config,
            expected_host_fingerprint,
            json: _,
        } => {
            let result = appliance::reset_enrollment(&config, &expected_host_fingerprint)
                .map_err(CliFailure::from)?;
            println!(
                "{}",
                serde_json::to_string(&result).map_err(anyhow::Error::from)?
            );
        }
        ApplianceCommand::Config { command } => {
            let result = match command {
                ApplianceConfigCommand::Show { config, json: _ } => serde_json::to_value(
                    config_management::show(&config).map_err(CliFailure::Management)?,
                ),
                ApplianceConfigCommand::Validate { config, json: _ } => {
                    let request = config_management::read_request(std::io::stdin().lock())
                        .map_err(CliFailure::Management)?;
                    serde_json::to_value(
                        config_management::validate(&config, &request)
                            .map_err(CliFailure::Management)?,
                    )
                }
                ApplianceConfigCommand::Set { config, json: _ } => {
                    let request = config_management::read_request(std::io::stdin().lock())
                        .map_err(CliFailure::Management)?;
                    serde_json::to_value(
                        config_management::set(&config, &request)
                            .map_err(CliFailure::Management)?,
                    )
                }
                ApplianceConfigCommand::Commit {
                    config,
                    transaction,
                    expected_instance,
                    json: _,
                } => serde_json::to_value(
                    config_management::commit(&config, &transaction, &expected_instance)
                        .map_err(CliFailure::Management)?,
                ),
                ApplianceConfigCommand::Rollback {
                    config,
                    transaction,
                    json: _,
                } => serde_json::to_value(
                    config_management::rollback(&config, &transaction)
                        .map_err(CliFailure::Management)?,
                ),
            }
            .map_err(anyhow::Error::from)?;
            println!("{result}");
        }
    }
    Ok(())
}

fn authorization_store_from_config(
    path: &Path,
) -> Result<(HostConfig, iroh::SecretKey, AuthorizationStore)> {
    let config = HostConfig::load(path)?;
    config.ensure_runtime_directory()?;
    let secret = identity::load(&config.identity_path)?;
    let store = AuthorizationStore::open(config.state_path.clone(), secret.public())?;
    Ok((config, secret, store))
}

fn invitation_command(command: InvitationCommand) -> Result<()> {
    match command {
        InvitationCommand::Create {
            config,
            peer,
            expires_in_seconds,
            pointer_keyboard,
            gamepad,
            output,
            print_deep_link,
        } => {
            let (_config, secret, store) = authorization_store_from_config(&config)?;
            let mut grants = InvitationGrants::VIEW;
            if pointer_keyboard {
                grants = grants.union(InvitationGrants::POINTER_KEYBOARD);
            }
            if gamepad {
                grants = grants.union(InvitationGrants::GAMEPAD);
            }
            let mut nonce = [0_u8; 32];
            getrandom::fill(&mut nonce).context("generating invitation nonce")?;
            let now = unix_timestamp_now()?;
            let claims = store.issue_claims(peer, grants, expires_in_seconds, now, nonce)?;
            let expires_at = claims.expires_at_unix;
            let invitation = SignedInvitation::issue(claims, &secret.to_bytes())?;
            let token = invitation.encode();
            write_invitation_file(&output, &token)?;
            println!("invitation={}", output.display());
            println!("host_node_id={}", secret.public());
            println!("peer_node_id={peer}");
            println!("grants={}", grant_names(grants));
            println!("expires_at_unix={expires_at}");
            if print_deep_link {
                println!("deep_link=goq://invite/{token}");
            }
        }
    }
    Ok(())
}

fn enrollment_command(command: EnrollmentCommand) -> Result<()> {
    match command {
        EnrollmentCommand::Show { config } => {
            let (_config, _secret, store) = authorization_store_from_config(&config)?;
            let snapshot = store.snapshot()?;
            println!("enrollment_epoch={}", snapshot.epoch);
            match (snapshot.peer, snapshot.grants) {
                (Some(peer), Some(grants)) => {
                    println!("enrollment=active");
                    println!("peer_node_id={peer}");
                    println!("grants={}", grant_names(grants));
                }
                (None, None) => println!("enrollment=none"),
                _ => unreachable!("validated authorization state is internally consistent"),
            }
        }
        EnrollmentCommand::Revoke {
            config,
            expected_host_fingerprint,
            json: _,
        } => {
            let result = appliance::reset_enrollment(&config, &expected_host_fingerprint)?;
            println!("{}", serde_json::to_string(&result)?);
        }
    }
    Ok(())
}

fn write_invitation_file(path: &Path, token: &str) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options
        .open(path)
        .with_context(|| format!("creating invitation {}", path.display()))?;
    file.write_all(token.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn identity_command(command: IdentityCommand) -> Result<()> {
    match command {
        IdentityCommand::Init { output } => {
            let secret = identity::init(&output)?;
            println!("node_id={}", secret.public());
            println!("identity={}", identity::display_path(&output).display());
        }
        IdentityCommand::Show { identity: path } => {
            let secret = identity::load(&path)?;
            println!("node_id={}", secret.public());
        }
    }
    Ok(())
}

async fn config_command(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Check { config: path } => {
            let config = HostConfig::load(&path)?;
            let secret = identity::load(&config.identity_path)?;
            let _input_backend = InputBackend::initialize(&config)?;
            println!("config=ok");
            println!("node_id={}", secret.public());
            println!("source={:?}", config.source);
            println!("input={:?}", config.input_mode);
            if config.source == VideoSource::GamescopePipewire {
                let preflight = source::preflight_gamescope_pipewire(&config).await?;
                println!("pipewire_target_object={}", preflight.target_object);
                println!(
                    "pointer_surface={}x{}",
                    preflight.pointer_surface_dimensions.width,
                    preflight.pointer_surface_dimensions.height
                );
                println!(
                    "encoded_mode={}x{}@{}",
                    preflight.video_mode.width,
                    preflight.video_mode.height,
                    preflight.video_mode.framerate
                );
                println!("capture_preflight=ok");
                if config.audio.is_some() {
                    let target = audio::preflight_audio(&config).await?;
                    println!("audio_pipewire_target_object={target}");
                    println!("audio_capture_preflight=ok");
                }
            }
        }
    }
    Ok(())
}

async fn capture_command(command: CaptureCommand) -> Result<()> {
    match command {
        CaptureCommand::Probe(args) => {
            ensure!(
                (1..=36_000).contains(&args.frames),
                "--frames must be between 1 and 36000"
            );
            if let Some((width, height)) = args.expect_size {
                ensure!(width > 0 && height > 0, "expected size must be non-zero");
            }
            match args.source {
                SourceArg::TestPattern => probe_test_pattern(args).await,
                SourceArg::GamescopePipewire => probe_gamescope_pipewire(args).await,
            }
        }
    }
}

async fn probe_test_pattern(args: CaptureProbeArgs) -> Result<()> {
    ensure!(
        args.config.is_none(),
        "--config is only accepted for gamescope-pipewire capture"
    );
    let (width, height) = args.expect_size.unwrap_or((1280, 800));
    let config = HostConfig {
        identity_path: PathBuf::from("unused-by-capture-probe"),
        state_path: PathBuf::from("."),
        source: VideoSource::TestPattern,
        width: Some(width),
        height: Some(height),
        framerate: 60,
        codec: "h264".into(),
        input_mode: InputMode::Disabled,
        uinput: None,
        ffmpeg_path: PathBuf::from("ffmpeg"),
        gamescope_pipewire: None,
        audio: None,
    };
    config.validate()?;

    let configured_framerate = config.framerate;
    let source = source::spawn_test_pattern(config, clock::SessionClock::start());
    consume_capture_probe(
        args,
        source.frames,
        source.task,
        "test-pattern",
        configured_framerate,
    )
    .await
}

async fn probe_gamescope_pipewire(args: CaptureProbeArgs) -> Result<()> {
    let config_path = args
        .config
        .as_ref()
        .context("--config is required for gamescope-pipewire capture")?;
    let config = HostConfig::load(config_path)?;
    ensure!(
        config.source == VideoSource::GamescopePipewire,
        "capture config source must be gamescope-pipewire"
    );
    let preflight = source::preflight_gamescope_pipewire(&config).await?;
    let observed = (
        u32::from(preflight.video_mode.width),
        u32::from(preflight.video_mode.height),
    );
    if let Some(expected) = args.expect_size {
        ensure!(
            expected == observed,
            "expected size {}x{} does not match resolved encoded size {}x{}",
            expected.0,
            expected.1,
            observed.0,
            observed.1
        );
    }
    println!(
        "resolved_encoded_mode={}x{}@{}",
        observed.0, observed.1, preflight.video_mode.framerate
    );
    let configured_framerate = preflight.video_mode.framerate;
    let source = source::spawn_gamescope_pipewire(config, clock::SessionClock::start()).await?;
    consume_capture_probe(
        args,
        source.frames,
        source.task,
        "Gamescope PipeWire",
        configured_framerate,
    )
    .await
}

async fn consume_capture_probe(
    args: CaptureProbeArgs,
    mut receiver: tokio::sync::watch::Receiver<Option<source::EncodedFrame>>,
    task: tokio::task::JoinHandle<Result<()>>,
    source_name: &str,
    configured_framerate: u32,
) -> Result<()> {
    if let Some(minimum_fps) = args.minimum_fps {
        ensure!(
            minimum_fps.is_finite()
                && minimum_fps > 0.0
                && minimum_fps <= f64::from(configured_framerate),
            "--minimum-fps must be finite, positive, and no greater than configured framerate {configured_framerate}"
        );
    }
    let task = CaptureTaskGuard::new(task);
    let started = Instant::now();
    let mut received = 0_u32;
    let mut keyframes = 0_u32;
    let mut decodable_keyframes = 0_u32;
    let mut encoded_bytes = 0_u64;
    let mut last_sequence = None;
    let mut dropped = 0_u64;
    let mut max_post_encode_queue_age_micros = 0_u128;

    while received < args.frames {
        tokio::time::timeout(Duration::from_secs(5), receiver.changed())
            .await
            .with_context(|| format!("timed out waiting for encoded {source_name} frame"))?
            .with_context(|| format!("{source_name} encoder stopped"))?;
        let Some(frame) = receiver.borrow_and_update().clone() else {
            continue;
        };
        max_post_encode_queue_age_micros =
            max_post_encode_queue_age_micros.max(frame.observed_at.elapsed().as_micros());
        if let Some(previous) = last_sequence {
            dropped += frame.sequence.saturating_sub(previous + 1);
        }
        last_sequence = Some(frame.sequence);
        keyframes += u32::from(frame.keyframe);
        decodable_keyframes += u32::from(frame.keyframe && frame.codec_config);
        encoded_bytes = encoded_bytes.saturating_add(frame.data.len() as u64);
        received += 1;
    }

    let elapsed = started.elapsed();
    task.abort_and_wait().await;
    ensure!(keyframes > 0, "capture probe produced no H.264 keyframe");
    ensure!(
        decodable_keyframes > 0,
        "capture probe produced no keyframe with SPS/PPS codec configuration"
    );
    ensure!(
        dropped == 0,
        "capture probe dropped {dropped} frames after encode and before its consumer"
    );
    let observed_fps = f64::from(received) / elapsed.as_secs_f64();
    if let Some(minimum_fps) = args.minimum_fps {
        ensure!(
            observed_fps >= minimum_fps,
            "capture probe sustained only {observed_fps:.3} fps; required at least {minimum_fps:.3} fps"
        );
    }
    println!("probe=ok");
    println!("frames={received}");
    println!("keyframes={keyframes}");
    println!("decodable_keyframes={decodable_keyframes}");
    println!("dropped_after_encode_before_probe_consumer={dropped}");
    println!("observed_fps={observed_fps:.3}");
    println!("max_post_encode_queue_age_micros={max_post_encode_queue_age_micros}");
    println!("encoded_bytes={encoded_bytes}");
    println!("elapsed_ms={}", elapsed.as_millis());
    Ok(())
}

struct CaptureTaskGuard(Option<tokio::task::JoinHandle<Result<()>>>);

impl CaptureTaskGuard {
    fn new(task: tokio::task::JoinHandle<Result<()>>) -> Self {
        Self(Some(task))
    }

    async fn abort_and_wait(mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for CaptureTaskGuard {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

async fn serve_command(args: ServeArgs) -> Result<()> {
    let configured_service = args.config.is_some();
    let (mut config, loaded_config_revision, _lifecycle) = if let Some(config_path) = &args.config {
        let (loaded, lifecycle) =
            config_management::prepare_service(config_path).map_err(anyhow::Error::new)?;
        (loaded.config, Some(loaded.revision), lifecycle)
    } else {
        let loaded = load_serve_config(&args)?;
        loaded.config.validate()?;
        loaded.config.ensure_runtime_directory()?;
        let lifecycle = appliance::LifecycleGuard::acquire(&loaded.config.state_path, false)?;
        (loaded.config, loaded.revision, lifecycle)
    };
    let secret = identity::load(&config.identity_path)?;
    let sessions = Arc::new(SessionRegistry::default());
    let publisher = appliance::RuntimePublisher::start(
        secret.public(),
        Arc::clone(&sessions),
        configured_service,
        loaded_config_revision,
    )?;
    let authorization_result = if configured_service {
        AuthorizationStore::open(config.state_path.clone(), secret.public())
            .map(AuthorizationPolicy::Required)
    } else {
        Ok(AuthorizationPolicy::TestPatternProof)
    };
    let authorization = match authorization_result {
        Ok(authorization) => authorization,
        Err(error) => {
            if let Some(publisher) = publisher.as_ref() {
                let _ = publisher
                    .mark_degraded(appliance::RuntimeErrorCode::AuthorizationState)
                    .await;
            }
            return Err(error);
        }
    };

    let preflight_result: Result<(InputBackend, Option<PointerPositionTracker>)> = async {
        let pointer_surface_dimensions = if config.source == VideoSource::TestPattern {
            let (width, height) = config.test_pattern_dimensions()?;
            config.apply_resolved_dimensions(width, height)?;
            let status = tokio::process::Command::new(&config.ffmpeg_path)
                .arg("-version")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await
                .with_context(|| format!("starting {}", config.ffmpeg_path.display()))?;
            ensure!(
                status.success(),
                "ffmpeg version probe failed with {status}"
            );
            None
        } else {
            let preflight = source::preflight_gamescope_pipewire(&config).await?;
            info!(
                target_object = %preflight.target_object,
                pointer_surface_width = preflight.pointer_surface_dimensions.width,
                pointer_surface_height = preflight.pointer_surface_dimensions.height,
                encoded_width = preflight.video_mode.width,
                encoded_height = preflight.video_mode.height,
                encoded_framerate = preflight.video_mode.framerate,
                "Gamescope PipeWire capture preflight passed"
            );
            config.apply_resolved_dimensions(
                u32::from(preflight.video_mode.width),
                u32::from(preflight.video_mode.height),
            )?;
            if config.audio.is_some() {
                audio::preflight_audio_static(&config).await?;
                info!("PipeWire Opus audio static preflight passed");
            }
            Some(preflight.pointer_surface_dimensions)
        };
        let input_backend = InputBackend::initialize(&config)?;

        let pointer_positions = if config.source == VideoSource::GamescopePipewire
            && config.input_mode == InputMode::Uinput
        {
            let configured_display = config
                .gamescope_pipewire
                .as_ref()
                .and_then(|gamescope| gamescope.xwayland_display.as_deref());
            let pointer_surface_dimensions = pointer_surface_dimensions.context(
                "Gamescope capture preflight did not provide pointer surface dimensions",
            )?;
            match PointerPositionTracker::try_initialize(
                configured_display,
                pointer_surface_dimensions,
            ) {
                Ok(tracker) => {
                    info!("Gamescope Xwayland pointer feedback ready");
                    Some(tracker)
                }
                Err(error) => {
                    warn!(%error, "Gamescope Xwayland pointer feedback unavailable");
                    None
                }
            }
        } else {
            None
        };
        Ok((input_backend, pointer_positions))
    }
    .await;
    let (input_backend, pointer_positions) = match preflight_result {
        Ok(preflight) => preflight,
        Err(error) => {
            if let Some(publisher) = publisher.as_ref() {
                let _ = publisher
                    .mark_degraded(appliance::RuntimeErrorCode::Preflight)
                    .await;
            }
            return Err(error);
        }
    };

    let idle_timeout = CONNECTION_IDLE_TIMEOUT
        .try_into()
        .context("converting bounded QUIC idle timeout")?;
    let transport_config = QuicTransportConfig::builder()
        .max_idle_timeout(Some(idle_timeout))
        .keep_alive_interval(CONNECTION_KEEP_ALIVE_INTERVAL)
        .build();
    let endpoint = match Endpoint::builder(presets::N0)
        .secret_key(secret)
        .transport_config(transport_config)
        .bind()
        .await
    {
        Ok(endpoint) => endpoint,
        Err(error) => {
            if let Some(publisher) = publisher.as_ref() {
                let _ = publisher
                    .mark_degraded(appliance::RuntimeErrorCode::EndpointBind)
                    .await;
            }
            return Err(error).context("binding iroh endpoint");
        }
    };
    let node_id = endpoint.id();
    match tokio::time::timeout(Duration::from_secs(10), endpoint.online()).await {
        Ok(()) => info!(%node_id, "iroh endpoint is online"),
        Err(_) => {
            warn!(%node_id, "iroh endpoint online check timed out; continuing offline-capable")
        }
    }

    let moq_origin = Origin::random();
    let router = Router::builder(endpoint)
        .accept(
            CONTROL_ALPN_V1,
            ControlHandler {
                config: config.clone(),
                sessions: Arc::clone(&sessions),
                authorization: authorization.clone(),
            },
        )
        .accept(
            iroh_moq::ALPN,
            AuthorizedMoqHandler {
                sessions: Arc::clone(&sessions),
                origin: moq_origin,
            },
        )
        .accept(
            MEDIA_ALPN_V3,
            MediaV3Handler {
                config: config.clone(),
                sessions: Arc::clone(&sessions),
                authorization: authorization.clone(),
            },
        )
        .accept(
            MEDIA_ALPN_V2,
            MediaV2Handler {
                config: config.clone(),
                sessions: Arc::clone(&sessions),
                authorization: authorization.clone(),
            },
        )
        .accept(
            MEDIA_ALPN_V1,
            MediaHandler {
                config: config.clone(),
                sessions: Arc::clone(&sessions),
                authorization: authorization.clone(),
            },
        )
        .accept(
            MEDIA_FEEDBACK_ALPN_V1,
            MediaFeedbackHandler {
                config: config.clone(),
                sessions: Arc::clone(&sessions),
                authorization,
            },
        )
        .accept(
            INPUT_ALPN_V1,
            InputHandler {
                backend: input_backend,
                pointer_positions,
                sessions: Arc::clone(&sessions),
            },
        )
        .accept(
            AUDIO_ALPN_V1,
            AudioHandler {
                config: config.clone(),
                sessions: Arc::clone(&sessions),
            },
        )
        .spawn();

    if let Some(publisher) = publisher.as_ref()
        && let Err(error) = publisher.mark_ready().await
    {
        warn!(%error, "publishing ready appliance status failed; continuing service");
    }

    println!("node_id={node_id}");
    println!("status=ready");
    info!(%node_id, source = ?config.source, "sigil host ready");

    if let Some(seconds) = args.max_runtime_seconds {
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        info!(seconds, "maximum runtime reached");
    } else {
        let signal = match wait_for_shutdown_signal().await {
            Ok(signal) => signal,
            Err(error) => {
                if let Some(publisher) = publisher.as_ref() {
                    let _ = publisher
                        .mark_degraded(appliance::RuntimeErrorCode::ShutdownSignal)
                        .await;
                }
                return Err(error);
            }
        };
        info!(signal, "shutdown signal received");
    }

    if let Some(publisher) = publisher.as_ref()
        && let Err(error) = publisher.mark_stopping().await
    {
        warn!(%error, "publishing stopping appliance status failed; continuing shutdown");
    }
    if let Err(error) = router.shutdown().await {
        if let Some(publisher) = publisher.as_ref() {
            let _ = publisher
                .mark_degraded(appliance::RuntimeErrorCode::RouterShutdown)
                .await;
        }
        return Err(error).context("shutting down iroh router");
    }
    if let Some(publisher) = publisher
        && let Err(error) = publisher.finish_clean().await
    {
        warn!(%error, "publishing stopped appliance status failed");
    }
    Ok(())
}

async fn wait_for_shutdown_signal() -> Result<&'static str> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("installing SIGTERM handler")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.context("waiting for SIGINT")?;
                Ok("SIGINT")
            }
            _ = terminate.recv() => Ok("SIGTERM"),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .context("waiting for shutdown signal")?;
        Ok("interrupt")
    }
}

struct LoadedServeConfig {
    config: HostConfig,
    revision: Option<ConfigRevision>,
}

fn load_serve_config(args: &ServeArgs) -> Result<LoadedServeConfig> {
    ensure!(
        args.config.is_none(),
        "configured service loading must hold its lifecycle transaction locks"
    );
    let identity_path = args
        .identity
        .clone()
        .context("either --config or --identity/--source is required")?;
    let source = args
        .source
        .context("either --config or --identity/--source is required")?;
    ensure!(
        matches!(source, SourceArg::TestPattern),
        "direct proof mode supports only test-pattern; gamescope-pipewire requires --config"
    );
    let state_path = args.state_path.clone().unwrap_or_else(|| {
        identity_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("runtime")
    });
    Ok(LoadedServeConfig {
        config: HostConfig {
            identity_path,
            state_path,
            source: source.into(),
            width: Some(args.width),
            height: Some(args.height),
            framerate: args.framerate,
            codec: "h264".into(),
            input_mode: InputMode::Log,
            uinput: None,
            ffmpeg_path: args.ffmpeg.clone(),
            gamescope_pipewire: None,
            audio: None,
        },
        revision: None,
    })
}

fn parse_size(value: &str) -> std::result::Result<(u32, u32), String> {
    let (width, height) = value
        .split_once('x')
        .ok_or_else(|| "size must be WIDTHxHEIGHT".to_owned())?;
    let width = width.parse().map_err(|_| "invalid width".to_owned())?;
    let height = height.parse().map_err(|_| "invalid height".to_owned())?;
    Ok((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn parses_expected_size() {
        assert_eq!(parse_size("1280x800").unwrap(), (1280, 800));
        assert!(parse_size("1280").is_err());
    }

    #[test]
    fn direct_serve_requires_identity_and_source() {
        assert!(Cli::try_parse_from(["sigil", "serve"]).is_err());
        assert!(
            Cli::try_parse_from([
                "sigil",
                "serve",
                "--identity",
                "/tmp/host.key",
                "--source",
                "test-pattern"
            ])
            .is_ok()
        );
    }

    #[test]
    fn invitation_and_enrollment_commands_parse_strictly() {
        let peer = iroh::SecretKey::from_bytes(&[19; 32]).public().to_string();
        assert!(
            Cli::try_parse_from([
                "sigil",
                "invitation",
                "create",
                "--config",
                "/tmp/host.toml",
                "--peer",
                &peer,
                "--pointer-keyboard",
                "--gamepad",
                "--print-deep-link",
                "--output",
                "/tmp/portal.goq-invite",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "sigil",
                "invitation",
                "create",
                "--config",
                "/tmp/host.toml",
                "--peer",
                &peer,
                "--expires-in-seconds",
                "901",
                "--output",
                "/tmp/portal.goq-invite",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "sigil",
                "enrollment",
                "revoke",
                "--config",
                "/tmp/host.toml",
                "--expected-host-fingerprint",
                "aaaaaaaa…bbbbbbbb",
                "--json",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "sigil",
                "enrollment",
                "revoke",
                "--config",
                "/tmp/host.toml"
            ])
            .is_err()
        );
    }

    #[test]
    fn appliance_status_requires_explicit_json_output() {
        assert!(
            Cli::try_parse_from([
                "sigil",
                "appliance",
                "status",
                "--config",
                "/tmp/host.toml",
                "--json",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["sigil", "appliance", "status", "--config", "/tmp/host.toml"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "sigil",
                "appliance",
                "status",
                "--config",
                "/tmp/host.toml",
                "--json",
                "--schema-version",
                "2",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "sigil",
                "appliance",
                "status",
                "--config",
                "/tmp/host.toml",
                "--json",
                "--schema-version",
                "3",
            ])
            .is_err()
        );
    }

    #[test]
    fn appliance_enrollment_reset_requires_confirmation_and_json() {
        let valid = [
            "sigil",
            "appliance",
            "enrollment-reset",
            "--config",
            "/tmp/host.toml",
            "--expected-host-fingerprint",
            "aaaaaaaa…bbbbbbbb",
            "--json",
        ];
        assert!(Cli::try_parse_from(valid).is_ok());
        assert!(
            Cli::try_parse_from([
                "sigil",
                "appliance",
                "enrollment-reset",
                "--config",
                "/tmp/host.toml",
                "--json",
            ])
            .is_err()
        );
    }

    #[test]
    fn appliance_config_commands_require_explicit_machine_inputs() {
        assert!(
            Cli::try_parse_from([
                "sigil",
                "appliance",
                "config",
                "show",
                "--config",
                "/tmp/host.toml",
                "--json",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "sigil",
                "appliance",
                "config",
                "set",
                "--config",
                "/tmp/host.toml",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "sigil",
                "appliance",
                "config",
                "commit",
                "--config",
                "/tmp/host.toml",
                "--transaction",
                "0123456789abcdef0123456789abcdef",
                "--expected-instance",
                "fedcba9876543210fedcba9876543210",
                "--json",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "sigil",
                "appliance",
                "config",
                "commit",
                "--config",
                "/tmp/host.toml",
                "--transaction",
                "0123456789abcdef0123456789abcdef",
                "--json",
            ])
            .is_err()
        );
    }

    #[test]
    fn invitation_file_uses_create_new_semantics() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("portal.goq-invite");
        write_invitation_file(&path, "token").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "token\n");
        assert!(write_invitation_file(&path, "replacement").is_err());
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn static_media_transport_liveness_is_bounded() {
        assert_eq!(CONNECTION_IDLE_TIMEOUT, Duration::from_secs(5));
        assert_eq!(CONNECTION_KEEP_ALIVE_INTERVAL, Duration::from_secs(1));
        assert!(CONNECTION_KEEP_ALIVE_INTERVAL < CONNECTION_IDLE_TIMEOUT);
        assert!(CONNECTION_IDLE_TIMEOUT <= Duration::from_secs(6));
    }
}
