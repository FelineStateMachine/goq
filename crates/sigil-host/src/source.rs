use std::borrow::Cow;
use std::ffi::OsString;
use std::fs;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use sigil_protocol::PointerSurfaceDimensions;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::watch;
use tracing::{debug, warn};

use crate::clock::SessionClock;
use crate::config::{
    GamescopeEncoderBackend, GamescopePipewireConfig, HostConfig, VaapiRateControl,
};

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
use gstreamer::prelude::ObjectExt;

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, PermissionsExt};

const MAX_ENCODE_BUFFER: usize = 16 * 1024 * 1024;
const MAX_CURRENT_GOP_BYTES: usize = 32 * 1024 * 1024;
const MAX_PW_DUMP_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_INSPECT_OUTPUT: usize = 1024 * 1024;
const MAX_DIAGNOSTIC_OUTPUT: usize = 512 * 1024;
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ENCODER_BITRATE_KBPS: u32 = 100_000;
#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
const RESOLUTION_APPLY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RevisedBitrate {
    revision: u64,
    kbps: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RevisedResolution {
    revision: u64,
    width: u16,
    height: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct EncoderControlDesired {
    revision: u64,
    bitrate: Option<RevisedBitrate>,
    resolution: Option<RevisedResolution>,
    force_keyframe_revision: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncoderControlStatus {
    pub latest_revision: u64,
    pub applied_bitrate_revision: Option<u64>,
    pub applied_bitrate_kbps: Option<u32>,
    pub requested_resolution_revision: Option<u64>,
    pub applied_resolution_revision: Option<u64>,
    pub applied_width: Option<u16>,
    pub applied_height: Option<u16>,
    pub requested_force_keyframe_revision: Option<u64>,
    pub acknowledged_force_keyframe_revision: Option<u64>,
}

/// Bounded latest-state control for an in-process encoder.
///
/// Both directions are Tokio watch channels: callers can update control state
/// arbitrarily often without building a queue, while the encoder applies only
/// the latest revision. Revisions are process-monotonic and never wrap.
#[derive(Clone, Debug)]
pub struct EncoderControl {
    next_revision: Arc<AtomicU64>,
    desired: watch::Sender<EncoderControlDesired>,
    status: watch::Receiver<EncoderControlStatus>,
}

#[cfg_attr(
    not(any(test, all(target_os = "linux", feature = "in-process-gstreamer"))),
    expect(dead_code, reason = "constructed only by the opt-in encoder backend")
)]
impl EncoderControl {
    fn new() -> (
        Self,
        watch::Receiver<EncoderControlDesired>,
        watch::Sender<EncoderControlStatus>,
    ) {
        let (desired, desired_rx) = watch::channel(EncoderControlDesired::default());
        let (status_tx, status) = watch::channel(EncoderControlStatus::default());
        (
            Self {
                next_revision: Arc::new(AtomicU64::new(0)),
                desired,
                status,
            },
            desired_rx,
            status_tx,
        )
    }

    fn next_revision(&self) -> Result<u64> {
        self.next_revision
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |revision| {
                revision.checked_add(1)
            })
            .map(|revision| revision + 1)
            .map_err(|_| anyhow::anyhow!("encoder control revision exhausted"))
    }

    pub fn request_bitrate_kbps(&self, kbps: u32) -> Result<u64> {
        ensure!(
            (1..=MAX_ENCODER_BITRATE_KBPS).contains(&kbps),
            "encoder bitrate must be between 1 and {MAX_ENCODER_BITRATE_KBPS} kbps"
        );
        let revision = self.next_revision()?;
        self.desired.send_modify(|desired| {
            desired.revision = revision;
            desired.bitrate = Some(RevisedBitrate { revision, kbps });
        });
        Ok(revision)
    }

    pub fn request_force_keyframe(&self) -> Result<u64> {
        let revision = self.next_revision()?;
        self.desired.send_modify(|desired| {
            desired.revision = revision;
            desired.force_keyframe_revision = Some(revision);
        });
        Ok(revision)
    }

    pub fn request_resolution(&self, width: u16, height: u16) -> Result<u64> {
        ensure!(
            width >= 64 && height >= 64,
            "encoder dimensions must be at least 64x64"
        );
        ensure!(
            width.is_multiple_of(2) && height.is_multiple_of(2),
            "H.264 encoder dimensions must be even"
        );
        let revision = self.next_revision()?;
        self.desired.send_modify(|desired| {
            desired.revision = revision;
            desired.resolution = Some(RevisedResolution {
                revision,
                width,
                height,
            });
        });
        Ok(revision)
    }

    pub fn status(&self) -> EncoderControlStatus {
        *self.status.borrow()
    }

    /// Wait until the encoder confirms the exact bitrate revision and exposes
    /// its property readback. A newer applied revision means this request was
    /// coalesced before application and therefore fails rather than pretending
    /// the requested revision committed.
    pub async fn wait_for_bitrate_applied(
        &self,
        revision: u64,
        timeout: Duration,
    ) -> Result<EncoderControlStatus> {
        let mut status = self.status.clone();
        let wait = async {
            loop {
                let snapshot = *status.borrow_and_update();
                match snapshot.applied_bitrate_revision {
                    Some(applied) if applied == revision => return Ok(snapshot),
                    Some(applied) if applied > revision => bail!(
                        "encoder bitrate revision {revision} was superseded by revision {applied}"
                    ),
                    _ => {}
                }
                status
                    .changed()
                    .await
                    .context("encoder control status closed before bitrate was applied")?;
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .with_context(|| format!("timed out waiting for encoder bitrate revision {revision}"))?
    }

    /// Wait for a forced-keyframe request to be committed by observing a
    /// keyframe that also carries codec configuration.
    pub async fn wait_for_force_keyframe_acknowledged(
        &self,
        revision: u64,
        timeout: Duration,
    ) -> Result<EncoderControlStatus> {
        let mut status = self.status.clone();
        let wait = async {
            loop {
                let snapshot = *status.borrow_and_update();
                match snapshot.acknowledged_force_keyframe_revision {
                    Some(acknowledged) if acknowledged == revision => return Ok(snapshot),
                    Some(acknowledged) if acknowledged > revision => bail!(
                        "force-keyframe revision {revision} was superseded by revision {acknowledged}"
                    ),
                    _ => {}
                }
                if snapshot
                    .requested_force_keyframe_revision
                    .is_some_and(|requested| requested > revision)
                {
                    bail!("force-keyframe revision {revision} was coalesced before application");
                }
                status.changed().await.context(
                    "encoder control status closed before forced keyframe was acknowledged",
                )?;
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .with_context(|| format!("timed out waiting for force-keyframe revision {revision}"))?
    }

    /// Wait for recovery to reach any configured IDR at or after `revision`.
    ///
    /// Recovery requests are deliberately coalesced with later encoder
    /// controls. Unlike transactional adaptive updates, a newer configured IDR
    /// still satisfies the decoder barrier established by an older request.
    pub async fn wait_for_recovery_keyframe_acknowledged(
        &self,
        revision: u64,
        timeout: Duration,
    ) -> Result<EncoderControlStatus> {
        let mut status = self.status.clone();
        let wait = async {
            loop {
                let snapshot = *status.borrow_and_update();
                if snapshot
                    .acknowledged_force_keyframe_revision
                    .is_some_and(|acknowledged| acknowledged >= revision)
                {
                    return Ok(snapshot);
                }
                status.changed().await.context(
                    "encoder control status closed before recovery keyframe was acknowledged",
                )?;
            }
        };
        tokio::time::timeout(timeout, wait).await.with_context(|| {
            format!("timed out waiting for recovery keyframe revision {revision}")
        })?
    }

    /// Wait until the exact target dimensions emerge from the encoder on an
    /// independently decodable access unit. A property write or caps event is
    /// not an application acknowledgement.
    pub async fn wait_for_resolution_applied(
        &self,
        revision: u64,
        width: u16,
        height: u16,
        timeout: Duration,
    ) -> Result<EncoderControlStatus> {
        let mut status = self.status.clone();
        let wait = async {
            loop {
                let snapshot = *status.borrow_and_update();
                match snapshot.applied_resolution_revision {
                    Some(applied) if applied == revision => {
                        ensure!(
                            snapshot.applied_width == Some(width)
                                && snapshot.applied_height == Some(height),
                            "encoder resolution revision {revision} acknowledged {:?}x{:?}, expected {width}x{height}",
                            snapshot.applied_width,
                            snapshot.applied_height
                        );
                        return Ok(snapshot);
                    }
                    Some(applied) if applied > revision => bail!(
                        "encoder resolution revision {revision} was superseded by revision {applied}"
                    ),
                    _ => {}
                }
                if snapshot
                    .requested_resolution_revision
                    .is_some_and(|requested| requested > revision)
                {
                    bail!(
                        "encoder resolution revision {revision} was coalesced before application"
                    );
                }
                status
                    .changed()
                    .await
                    .context("encoder control status closed before resolution was applied")?;
            }
        };
        tokio::time::timeout(timeout, wait).await.with_context(|| {
            format!("timed out waiting for encoder resolution revision {revision}")
        })?
    }
}

#[cfg(test)]
pub(crate) struct EncoderControlTestHarness {
    pub control: EncoderControl,
    pub status: watch::Sender<EncoderControlStatus>,
    _desired: watch::Receiver<EncoderControlDesired>,
}

#[cfg(test)]
impl EncoderControlTestHarness {
    pub(crate) fn new() -> Self {
        let (control, desired, status) = EncoderControl::new();
        Self {
            control,
            status,
            _desired: desired,
        }
    }

    pub(crate) fn requested_force_keyframe_revision(&self) -> Option<u64> {
        self._desired.borrow().force_keyframe_revision
    }
}

#[derive(Clone, Debug)]
pub struct EncodedFrame {
    pub sequence: u64,
    pub width: u16,
    pub height: u16,
    /// Session-monotonic timestamp when the complete encoded access unit became
    /// observable to the daemon, using the same epoch as audio. Raw PipeWire
    /// capture PTS is not preserved by the current stdout bridge, so this must
    /// not be presented as capture age.
    pub capture_timestamp_micros: u64,
    /// Post-encode observation PTS in the shared session clock. This preserves
    /// real gaps in damage-driven video output.
    pub presentation_timestamp_micros: i64,
    pub observed_at: Instant,
    pub keyframe: bool,
    pub codec_config: bool,
    /// This configured keyframe begins a decoder generation that must not use
    /// reference state from the preceding encoded resolution.
    pub discontinuity: bool,
    pub data: Arc<[u8]>,
}

#[derive(Clone, Debug)]
pub(crate) struct EncodedGop {
    pub frames: Vec<EncodedFrame>,
    pub(crate) payload_bytes: usize,
}

pub struct EncodedSource {
    pub frames: watch::Receiver<Option<EncodedFrame>>,
    /// A bounded snapshot of every access unit from the latest IDR through the
    /// current frame. A new subscriber can therefore reconstruct the current
    /// static image without receiving an orphan inter-frame.
    pub(crate) current_gop: watch::Receiver<Option<EncodedGop>>,
    pub task: tokio::task::JoinHandle<Result<()>>,
    pub pointer_surface_dimensions: Option<PointerSurfaceDimensions>,
    pub encoder_control: Option<EncoderControl>,
}

pub fn spawn_test_pattern(config: HostConfig, session_clock: SessionClock) -> EncodedSource {
    let (sender, receiver) = watch::channel(None);
    let (current_gop_sender, current_gop) = watch::channel(None);
    let task = tokio::spawn(async move {
        run_test_pattern(config, session_clock, sender, current_gop_sender).await
    });
    EncodedSource {
        frames: receiver,
        current_gop,
        task,
        pointer_surface_dimensions: None,
        encoder_control: None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GamescopePreflight {
    /// PipeWire object.serial, resolved from exact configured properties.
    pub target_object: String,
    /// Native Gamescope surface used by the relative-pointer coordinate space.
    pub pointer_surface_dimensions: PointerSurfaceDimensions,
    /// Bounded encoded mode after applying an optional explicit size override.
    pub video_mode: CaptureVideoMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureVideoMode {
    pub width: u16,
    pub height: u16,
    pub framerate: u32,
}

impl CaptureVideoMode {
    fn resolve(config: &HostConfig, native: PointerSurfaceDimensions) -> Result<Self> {
        let configured = config.configured_dimensions();
        let (width, height) =
            configured.unwrap_or((u32::from(native.width), u32::from(native.height)));
        ensure!(
            (64..=7_680).contains(&width) && (64..=4_320).contains(&height),
            "resolved Gamescope encoded dimensions {width}x{height} are outside H.264 bounds"
        );
        ensure!(
            width.is_multiple_of(2) && height.is_multiple_of(2),
            "resolved Gamescope encoded dimensions {width}x{height} must be even"
        );
        if configured.is_some() {
            ensure!(
                u64::from(width) * u64::from(native.height)
                    == u64::from(height) * u64::from(native.width),
                "configured Gamescope encoded dimensions {width}x{height} must preserve native {}x{} aspect ratio",
                native.width,
                native.height
            );
        }
        Ok(Self {
            width: u16::try_from(width).context("resolved Gamescope width exceeds metadata")?,
            height: u16::try_from(height).context("resolved Gamescope height exceeds metadata")?,
            framerate: config.framerate,
        })
    }
}

pub async fn preflight_gamescope_pipewire(config: &HostConfig) -> Result<GamescopePreflight> {
    config.validate()?;
    preflight_gamescope_static(config).await?;
    resolve_gamescope_pipewire_target(config).await
}

async fn preflight_gamescope_static(config: &HostConfig) -> Result<()> {
    let pipewire = gamescope_config(config)?;

    for (name, path) in [
        ("pw_dump_path", &pipewire.pw_dump_path),
        ("gst_inspect_path", &pipewire.gst_inspect_path),
    ] {
        validate_executable(name, path)?;
    }
    if pipewire.encoder_backend == GamescopeEncoderBackend::ExternalGstLaunch {
        validate_executable("gst_launch_path", &pipewire.gst_launch_path)?;
    }
    validate_amd_render_node(&pipewire.vaapi_render_node)?;

    let terminal_sink = match pipewire.encoder_backend {
        GamescopeEncoderBackend::ExternalGstLaunch => "fdsink",
        GamescopeEncoderBackend::InProcessGstreamer => "appsink",
    };
    for element in [
        "pipewiresrc",
        "queue",
        "videoconvert",
        "videoscale",
        "videorate",
        "capsfilter",
        pipewire.vaapi_encoder.as_str(),
        "h264parse",
        terminal_sink,
    ] {
        let mut command = Command::new(&pipewire.gst_inspect_path);
        command.arg(element).env("LC_ALL", "C");
        let output = run_bounded_command(command, MAX_INSPECT_OUTPUT).await?;
        ensure!(
            output.status.success(),
            "GStreamer element {element:?} is unavailable: {}",
            diagnostic(&output.stderr)
        );
        if element == pipewire.vaapi_encoder {
            validate_encoder_inspection(&output.stdout, pipewire)?;
        }
    }

    Ok(())
}

async fn resolve_gamescope_pipewire_target(config: &HostConfig) -> Result<GamescopePreflight> {
    config.validate()?;
    let pipewire = gamescope_config(config)?;
    let mut command = Command::new(&pipewire.pw_dump_path);
    command.env("LC_ALL", "C");
    let output = run_bounded_command(command, MAX_PW_DUMP_OUTPUT).await?;
    ensure!(
        output.status.success(),
        "pw-dump failed: {}",
        diagnostic(&output.stderr)
    );
    resolve_pipewire_node(&output.stdout, config)
}

pub async fn spawn_gamescope_pipewire(
    config: HostConfig,
    session_clock: SessionClock,
) -> Result<EncodedSource> {
    let preflight = preflight_gamescope_pipewire(&config).await?;
    spawn_gamescope_pipewire_with_preflight(config, session_clock, preflight)
}

/// Start capture after `serve` has completed the static executable/GstVA
/// preflight. The PipeWire node is still resolved immediately before each
/// session because its object serial can change when Gamescope restarts.
pub async fn spawn_gamescope_pipewire_after_static_preflight(
    config: HostConfig,
    session_clock: SessionClock,
) -> Result<EncodedSource> {
    let preflight = resolve_gamescope_pipewire_target(&config).await?;
    spawn_gamescope_pipewire_with_preflight(config, session_clock, preflight)
}

fn spawn_gamescope_pipewire_with_preflight(
    config: HostConfig,
    session_clock: SessionClock,
    preflight: GamescopePreflight,
) -> Result<EncodedSource> {
    match gamescope_config(&config)?.encoder_backend {
        GamescopeEncoderBackend::ExternalGstLaunch => {
            spawn_gamescope_pipewire_with_target(config, session_clock, preflight)
        }
        GamescopeEncoderBackend::InProcessGstreamer => {
            #[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
            {
                spawn_gamescope_pipewire_in_process_with_target(config, session_clock, preflight)
            }
            #[cfg(not(all(target_os = "linux", feature = "in-process-gstreamer")))]
            {
                let _ = (session_clock, preflight);
                bail!(
                    "gamescope_pipewire.encoder_backend=in-process-gstreamer requires a Linux Sigil build with the in-process-gstreamer feature"
                )
            }
        }
    }
}

fn spawn_gamescope_pipewire_with_target(
    config: HostConfig,
    session_clock: SessionClock,
    preflight: GamescopePreflight,
) -> Result<EncodedSource> {
    let (sender, receiver) = watch::channel(None);
    let (current_gop_sender, current_gop) = watch::channel(None);
    let target_object = preflight.target_object;
    let video_mode = preflight.video_mode;
    let task = tokio::spawn(async move {
        run_gamescope_pipewire(
            config,
            session_clock,
            target_object,
            video_mode,
            sender,
            current_gop_sender,
        )
        .await
    });
    Ok(EncodedSource {
        frames: receiver,
        current_gop,
        task,
        pointer_surface_dimensions: Some(preflight.pointer_surface_dimensions),
        encoder_control: None,
    })
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn spawn_gamescope_pipewire_in_process_with_target(
    config: HostConfig,
    session_clock: SessionClock,
    preflight: GamescopePreflight,
) -> Result<EncodedSource> {
    let description = gamescope_in_process_pipeline_description(
        &config,
        &preflight.target_object,
        preflight.video_mode,
    )?;
    let max_gop_frames = interactive_gop_frames(preflight.video_mode.framerate) as usize;
    let expected_device_path = Some(
        gamescope_config(&config)?
            .vaapi_render_node
            .to_string_lossy()
            .into_owned(),
    );
    spawn_in_process_pipeline(
        description,
        session_clock,
        max_gop_frames,
        Some(preflight.pointer_surface_dimensions),
        expected_device_path,
        preflight.video_mode.width,
        preflight.video_mode.height,
    )
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn spawn_in_process_pipeline(
    description: String,
    session_clock: SessionClock,
    max_gop_frames: usize,
    pointer_surface_dimensions: Option<PointerSurfaceDimensions>,
    expected_device_path: Option<String>,
    initial_width: u16,
    initial_height: u16,
) -> Result<EncodedSource> {
    let (sender, receiver) = watch::channel(None);
    let (current_gop_sender, current_gop) = watch::channel(None);
    let (control, desired, status) = EncoderControl::new();
    let task = tokio::spawn(async move {
        tokio::task::spawn_blocking(move || {
            run_in_process_pipeline(
                &description,
                session_clock,
                sender,
                current_gop_sender,
                max_gop_frames,
                desired,
                status,
                expected_device_path.as_deref(),
                initial_width,
                initial_height,
            )
        })
        .await
        .context("in-process GStreamer worker panicked")?
    });
    Ok(EncodedSource {
        frames: receiver,
        current_gop,
        task,
        pointer_surface_dimensions,
        encoder_control: Some(control),
    })
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn gamescope_in_process_pipeline_description(
    config: &HostConfig,
    target_object: &str,
    video_mode: CaptureVideoMode,
) -> Result<String> {
    let pipewire = gamescope_config(config)?;
    validate_pipewire_target_object(target_object)?;
    ensure!(
        pipewire.rate_control == VaapiRateControl::Cbr,
        "in-process GStreamer requires CBR so bitrate can be changed while playing"
    );
    let bitrate = pipewire
        .bitrate_kbps
        .context("CBR bitrate is missing after validation")?;
    let raw_caps = format!(
        "video/x-raw,format=NV12,width={},height={},framerate={}/1",
        video_mode.width, video_mode.height, video_mode.framerate
    );
    Ok(format!(
        "pipewiresrc do-timestamp=true min-buffers=1 max-buffers=4 use-bufferpool=false target-object={target_object} \
         ! queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream \
         ! videorate drop-only=true max-rate={} \
         ! videoconvert ! videoscale \
         ! capsfilter name=sigil_scale_caps caps={raw_caps} caps-change-mode=delayed \
         ! {} name=sigil_encoder rate-control=cbr bitrate={bitrate} target-usage=7 key-int-max={} b-frames=0 ref-frames=1 aud=true \
         ! h264parse config-interval=-1 \
         ! video/x-h264,stream-format=byte-stream,alignment=au \
         ! appsink name=sigil_sink max-buffers=1 drop=false sync=false",
        video_mode.framerate,
        pipewire.vaapi_encoder,
        interactive_gop_frames(video_mode.framerate),
    ))
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn validate_pipewire_target_object(target_object: &str) -> Result<()> {
    ensure!(
        !target_object.is_empty()
            && target_object.len() <= 32
            && target_object.bytes().all(|byte| byte.is_ascii_digit()),
        "resolved PipeWire object.serial is invalid"
    );
    Ok(())
}

async fn run_gamescope_pipewire(
    config: HostConfig,
    session_clock: SessionClock,
    target_object: String,
    video_mode: CaptureVideoMode,
    sender: watch::Sender<Option<EncodedFrame>>,
    current_gop_sender: watch::Sender<Option<EncodedGop>>,
) -> Result<()> {
    let pipewire = gamescope_config(&config)?;
    let args = gamescope_pipeline_args(&config, &target_object, video_mode)?;
    let mut command = Command::new(&pipewire.gst_launch_path);
    command
        .args(args)
        .env("LC_ALL", "C")
        // This is honored by the legacy VAAPI plugin and harmless for GstVA.
        // GstVA selection is independently checked against device-path above.
        .env("GST_VAAPI_DRM_DEVICE", &pipewire.vaapi_render_node)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().with_context(|| {
        format!(
            "starting Gamescope pipeline with {}",
            pipewire.gst_launch_path.display()
        )
    })?;
    let mut stdout = child
        .stdout
        .take()
        .context("GStreamer stdout was not piped")?;
    let stderr = child
        .stderr
        .take()
        .context("GStreamer stderr was not piped")?;
    let stderr_task = tokio::spawn(log_stderr_chunks(stderr, "gstreamer"));

    let result = forward_annex_b_stream(
        session_clock,
        &mut stdout,
        &sender,
        &current_gop_sender,
        interactive_gop_frames(video_mode.framerate) as usize,
        video_mode.width,
        video_mode.height,
    )
    .await;
    let _ = child.kill().await;
    let exit_status = child.wait().await.ok();
    stderr_task.abort();

    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Some(status) = exit_status {
                Err(error).with_context(|| format!("GStreamer pipeline exited with {status}"))
            } else {
                Err(error)
            }
        }
    }
}

async fn run_test_pattern(
    config: HostConfig,
    session_clock: SessionClock,
    sender: watch::Sender<Option<EncodedFrame>>,
    current_gop_sender: watch::Sender<Option<EncodedGop>>,
) -> Result<()> {
    let (width, height) = config.test_pattern_dimensions()?;
    let input = format!(
        "testsrc2=size={}x{}:rate={}",
        width, height, config.framerate
    );
    let gop = interactive_gop_frames(config.framerate).to_string();

    let mut command = Command::new(&config.ffmpeg_path);
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-re")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(input)
        .arg("-an")
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("ultrafast")
        .arg("-tune")
        .arg("zerolatency")
        .arg("-g")
        .arg(&gop)
        .arg("-keyint_min")
        .arg(&gop)
        .arg("-sc_threshold")
        .arg("0")
        .arg("-bf")
        .arg("0")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-bsf:v")
        .arg("h264_metadata=aud=insert")
        .arg("-f")
        .arg("h264")
        .arg("pipe:1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .with_context(|| format!("starting {}", config.ffmpeg_path.display()))?;
    let mut stdout = child.stdout.take().context("ffmpeg stdout was not piped")?;
    let stderr = child.stderr.take().context("ffmpeg stderr was not piped")?;
    let stderr_task = tokio::spawn(log_stderr_chunks(stderr, "ffmpeg"));

    let result = forward_annex_b_stream(
        session_clock,
        &mut stdout,
        &sender,
        &current_gop_sender,
        interactive_gop_frames(config.framerate) as usize,
        u16::try_from(width).context("configured width exceeds encoder metadata")?,
        u16::try_from(height).context("configured height exceeds encoder metadata")?,
    )
    .await;
    if result.is_ok() {
        let _ = child.kill().await;
    }
    let status = child.wait().await.context("waiting for ffmpeg")?;
    stderr_task.abort();
    match result {
        Ok(()) => Ok(()),
        Err(error) => Err(error).with_context(|| format!("ffmpeg exited with {status}")),
    }
}

async fn forward_annex_b_stream<R>(
    session_clock: SessionClock,
    stdout: &mut R,
    sender: &watch::Sender<Option<EncodedFrame>>,
    current_gop_sender: &watch::Sender<Option<EncodedGop>>,
    max_gop_frames: usize,
    width: u16,
    height: u16,
) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut publisher = AccessUnitPublisher::new(
        session_clock,
        sender,
        current_gop_sender,
        max_gop_frames,
        width,
        height,
    );
    let mut parser = AnnexBAccessUnitParser::default();
    let mut chunk = [0_u8; 16 * 1024];

    loop {
        let count = stdout
            .read(&mut chunk)
            .await
            .context("reading encoded source output")?;
        if count == 0 {
            bail!("encoded source ended unexpectedly");
        }

        for access_unit in parser.push(&chunk[..count])? {
            publisher.publish(access_unit)?;
            if publisher.receivers_closed() {
                debug!("encoded source has no receivers");
                return Ok(());
            }
        }
    }
}

struct AccessUnitPublisher<'a> {
    session_clock: SessionClock,
    sender: &'a watch::Sender<Option<EncodedFrame>>,
    current_gop_sender: &'a watch::Sender<Option<EncodedGop>>,
    max_gop_frames: usize,
    width: u16,
    height: u16,
    sequence: u64,
}

#[cfg(any(test, all(target_os = "linux", feature = "in-process-gstreamer")))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingResolutionTransition {
    target: RevisedResolution,
    deadline: Instant,
}

#[cfg(any(test, all(target_os = "linux", feature = "in-process-gstreamer")))]
fn classify_resolution_sample(
    current: (u16, u16),
    pending: Option<PendingResolutionTransition>,
    sample: (u16, u16),
    configured_idr: bool,
) -> Result<Option<bool>> {
    if let Some(pending) = pending {
        if sample != (pending.target.width, pending.target.height) || !configured_idr {
            return Ok(None);
        }
        return Ok(Some(true));
    }
    ensure!(
        sample == current,
        "encoder changed resolution from {}x{} to {}x{} without a pending control revision",
        current.0,
        current.1,
        sample.0,
        sample.1
    );
    Ok(Some(false))
}

impl<'a> AccessUnitPublisher<'a> {
    fn new(
        session_clock: SessionClock,
        sender: &'a watch::Sender<Option<EncodedFrame>>,
        current_gop_sender: &'a watch::Sender<Option<EncodedGop>>,
        max_gop_frames: usize,
        width: u16,
        height: u16,
    ) -> Self {
        Self {
            session_clock,
            sender,
            current_gop_sender,
            max_gop_frames,
            width,
            height,
            sequence: 0,
        }
    }

    fn publish(&mut self, access_unit: Vec<u8>) -> Result<bool> {
        self.publish_with_metadata(access_unit, self.width, self.height, false)
    }

    fn publish_with_metadata(
        &mut self,
        access_unit: Vec<u8>,
        width: u16,
        height: u16,
        discontinuity: bool,
    ) -> Result<bool> {
        ensure!(!access_unit.is_empty(), "encoded access unit is empty");
        ensure!(
            access_unit.len() <= MAX_ENCODE_BUFFER,
            "encoded access unit exceeds {MAX_ENCODE_BUFFER} bytes"
        );
        let observed_at = Instant::now();
        let capture_timestamp_micros = self.session_clock.micros_at(observed_at);
        let presentation_timestamp_micros =
            i64::try_from(capture_timestamp_micros).unwrap_or(i64::MAX);
        let keyframe = is_h264_keyframe(&access_unit);
        let codec_config = has_h264_codec_config(&access_unit);
        let frame = EncodedFrame {
            sequence: self.sequence,
            width,
            height,
            capture_timestamp_micros,
            presentation_timestamp_micros,
            observed_at,
            keyframe,
            codec_config,
            discontinuity,
            data: Arc::from(access_unit),
        };
        self.sequence = self
            .sequence
            .checked_add(1)
            .context("encoded frame sequence exhausted")?;
        publish_encoded_frame(
            self.sender,
            self.current_gop_sender,
            frame,
            self.max_gop_frames,
            MAX_CURRENT_GOP_BYTES,
        );
        self.width = width;
        self.height = height;
        Ok(keyframe && codec_config)
    }

    fn receivers_closed(&self) -> bool {
        self.sender.is_closed() && self.current_gop_sender.is_closed()
    }
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn run_in_process_pipeline(
    description: &str,
    session_clock: SessionClock,
    sender: watch::Sender<Option<EncodedFrame>>,
    current_gop_sender: watch::Sender<Option<EncodedGop>>,
    max_gop_frames: usize,
    mut desired: watch::Receiver<EncoderControlDesired>,
    status: watch::Sender<EncoderControlStatus>,
    expected_device_path: Option<&str>,
    initial_width: u16,
    initial_height: u16,
) -> Result<()> {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;
    gst::init().context("initializing in-process GStreamer")?;
    let pipeline = gst::parse::launch(description)
        .context("parsing in-process GStreamer pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| {
            anyhow::anyhow!("in-process GStreamer description did not create a pipeline")
        })?;
    let encoder = pipeline
        .by_name("sigil_encoder")
        .context("in-process GStreamer pipeline has no named encoder")?;
    let scale_caps = pipeline
        .by_name("sigil_scale_caps")
        .context("in-process GStreamer pipeline has no named scale capsfilter")?;
    let sink = pipeline
        .by_name("sigil_sink")
        .context("in-process GStreamer pipeline has no named appsink")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("named in-process sink is not an appsink"))?;
    sink.set_max_buffers(1);
    // Encoded dependent AUs must not be dropped here: the raw queue before the
    // encoder is the only latest-frame boundary. This one-buffer sink applies
    // bounded backpressure until the publishing loop consumes the AU.
    sink.set_drop(false);
    sink.set_sync(false);
    validate_mutable_playing_bitrate(&encoder)?;
    validate_resolution_capsfilter(&scale_caps)?;

    pipeline
        .set_state(gst::State::Playing)
        .context("starting in-process GStreamer pipeline")?;
    let result = (|| {
        let (state_change, state, pending) = pipeline.state(gst::ClockTime::from_seconds(5));
        state_change.context("in-process GStreamer pipeline failed while waiting for PLAYING")?;
        ensure!(
            state == gst::State::Playing,
            "in-process GStreamer pipeline did not reach PLAYING within 5 seconds (state {state:?}, pending {pending:?})"
        );
        if let Some(expected_device_path) = expected_device_path {
            validate_active_encoder_device_path(&encoder, expected_device_path)?;
        }
        let bus = pipeline
            .bus()
            .context("in-process GStreamer pipeline has no bus")?;
        let mut publisher = AccessUnitPublisher::new(
            session_clock,
            &sender,
            &current_gop_sender,
            max_gop_frames,
            initial_width,
            initial_height,
        );
        let mut applied_desired_revision = 0_u64;
        let mut pending_force_keyframe_revision = None;
        let mut pending_resolution = None;
        status.send_modify(|status| {
            status.applied_width = Some(initial_width);
            status.applied_height = Some(initial_height);
        });

        loop {
            if let Some(message) = bus.timed_pop(gst::ClockTime::ZERO) {
                match message.view() {
                    gst::MessageView::Error(error) => {
                        bail!(
                            "in-process GStreamer pipeline error from {:?}: {} ({:?})",
                            error.src().map(|source| source.path_string()),
                            error.error(),
                            error.debug()
                        );
                    }
                    gst::MessageView::Eos(_) => {
                        bail!("in-process GStreamer pipeline ended unexpectedly");
                    }
                    _ => {}
                }
            }

            let desired_state = *desired.borrow_and_update();
            if desired_state.revision > applied_desired_revision {
                apply_encoder_control(
                    &encoder,
                    &scale_caps,
                    &sink,
                    desired_state,
                    &status,
                    &current_gop_sender,
                    &mut pending_force_keyframe_revision,
                    &mut pending_resolution,
                )?;
                applied_desired_revision = desired_state.revision;
            }

            if pending_resolution.is_some_and(|pending: PendingResolutionTransition| {
                Instant::now() >= pending.deadline
            }) {
                let pending = pending_resolution.expect("checked as present");
                bail!(
                    "encoder resolution revision {} did not produce a configured {}x{} IDR within {:?}",
                    pending.target.revision,
                    pending.target.width,
                    pending.target.height,
                    RESOLUTION_APPLY_TIMEOUT
                );
            }

            if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(50)) {
                let (width, height) = encoded_sample_dimensions(&sample)?;
                let buffer = sample
                    .buffer()
                    .context("in-process appsink sample has no buffer")?;
                let map = buffer
                    .map_readable()
                    .context("mapping in-process encoded access unit")?;
                ensure!(
                    map.as_slice().len() <= MAX_ENCODE_BUFFER,
                    "in-process encoded access unit exceeds {MAX_ENCODE_BUFFER} bytes"
                );
                let access_unit = map.as_slice();
                let configured_idr =
                    is_h264_keyframe(access_unit) && has_h264_codec_config(access_unit);
                let Some(discontinuity) = classify_resolution_sample(
                    (publisher.width, publisher.height),
                    pending_resolution,
                    (width, height),
                    configured_idr,
                )?
                else {
                    continue;
                };
                let configured_idr = publisher.publish_with_metadata(
                    access_unit.to_vec(),
                    width,
                    height,
                    discontinuity,
                )?;
                if discontinuity {
                    let applied = pending_resolution
                        .take()
                        .expect("discontinuity requires a pending resolution");
                    status.send_modify(|status| {
                        status.applied_resolution_revision = Some(applied.target.revision);
                        status.applied_width = Some(width);
                        status.applied_height = Some(height);
                    });
                }
                acknowledge_force_keyframe_if_configured_idr(
                    configured_idr,
                    &mut pending_force_keyframe_revision,
                    &status,
                );
            } else if sink.is_eos() {
                bail!("in-process GStreamer appsink reached EOS unexpectedly");
            }

            if publisher.receivers_closed() {
                debug!("in-process encoded source has no receivers");
                return Ok(());
            }
        }
    })();
    if let Err(error) = pipeline.set_state(gst::State::Null) {
        warn!(%error, "failed to stop in-process GStreamer pipeline cleanly");
    }
    result
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn validate_active_encoder_device_path(
    encoder: &gstreamer::Element,
    expected_device_path: &str,
) -> Result<()> {
    use gstreamer as gst;
    use gstreamer::prelude::*;

    let property = encoder
        .find_property("device-path")
        .context("configured in-process VA encoder has no device-path property")?;
    ensure!(
        property.flags().contains(gst::glib::ParamFlags::READABLE),
        "configured in-process VA encoder device-path property is not readable"
    );
    let observed_device_path = encoder.property::<Option<String>>("device-path");
    ensure!(
        observed_device_path.as_deref() == Some(expected_device_path),
        "configured in-process VA encoder uses device-path {observed_device_path:?}, expected {expected_device_path:?}"
    );
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn validate_mutable_playing_bitrate(encoder: &gstreamer::Element) -> Result<()> {
    use gstreamer as gst;
    use gstreamer::prelude::*;

    let property = encoder
        .find_property("bitrate")
        .context("configured in-process encoder has no bitrate property")?;
    ensure!(
        property.flags().contains(gst::glib::ParamFlags::WRITABLE),
        "configured in-process encoder bitrate property is not writable"
    );
    ensure!(
        property.flags().contains(gst::PARAM_FLAG_MUTABLE_PLAYING),
        "configured in-process encoder bitrate property is not mutable while playing"
    );
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn validate_resolution_capsfilter(capsfilter: &gstreamer::Element) -> Result<()> {
    use gstreamer::prelude::*;

    let property = capsfilter
        .find_property("caps")
        .context("configured scale capsfilter has no caps property")?;
    ensure!(
        property
            .flags()
            .contains(gstreamer::glib::ParamFlags::WRITABLE),
        "configured scale capsfilter caps property is not writable"
    );
    let caps = capsfilter.property::<gstreamer::Caps>("caps");
    let _ = dimensions_from_caps(&caps)?;
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn dimensions_from_caps(caps: &gstreamer::CapsRef) -> Result<(u16, u16)> {
    let structure = caps
        .structure(0)
        .context("configured video caps have no structure")?;
    let width = structure
        .get::<i32>("width")
        .context("configured video caps have no integer width")?;
    let height = structure
        .get::<i32>("height")
        .context("configured video caps have no integer height")?;
    Ok((
        u16::try_from(width).context("configured video caps width is outside u16")?,
        u16::try_from(height).context("configured video caps height is outside u16")?,
    ))
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn encoded_sample_dimensions(sample: &gstreamer::Sample) -> Result<(u16, u16)> {
    let caps = sample
        .caps()
        .context("in-process encoded sample has no negotiated caps")?;
    dimensions_from_caps(caps)
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn apply_resolution_caps(
    capsfilter: &gstreamer::Element,
    resolution: RevisedResolution,
) -> Result<()> {
    use gstreamer::prelude::*;

    let mut caps = capsfilter.property::<gstreamer::Caps>("caps");
    let caps_mut = caps.make_mut();
    let structure = caps_mut
        .structure_mut(0)
        .context("configured scale capsfilter has no caps structure")?;
    structure.set("width", i32::from(resolution.width));
    structure.set("height", i32::from(resolution.height));
    capsfilter.set_property("caps", caps);
    let readback = capsfilter.property::<gstreamer::Caps>("caps");
    ensure!(
        dimensions_from_caps(&readback)? == (resolution.width, resolution.height),
        "scale capsfilter resolution readback does not match requested {}x{}",
        resolution.width,
        resolution.height
    );
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn push_configured_keyframe_request(sink: &gstreamer_app::AppSink) -> Result<()> {
    use gstreamer::prelude::*;

    let event = gstreamer_video::UpstreamForceKeyUnitEvent::builder()
        .all_headers(true)
        .build();
    let sink_pad = sink
        .static_pad("sink")
        .context("configured in-process appsink has no sink pad")?;
    ensure!(
        sink_pad.push_event(event),
        "in-process encoder rejected upstream ForceKeyUnit"
    );
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
fn apply_encoder_control(
    encoder: &gstreamer::Element,
    scale_caps: &gstreamer::Element,
    sink: &gstreamer_app::AppSink,
    desired: EncoderControlDesired,
    status: &watch::Sender<EncoderControlStatus>,
    current_gop_sender: &watch::Sender<Option<EncodedGop>>,
    pending_force_keyframe_revision: &mut Option<u64>,
    pending_resolution: &mut Option<PendingResolutionTransition>,
) -> Result<()> {
    let current = *status.borrow();
    if let Some(bitrate) = desired.bitrate
        && current
            .applied_bitrate_revision
            .is_none_or(|revision| bitrate.revision > revision)
    {
        encoder.set_property("bitrate", bitrate.kbps);
        let readback = encoder.property::<u32>("bitrate");
        ensure!(
            readback == bitrate.kbps,
            "encoder bitrate readback mismatch: requested {} kbps, observed {readback} kbps",
            bitrate.kbps
        );
        status.send_modify(|status| {
            status.applied_bitrate_revision = Some(bitrate.revision);
            status.applied_bitrate_kbps = Some(readback);
        });
    }

    let mut keyframe_requested = false;
    if let Some(resolution) = desired.resolution
        && current
            .requested_resolution_revision
            .is_none_or(|requested| resolution.revision > requested)
    {
        apply_resolution_caps(scale_caps, resolution)?;
        current_gop_sender.send_replace(None);
        push_configured_keyframe_request(sink)?;
        keyframe_requested = true;
        *pending_resolution = Some(PendingResolutionTransition {
            target: resolution,
            deadline: Instant::now() + RESOLUTION_APPLY_TIMEOUT,
        });
        status.send_modify(|status| {
            status.requested_resolution_revision = Some(resolution.revision);
        });
    }

    if let Some(revision) = desired.force_keyframe_revision
        && current
            .requested_force_keyframe_revision
            .is_none_or(|requested| revision > requested)
    {
        if !keyframe_requested {
            push_configured_keyframe_request(sink)?;
        }
        *pending_force_keyframe_revision = Some(revision);
        status.send_modify(|status| {
            status.requested_force_keyframe_revision = Some(revision);
        });
    }

    status.send_modify(|status| status.latest_revision = desired.revision);
    Ok(())
}

#[cfg(any(test, all(target_os = "linux", feature = "in-process-gstreamer")))]
fn acknowledge_force_keyframe_if_configured_idr(
    configured_idr: bool,
    pending_revision: &mut Option<u64>,
    status: &watch::Sender<EncoderControlStatus>,
) {
    if configured_idr && let Some(revision) = pending_revision.take() {
        status.send_modify(|status| {
            status.acknowledged_force_keyframe_revision = Some(revision);
        });
    }
}

fn publish_encoded_frame(
    sender: &watch::Sender<Option<EncodedFrame>>,
    current_gop_sender: &watch::Sender<Option<EncodedGop>>,
    frame: EncodedFrame,
    max_gop_frames: usize,
    max_gop_bytes: usize,
) {
    if !current_gop_sender.is_closed() {
        current_gop_sender.send_if_modified(|current_gop| {
            update_current_gop(current_gop, frame.clone(), max_gop_frames, max_gop_bytes)
        });
    }
    sender.send_replace(Some(frame.clone()));
}

fn update_current_gop(
    current_gop: &mut Option<EncodedGop>,
    frame: EncodedFrame,
    max_gop_frames: usize,
    max_gop_bytes: usize,
) -> bool {
    let independently_decodable = frame.keyframe && frame.codec_config;
    if independently_decodable {
        let payload_bytes = frame.data.len();
        if max_gop_frames == 0 || payload_bytes > max_gop_bytes {
            return current_gop.take().is_some();
        }
        *current_gop = Some(EncodedGop {
            frames: vec![frame],
            payload_bytes,
        });
        return true;
    }

    let Some(gop) = current_gop.as_mut() else {
        return false;
    };
    let contiguous = gop
        .frames
        .last()
        .and_then(|previous| previous.sequence.checked_add(1))
        == Some(frame.sequence);
    let payload_bytes = gop.payload_bytes.saturating_add(frame.data.len());
    if !contiguous || gop.frames.len() >= max_gop_frames || payload_bytes > max_gop_bytes {
        *current_gop = None;
        return true;
    }

    gop.frames.push(frame);
    gop.payload_bytes = payload_bytes;
    true
}

pub(crate) async fn log_stderr_chunks<R>(mut stderr: R, target: &'static str)
where
    R: AsyncRead + Unpin,
{
    const LOG_INTERVAL: Duration = Duration::from_secs(1);
    const MAX_LOG_BYTES_PER_INTERVAL: usize = 16 * 1024;

    let mut chunk = [0_u8; 4096];
    let mut interval_started = tokio::time::Instant::now();
    let mut logged_bytes = 0_usize;
    let mut suppressed_bytes = 0_u64;
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) => return,
            Ok(count) => {
                if interval_started.elapsed() >= LOG_INTERVAL {
                    if suppressed_bytes > 0 {
                        warn!(
                            target: "capture-process",
                            process = target,
                            suppressed_bytes,
                            "capture diagnostics rate limited"
                        );
                    }
                    interval_started = tokio::time::Instant::now();
                    logged_bytes = 0;
                    suppressed_bytes = 0;
                }
                if logged_bytes.saturating_add(count) <= MAX_LOG_BYTES_PER_INTERVAL {
                    warn!(target: "capture-process", process = target, "{}", String::from_utf8_lossy(&chunk[..count]).trim_end());
                    logged_bytes = logged_bytes.saturating_add(count);
                } else {
                    suppressed_bytes = suppressed_bytes.saturating_add(count as u64);
                }
            }
            Err(error) => {
                warn!(target: "capture-process", process = target, %error, "failed reading stderr");
                return;
            }
        }
    }
}

fn gamescope_config(config: &HostConfig) -> Result<&GamescopePipewireConfig> {
    config
        .gamescope_pipewire
        .as_ref()
        .context("gamescope_pipewire configuration is missing")
}

fn gamescope_pipeline_args(
    config: &HostConfig,
    target_object: &str,
    video_mode: CaptureVideoMode,
) -> Result<Vec<OsString>> {
    let pipewire = gamescope_config(config)?;
    ensure!(
        !target_object.is_empty()
            && target_object.len() <= 32
            && target_object.bytes().all(|byte| byte.is_ascii_digit()),
        "resolved PipeWire object.serial is invalid"
    );
    let caps = format!(
        "video/x-raw,format=NV12,width={},height={},framerate={}/1",
        video_mode.width, video_mode.height, video_mode.framerate
    );
    let encoded_caps = "video/x-h264,stream-format=byte-stream,alignment=au";
    let mut args = vec![
        "--quiet".to_owned(),
        "pipewiresrc".to_owned(),
        "do-timestamp=true".to_owned(),
        // Keep Gamescope's negotiated producer pool bounded at its preferred
        // four buffers, but copy each delivered frame into GStreamer's own
        // buffer before downstream conversion/encoding can retain it. This
        // returns producer buffers promptly instead of starving Gamescope at
        // vblank. `use-bufferpool=false` is the supported replacement for the
        // deprecated `always-copy=true` spelling.
        "min-buffers=1".to_owned(),
        "max-buffers=4".to_owned(),
        "use-bufferpool=false".to_owned(),
        format!("target-object={target_object}"),
        "!".to_owned(),
        "queue".to_owned(),
        "max-size-buffers=1".to_owned(),
        "max-size-bytes=0".to_owned(),
        "max-size-time=0".to_owned(),
        "leaky=downstream".to_owned(),
        "!".to_owned(),
        "videorate".to_owned(),
        // Gamescope is damage-driven. Never manufacture replacement frames:
        // doing so can turn an irregular live source into stale catch-up work.
        "drop-only=true".to_owned(),
        format!("max-rate={}", video_mode.framerate),
        "!".to_owned(),
        "videoconvert".to_owned(),
        "!".to_owned(),
        "videoscale".to_owned(),
        "!".to_owned(),
        caps,
        "!".to_owned(),
        pipewire.vaapi_encoder.clone(),
    ];
    args.push(format!(
        "rate-control={}",
        match pipewire.rate_control {
            VaapiRateControl::Cbr => "cbr",
            VaapiRateControl::Cqp => "cqp",
        }
    ));
    match pipewire.rate_control {
        VaapiRateControl::Cbr => args.push(format!(
            "bitrate={}",
            pipewire
                .bitrate_kbps
                .context("CBR bitrate is missing after validation")?
        )),
        VaapiRateControl::Cqp => {
            let quantizer = pipewire
                .quantizer
                .context("CQP quantizer is missing after validation")?;
            args.push(format!("qpi={quantizer}"));
            args.push(format!("qpp={quantizer}"));
        }
    }
    args.extend([
        "target-usage=7".to_owned(),
        format!(
            "key-int-max={}",
            interactive_gop_frames(video_mode.framerate)
        ),
        "b-frames=0".to_owned(),
        "ref-frames=1".to_owned(),
        "aud=true".to_owned(),
        "!".to_owned(),
        "h264parse".to_owned(),
        "config-interval=-1".to_owned(),
        "!".to_owned(),
        encoded_caps.to_owned(),
        "!".to_owned(),
        "fdsink".to_owned(),
        "fd=1".to_owned(),
        // Gamescope's PipeWire timestamps can advance in larger jumps than
        // the negotiated frame duration. Clocking this terminal sink makes it
        // wait on those producer timestamps while the one-frame leaky queue
        // drops fresh motion. Videorate already caps the live stream at the
        // configured maximum, so drain encoded access units immediately.
        "sync=false".to_owned(),
    ]);
    Ok(args.into_iter().map(OsString::from).collect())
}

fn interactive_gop_frames(framerate: u32) -> u32 {
    // A dropped encoded reference requires the client to wait for the next
    // independently decodable IDR. Bound that recovery to roughly 500 ms until
    // the in-process encoder can service explicit keyframe requests.
    framerate.div_ceil(2).max(1)
}

pub(crate) async fn resolve_pipewire_node_by_properties(
    pw_dump_path: &std::path::Path,
    node_name: &str,
    media_class: &str,
    match_properties: &std::collections::BTreeMap<String, String>,
) -> Result<String> {
    let mut command = Command::new(pw_dump_path);
    command.env("LC_ALL", "C");
    let output = run_bounded_command(command, MAX_PW_DUMP_OUTPUT).await?;
    ensure!(
        output.status.success(),
        "pw-dump failed: {}",
        diagnostic(&output.stderr)
    );
    resolve_pipewire_node_exact(&output.stdout, node_name, media_class, match_properties)
}

fn resolve_pipewire_node(output: &[u8], config: &HostConfig) -> Result<GamescopePreflight> {
    let pipewire = gamescope_config(config)?;
    let target_object = resolve_pipewire_node_exact(
        output,
        &pipewire.node_name,
        &pipewire.media_class,
        &pipewire.match_properties,
    )?;
    let pointer_surface_dimensions = resolve_pointer_surface_dimensions(
        output,
        &target_object,
        &pipewire.node_name,
        &pipewire.media_class,
        &pipewire.match_properties,
    )?;
    let video_mode = CaptureVideoMode::resolve(config, pointer_surface_dimensions)?;
    Ok(GamescopePreflight {
        target_object,
        pointer_surface_dimensions,
        video_mode,
    })
}

pub(crate) fn resolve_pipewire_node_exact(
    output: &[u8],
    node_name: &str,
    media_class: &str,
    match_properties: &std::collections::BTreeMap<String, String>,
) -> Result<String> {
    let objects: Value = serde_json::from_slice(output).context("parsing bounded pw-dump JSON")?;
    let objects = objects
        .as_array()
        .context("pw-dump output is not an array")?;
    let mut matches = Vec::new();

    for object in objects {
        if object.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Node") {
            continue;
        }
        let Some(properties) = object
            .get("info")
            .and_then(|info| info.get("props"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        if !pipewire_node_matches(properties, node_name, media_class, match_properties) {
            continue;
        }
        let serial = property_string(properties.get("object.serial"))
            .context("matching PipeWire node has no object.serial")?;
        ensure!(
            !serial.is_empty() && serial.bytes().all(|byte| byte.is_ascii_digit()),
            "matching PipeWire node has invalid object.serial"
        );
        matches.push(serial.into_owned());
    }

    ensure!(
        matches.len() == 1,
        "expected exactly one PipeWire node matching node.name={:?}, media.class={:?}, and configured properties; found {}",
        node_name,
        media_class,
        matches.len()
    );
    Ok(matches.remove(0))
}

fn resolve_pointer_surface_dimensions(
    output: &[u8],
    target_object: &str,
    node_name: &str,
    media_class: &str,
    match_properties: &std::collections::BTreeMap<String, String>,
) -> Result<PointerSurfaceDimensions> {
    let objects: Value = serde_json::from_slice(output).context("parsing bounded pw-dump JSON")?;
    let objects = objects
        .as_array()
        .context("pw-dump output is not an array")?;
    let mut matched_node_count = 0_usize;
    let mut native_size = None;

    for object in objects {
        if object.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Node") {
            continue;
        }
        let Some(info) = object.get("info").and_then(Value::as_object) else {
            continue;
        };
        let Some(properties) = info.get("props").and_then(Value::as_object) else {
            continue;
        };
        if !pipewire_node_matches(properties, node_name, media_class, match_properties)
            || property_string(properties.get("object.serial")).as_deref() != Some(target_object)
        {
            continue;
        }
        matched_node_count += 1;
        let formats = info
            .get("params")
            .and_then(|params| params.get("EnumFormat"))
            .and_then(Value::as_array)
            .context("matching Gamescope PipeWire node has no EnumFormat array")?;
        for format in formats {
            if format.get("mediaType").and_then(Value::as_str) != Some("video")
                || format.get("mediaSubtype").and_then(Value::as_str) != Some("raw")
            {
                continue;
            }
            let size = format
                .get("size")
                .context("Gamescope raw EnumFormat has no native size")?;
            let size = size.get("default").unwrap_or(size);
            let width = size
                .get("width")
                .and_then(Value::as_u64)
                .and_then(|value| u16::try_from(value).ok())
                .context("Gamescope raw EnumFormat native width is not a u16")?;
            let height = size
                .get("height")
                .and_then(Value::as_u64)
                .and_then(|value| u16::try_from(value).ok())
                .context("Gamescope raw EnumFormat native height is not a u16")?;
            let dimensions = PointerSurfaceDimensions::new(width, height)
                .context("Gamescope raw EnumFormat native size is outside pointer bounds")?;
            if native_size.is_some_and(|existing| existing != dimensions) {
                bail!("matching Gamescope PipeWire node advertises ambiguous native sizes");
            }
            native_size = Some(dimensions);
        }
    }

    ensure!(
        matched_node_count == 1,
        "expected exactly one matched Gamescope PipeWire node while resolving its native size; found {matched_node_count}"
    );
    native_size.context("matching Gamescope PipeWire node has no raw native EnumFormat size")
}

fn pipewire_node_matches(
    properties: &serde_json::Map<String, Value>,
    node_name: &str,
    media_class: &str,
    match_properties: &std::collections::BTreeMap<String, String>,
) -> bool {
    property_string(properties.get("node.name")).as_deref() == Some(node_name)
        && property_string(properties.get("media.class")).as_deref() == Some(media_class)
        && match_properties.iter().all(|(key, expected)| {
            property_string(properties.get(key)).is_some_and(|actual| actual == *expected)
        })
}

fn property_string(value: Option<&Value>) -> Option<Cow<'_, str>> {
    match value? {
        Value::String(value) => Some(Cow::Borrowed(value)),
        Value::Number(value) => Some(Cow::Owned(value.to_string())),
        _ => None,
    }
}

fn validate_encoder_inspection(output: &[u8], config: &GamescopePipewireConfig) -> Result<()> {
    let output = std::str::from_utf8(output).context("gst-inspect output is not UTF-8")?;
    let lines: Vec<&str> = output.lines().collect();
    let device_block = gst_property_block(&lines, "device-path")
        .context("configured VA encoder does not report a device-path property")?;
    let expected = config.vaapi_render_node.to_string_lossy();
    let expected_default = format!("Default: \"{expected}\"");
    ensure!(
        device_block.contains(&expected_default),
        "configured VA encoder {:?} does not report expected device-path {}",
        config.vaapi_encoder,
        config.vaapi_render_node.display()
    );
    for property in [
        "aud",
        "b-frames",
        "key-int-max",
        "rate-control",
        "ref-frames",
        "target-usage",
    ] {
        let block = gst_property_block(&lines, property).with_context(|| {
            format!(
                "configured VA encoder {:?} has no {property} property",
                config.vaapi_encoder
            )
        })?;
        ensure!(
            block.contains("writable") || block.contains("Writable"),
            "configured VA encoder {:?} property {property} is not writable",
            config.vaapi_encoder
        );
    }
    let rate_control =
        gst_property_block(&lines, "rate-control").expect("rate-control block was checked above");
    let expected_rate_control = match config.rate_control {
        VaapiRateControl::Cbr => "cbr",
        VaapiRateControl::Cqp => "cqp",
    };
    ensure!(
        rate_control.contains(expected_rate_control),
        "configured VA encoder {:?} does not advertise {expected_rate_control} rate control",
        config.vaapi_encoder
    );
    let mode_properties: &[&str] = match config.rate_control {
        VaapiRateControl::Cbr => &["bitrate"],
        VaapiRateControl::Cqp => &["qpi", "qpp"],
    };
    for property in mode_properties {
        let block = gst_property_block(&lines, property).with_context(|| {
            format!(
                "configured VA encoder {:?} has no {property} property",
                config.vaapi_encoder
            )
        })?;
        ensure!(
            block.contains("writable") || block.contains("Writable"),
            "configured VA encoder {:?} property {property} is not writable",
            config.vaapi_encoder
        );
    }
    Ok(())
}

fn gst_property_block(lines: &[&str], property: &str) -> Option<String> {
    let start = lines.iter().position(|line| {
        line.strip_prefix("  ").is_some_and(|line| {
            line.starts_with(property) && line[property.len()..].starts_with(' ')
        })
    })?;
    let end = lines[start + 1..]
        .iter()
        .position(|line| {
            line.strip_prefix("  ")
                .is_some_and(|line| !line.starts_with(' ') && line.split_once(" : ").is_some())
        })
        .map_or(lines.len(), |offset| start + 1 + offset);
    Some(lines[start..end].join("\n"))
}

pub(crate) fn validate_executable(name: &str, path: &std::path::Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting gamescope_pipewire.{name} at {}", path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "gamescope_pipewire.{name} must not be a symlink"
    );
    ensure!(
        metadata.is_file(),
        "gamescope_pipewire.{name} is not a file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        ensure!(
            metadata.permissions().mode() & 0o111 != 0,
            "gamescope_pipewire.{name} is not executable"
        );
        ensure!(
            metadata.permissions().mode() & 0o022 == 0,
            "gamescope_pipewire.{name} must not be writable by group or other users"
        );
        let owner = metadata.uid();
        let effective_user = unsafe { libc::geteuid() };
        ensure!(
            owner == 0 || owner == effective_user,
            "gamescope_pipewire.{name} must be owned by root or the current user"
        );
    }
    Ok(())
}

pub(crate) async fn preflight_gstreamer_element(
    gst_inspect_path: &std::path::Path,
    element: &str,
) -> Result<()> {
    let mut command = Command::new(gst_inspect_path);
    command.arg(element).env("LC_ALL", "C");
    let output = run_bounded_command(command, MAX_INSPECT_OUTPUT).await?;
    ensure!(
        output.status.success(),
        "GStreamer element {element:?} is unavailable: {}",
        diagnostic(&output.stderr)
    );
    Ok(())
}

fn validate_amd_render_node(path: &std::path::Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting VAAPI render node {}", path.display()))?;
    #[cfg(unix)]
    ensure!(
        metadata.file_type().is_char_device(),
        "VAAPI render node {} is not a character device",
        path.display()
    );
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| {
            format!(
                "opening VAAPI render node {} read/write as the current user",
                path.display()
            )
        })?;
    #[cfg(target_os = "linux")]
    {
        let name = path
            .file_name()
            .context("VAAPI render node has no file name")?;
        let driver = fs::canonicalize(
            std::path::Path::new("/sys/class/drm")
                .join(name)
                .join("device/driver"),
        )
        .with_context(|| format!("resolving kernel driver for {}", path.display()))?;
        ensure!(
            driver.file_name().and_then(|name| name.to_str()) == Some("amdgpu"),
            "VAAPI render node {} is not backed by amdgpu",
            path.display()
        );
    }
    Ok(())
}

struct BoundedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn run_bounded_command(mut command: Command, stdout_limit: usize) -> Result<BoundedOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let program = command.as_std().get_program().to_owned();
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {}", program.to_string_lossy()))?;
    let stdout = child.stdout.take().context("child stdout was not piped")?;
    let stderr = child.stderr.take().context("child stderr was not piped")?;

    let collect = async {
        let (stdout, stderr) = tokio::join!(
            read_bounded(stdout, stdout_limit),
            read_bounded(stderr, MAX_DIAGNOSTIC_OUTPUT)
        );
        let status = child
            .wait()
            .await
            .context("waiting for preflight command")?;
        Ok::<_, anyhow::Error>(BoundedOutput {
            status,
            stdout: stdout?,
            stderr: stderr?,
        })
    };
    match tokio::time::timeout(PREFLIGHT_TIMEOUT, collect).await {
        Ok(result) => result,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            bail!(
                "preflight command {} exceeded {} seconds",
                program.to_string_lossy(),
                PREFLIGHT_TIMEOUT.as_secs()
            )
        }
    }
}

async fn read_bounded<R>(reader: R, limit: usize) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await
        .context("reading bounded command output")?;
    ensure!(
        bytes.len() <= limit,
        "command output exceeded {limit} bytes"
    );
    Ok(bytes)
}

fn diagnostic(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

#[derive(Default)]
struct AnnexBAccessUnitParser {
    buffer: Vec<u8>,
}

impl AnnexBAccessUnitParser {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.buffer.extend_from_slice(bytes);
        ensure!(
            self.buffer.len() <= MAX_ENCODE_BUFFER,
            "encoded stream exceeded {MAX_ENCODE_BUFFER} bytes without a frame boundary"
        );

        let mut frames = Vec::new();
        while let Some(first) = find_h264_aud(&self.buffer, 0) {
            if first > 0 {
                self.buffer.drain(..first);
            }
            let Some(next) = find_h264_aud(&self.buffer, 4) else {
                break;
            };
            let frame: Vec<u8> = self.buffer.drain(..next).collect();
            if !frame.is_empty() {
                frames.push(frame);
            }
        }
        Ok(frames)
    }
}

fn find_h264_aud(data: &[u8], start: usize) -> Option<usize> {
    let mut index = start;
    while index + 4 <= data.len() {
        let nal_offset = if data.get(index..index + 4) == Some(&[0, 0, 0, 1]) {
            Some(index + 4)
        } else if data.get(index..index + 3) == Some(&[0, 0, 1]) {
            Some(index + 3)
        } else {
            None
        };
        if let Some(nal_offset) = nal_offset
            && data.get(nal_offset).is_some_and(|byte| byte & 0x1f == 9)
        {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn is_h264_keyframe(data: &[u8]) -> bool {
    h264_nal_types(data).any(|nal_type| nal_type == 5)
}

fn has_h264_codec_config(data: &[u8]) -> bool {
    let mut has_sps = false;
    let mut has_pps = false;
    for nal_type in h264_nal_types(data) {
        has_sps |= nal_type == 7;
        has_pps |= nal_type == 8;
    }
    has_sps && has_pps
}

fn h264_nal_types(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let mut index = 0;
    std::iter::from_fn(move || {
        while index + 4 <= data.len() {
            let nal_offset = if data.get(index..index + 4) == Some(&[0, 0, 0, 1]) {
                Some(index + 4)
            } else if data.get(index..index + 3) == Some(&[0, 0, 1]) {
                Some(index + 3)
            } else {
                None
            };
            index += 1;
            if let Some(nal_offset) = nal_offset
                && let Some(byte) = data.get(nal_offset)
            {
                return Some(byte & 0x1f);
            }
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GamescopeEncoderBackend, InputMode, VideoSource};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn gamescope_config() -> HostConfig {
        HostConfig {
            identity_path: PathBuf::from("/tmp/host.key"),
            state_path: PathBuf::from("/tmp/state"),
            source: VideoSource::GamescopePipewire,
            width: Some(1280),
            height: Some(800),
            framerate: 60,
            codec: "h264".into(),
            input_mode: InputMode::Disabled,
            uinput: None,
            ffmpeg_path: PathBuf::from("/usr/bin/ffmpeg"),
            gamescope_pipewire: Some(GamescopePipewireConfig {
                encoder_backend: GamescopeEncoderBackend::ExternalGstLaunch,
                xwayland_display: None,
                node_name: "gamescope".into(),
                media_class: "Video/Source".into(),
                match_properties: BTreeMap::from([(
                    "device.bus-path".into(),
                    "pci-0000:04:00.0".into(),
                )]),
                pw_dump_path: PathBuf::from("/usr/bin/pw-dump"),
                gst_launch_path: PathBuf::from("/usr/bin/gst-launch-1.0"),
                gst_inspect_path: PathBuf::from("/usr/bin/gst-inspect-1.0"),
                vaapi_encoder: "vah264enc".into(),
                vaapi_render_node: PathBuf::from("/dev/dri/renderD128"),
                rate_control: VaapiRateControl::Cbr,
                bitrate_kbps: Some(12_000),
                quantizer: None,
            }),
            audio: None,
        }
    }

    fn configured_video_mode() -> CaptureVideoMode {
        CaptureVideoMode {
            width: 1_280,
            height: 800,
            framerate: 60,
        }
    }

    #[tokio::test]
    async fn encoder_control_coalesces_latest_state_and_acknowledges_only_configured_idr() {
        let (control, mut desired, status) = EncoderControl::new();

        let superseded_bitrate_revision = control.request_bitrate_kbps(4_000).unwrap();
        let bitrate_revision = control.request_bitrate_kbps(8_000).unwrap();
        let superseded_resolution_revision = control.request_resolution(960, 600).unwrap();
        let resolution_revision = control.request_resolution(640, 400).unwrap();
        let force_keyframe_revision = control.request_force_keyframe().unwrap();

        assert!(superseded_bitrate_revision < bitrate_revision);
        assert!(bitrate_revision < superseded_resolution_revision);
        assert!(superseded_resolution_revision < resolution_revision);
        assert!(resolution_revision < force_keyframe_revision);
        let latest = *desired.borrow_and_update();
        assert_eq!(latest.revision, force_keyframe_revision);
        assert_eq!(
            latest.bitrate,
            Some(RevisedBitrate {
                revision: bitrate_revision,
                kbps: 8_000,
            })
        );
        assert_eq!(
            latest.resolution,
            Some(RevisedResolution {
                revision: resolution_revision,
                width: 640,
                height: 400,
            })
        );
        assert_eq!(
            latest.force_keyframe_revision,
            Some(force_keyframe_revision)
        );

        status.send_modify(|status| {
            status.latest_revision = latest.revision;
            status.applied_bitrate_revision = Some(bitrate_revision);
            status.applied_bitrate_kbps = Some(8_000);
            status.requested_resolution_revision = Some(resolution_revision);
            status.applied_resolution_revision = Some(resolution_revision);
            status.applied_width = Some(640);
            status.applied_height = Some(400);
            status.requested_force_keyframe_revision = Some(force_keyframe_revision);
        });
        let mut pending = Some(force_keyframe_revision);
        acknowledge_force_keyframe_if_configured_idr(false, &mut pending, &status);
        assert_eq!(pending, Some(force_keyframe_revision));
        assert_eq!(control.status().acknowledged_force_keyframe_revision, None);

        acknowledge_force_keyframe_if_configured_idr(true, &mut pending, &status);
        assert_eq!(pending, None);
        let applied = control
            .wait_for_bitrate_applied(bitrate_revision, Duration::from_millis(10))
            .await
            .unwrap();
        assert_eq!(applied.applied_bitrate_kbps, Some(8_000));
        let applied_resolution = control
            .wait_for_resolution_applied(resolution_revision, 640, 400, Duration::from_millis(10))
            .await
            .unwrap();
        assert_eq!(applied_resolution.applied_width, Some(640));
        assert_eq!(applied_resolution.applied_height, Some(400));
        assert!(
            control
                .wait_for_resolution_applied(
                    superseded_resolution_revision,
                    960,
                    600,
                    Duration::from_millis(10),
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("superseded")
        );
        let acknowledged = control
            .wait_for_force_keyframe_acknowledged(
                force_keyframe_revision,
                Duration::from_millis(10),
            )
            .await
            .unwrap();
        assert_eq!(
            acknowledged.acknowledged_force_keyframe_revision,
            Some(force_keyframe_revision)
        );
        assert!(control.request_bitrate_kbps(0).is_err());
        assert!(control.request_resolution(0, 400).is_err());
        assert!(control.request_resolution(641, 400).is_err());
        assert!(
            control
                .request_bitrate_kbps(MAX_ENCODER_BITRATE_KBPS + 1)
                .is_err()
        );

        let (closed_control, _desired, closed_status) = EncoderControl::new();
        let closed_revision = closed_control.request_force_keyframe().unwrap();
        drop(closed_status);
        assert!(
            closed_control
                .wait_for_force_keyframe_acknowledged(closed_revision, Duration::from_millis(10))
                .await
                .unwrap_err()
                .to_string()
                .contains("status closed")
        );
    }

    #[tokio::test]
    async fn recovery_waiter_accepts_a_newer_configured_idr_revision() {
        let (control, _desired, status) = EncoderControl::new();
        let recovery_revision = control.request_force_keyframe().unwrap();
        let newer_revision = control.request_force_keyframe().unwrap();
        assert!(newer_revision > recovery_revision);
        status.send_modify(|status| {
            status.requested_force_keyframe_revision = Some(newer_revision);
            status.acknowledged_force_keyframe_revision = Some(newer_revision);
        });

        let acknowledged = control
            .wait_for_recovery_keyframe_acknowledged(recovery_revision, Duration::from_millis(10))
            .await
            .unwrap();
        assert_eq!(
            acknowledged.acknowledged_force_keyframe_revision,
            Some(newer_revision)
        );
        assert!(
            control
                .wait_for_force_keyframe_acknowledged(recovery_revision, Duration::from_millis(10),)
                .await
                .unwrap_err()
                .to_string()
                .contains("superseded")
        );
    }

    #[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires GStreamer x264 and app plugins"]
    async fn in_process_gstreamer_x264_smoke() {
        let description = "videotestsrc is-live=true pattern=ball \
            ! queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream \
            ! videoconvert ! videoscale \
            ! capsfilter name=sigil_scale_caps caps=video/x-raw,width=320,height=180,framerate=30/1 caps-change-mode=delayed \
            ! x264enc name=sigil_encoder bitrate=2000 tune=zerolatency speed-preset=ultrafast key-int-max=15 bframes=0 byte-stream=true aud=true \
            ! h264parse config-interval=-1 \
            ! video/x-h264,stream-format=byte-stream,alignment=au \
            ! appsink name=sigil_sink max-buffers=1 drop=false sync=false"
            .to_owned();
        let mut source =
            spawn_in_process_pipeline(description, SessionClock::start(), 15, None, None, 320, 180)
                .unwrap();
        let control = source
            .encoder_control
            .clone()
            .expect("x264 source has no encoder control");

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                source.frames.changed().await.unwrap();
                if source
                    .frames
                    .borrow()
                    .as_ref()
                    .is_some_and(|frame| frame.keyframe && frame.codec_config)
                {
                    break;
                }
            }
        })
        .await
        .expect("x264 did not publish an initial configured IDR");

        let bitrate_revision = control.request_bitrate_kbps(1_500).unwrap();
        let force_keyframe_revision = control.request_force_keyframe().unwrap();
        let bitrate_status = control
            .wait_for_bitrate_applied(bitrate_revision, Duration::from_secs(10))
            .await
            .expect("x264 did not apply the bitrate revision");
        assert_eq!(bitrate_status.applied_bitrate_kbps, Some(1_500));
        control
            .wait_for_force_keyframe_acknowledged(force_keyframe_revision, Duration::from_secs(10))
            .await
            .expect("x264 did not acknowledge a configured forced IDR");
        let before_resolution_sequence = source
            .current_gop
            .borrow()
            .as_ref()
            .and_then(|gop| gop.frames.first())
            .map(|frame| frame.sequence)
            .expect("x264 has no configured GOP before resolution transition");

        let reduced_revision = control.request_resolution(256, 144).unwrap();
        control
            .wait_for_resolution_applied(reduced_revision, 256, 144, Duration::from_secs(10))
            .await
            .expect("x264 did not apply the reduced resolution");
        let reduced = source
            .current_gop
            .borrow()
            .as_ref()
            .and_then(|gop| gop.frames.first())
            .cloned()
            .expect("x264 has no reduced configured GOP");
        assert_eq!((reduced.width, reduced.height), (256, 144));
        assert!(reduced.keyframe && reduced.codec_config && reduced.discontinuity);
        assert!(reduced.sequence > before_resolution_sequence);

        let restored_revision = control.request_resolution(320, 180).unwrap();
        control
            .wait_for_resolution_applied(restored_revision, 320, 180, Duration::from_secs(10))
            .await
            .expect("x264 did not restore the native resolution");
        let restored = source
            .current_gop
            .borrow()
            .as_ref()
            .and_then(|gop| gop.frames.first())
            .cloned()
            .expect("x264 has no restored configured GOP");
        assert_eq!((restored.width, restored.height), (320, 180));
        assert!(restored.keyframe && restored.codec_config && restored.discontinuity);
        assert!(restored.sequence > reduced.sequence);

        {
            let current_gop = source.current_gop.borrow();
            let gop = current_gop
                .as_ref()
                .expect("x264 did not retain a current GOP");
            assert!(gop.frames[0].keyframe && gop.frames[0].codec_config);
            assert!(gop.frames.len() <= 15);
        }
        let EncodedSource {
            frames,
            current_gop,
            task,
            ..
        } = source;
        drop(frames);
        drop(current_gop);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("x264 pipeline did not stop after its receivers closed")
            .expect("x264 pipeline task panicked")
            .expect("x264 pipeline returned an error");
    }

    #[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
    #[test]
    fn in_process_pipeline_drops_only_before_encoding() {
        let mut config = gamescope_config();
        config.gamescope_pipewire.as_mut().unwrap().encoder_backend =
            GamescopeEncoderBackend::InProcessGstreamer;
        let description =
            gamescope_in_process_pipeline_description(&config, "1234", configured_video_mode())
                .unwrap();

        assert!(description.contains(
            "queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream"
        ));
        assert!(description.contains(
            "capsfilter name=sigil_scale_caps caps=video/x-raw,format=NV12,width=1280,height=800,framerate=60/1 caps-change-mode=delayed"
        ));
        assert!(
            description.contains("appsink name=sigil_sink max-buffers=1 drop=false sync=false")
        );
        assert!(!description.contains("appsink name=sigil_sink max-buffers=1 drop=true"));
    }

    #[test]
    fn splits_access_units_on_aud() {
        let first = [0, 0, 0, 1, 9, 0x10, 0, 0, 1, 5, 1, 2, 3];
        let second = [0, 0, 0, 1, 9, 0x10, 0, 0, 1, 1, 4, 5];
        let third = [0, 0, 1, 9, 0x10, 0, 0, 1, 1, 6, 7];
        let mut parser = AnnexBAccessUnitParser::default();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&first);
        bytes.extend_from_slice(&second);
        bytes.extend_from_slice(&third);
        let frames = parser.push(&bytes).unwrap();

        assert_eq!(frames, vec![first.to_vec(), second.to_vec()]);
    }

    #[test]
    fn detects_h264_keyframes() {
        assert!(is_h264_keyframe(&[0, 0, 0, 1, 5, 1, 2]));
        assert!(!is_h264_keyframe(&[0, 0, 1, 7, 1, 2]));
        assert!(!is_h264_keyframe(&[0, 0, 0, 1, 1, 1, 2]));
    }

    #[test]
    fn codec_config_requires_sps_and_pps() {
        assert!(has_h264_codec_config(&[
            0, 0, 0, 1, 7, 1, 2, 0, 0, 1, 8, 3, 4
        ]));
        assert!(!has_h264_codec_config(&[0, 0, 1, 7, 1, 2]));
        assert!(!has_h264_codec_config(&[0, 0, 1, 8, 1, 2]));
    }

    #[test]
    fn resolution_transition_suppresses_until_exact_configured_target_idr() {
        let transition = PendingResolutionTransition {
            target: RevisedResolution {
                revision: 7,
                width: 960,
                height: 600,
            },
            deadline: Instant::now() + Duration::from_secs(1),
        };
        assert!(transition.deadline > Instant::now());
        assert_eq!(
            classify_resolution_sample((1280, 800), Some(transition), (1280, 800), true).unwrap(),
            None
        );
        assert_eq!(
            classify_resolution_sample((1280, 800), Some(transition), (960, 600), false).unwrap(),
            None
        );
        assert_eq!(
            classify_resolution_sample((1280, 800), Some(transition), (960, 600), true).unwrap(),
            Some(true)
        );
        assert_eq!(
            classify_resolution_sample((960, 600), None, (960, 600), false).unwrap(),
            Some(false)
        );
        assert!(classify_resolution_sample((960, 600), None, (1280, 800), true).is_err());
    }

    #[test]
    fn publisher_carries_exact_resolution_and_discontinuity_into_new_gop() {
        let (frame_sender, frame_receiver) = watch::channel(None);
        let (current_gop_sender, current_gop_receiver) = watch::channel(None);
        let mut publisher = AccessUnitPublisher::new(
            SessionClock::start(),
            &frame_sender,
            &current_gop_sender,
            30,
            1280,
            800,
        );
        let configured_idr = vec![0, 0, 0, 1, 7, 1, 0, 0, 0, 1, 8, 1, 0, 0, 0, 1, 5, 1];

        assert!(
            publisher
                .publish_with_metadata(configured_idr, 960, 600, true)
                .unwrap()
        );
        let frame = frame_receiver.borrow().as_ref().unwrap().clone();
        assert_eq!((frame.width, frame.height), (960, 600));
        assert!(frame.discontinuity);
        let current_gop = current_gop_receiver.borrow();
        let first = &current_gop.as_ref().unwrap().frames[0];
        assert_eq!((first.width, first.height), (960, 600));
        assert!(first.keyframe && first.codec_config && first.discontinuity);
    }

    #[test]
    fn current_gop_retains_only_a_bounded_contiguous_decodable_chain() {
        let (frame_sender, frame_receiver) = watch::channel(None);
        let (current_gop_sender, current_gop_receiver) = watch::channel(None);
        let frame = |sequence, keyframe, codec_config, payload_len| EncodedFrame {
            sequence,
            width: 1280,
            height: 800,
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: Instant::now(),
            keyframe,
            codec_config,
            discontinuity: false,
            data: Arc::from(vec![sequence as u8; payload_len]),
        };

        for encoded in [
            frame(10, true, true, 1),
            frame(11, false, false, 1),
            frame(12, false, false, 1),
        ] {
            publish_encoded_frame(&frame_sender, &current_gop_sender, encoded, 4, 8);
        }

        assert_eq!(frame_receiver.borrow().as_ref().unwrap().sequence, 12);
        assert_eq!(
            current_gop_receiver
                .borrow()
                .as_ref()
                .unwrap()
                .frames
                .iter()
                .map(|frame| frame.sequence)
                .collect::<Vec<_>>(),
            vec![10, 11, 12]
        );

        publish_encoded_frame(
            &frame_sender,
            &current_gop_sender,
            frame(40, true, true, 1),
            4,
            8,
        );
        assert_eq!(
            current_gop_receiver.borrow().as_ref().unwrap().frames[0].sequence,
            40
        );

        publish_encoded_frame(
            &frame_sender,
            &current_gop_sender,
            frame(42, false, false, 1),
            4,
            8,
        );
        assert!(current_gop_receiver.borrow().is_none());

        publish_encoded_frame(
            &frame_sender,
            &current_gop_sender,
            frame(50, true, true, 3),
            4,
            4,
        );
        publish_encoded_frame(
            &frame_sender,
            &current_gop_sender,
            frame(51, false, false, 2),
            4,
            4,
        );
        assert!(current_gop_receiver.borrow().is_none());

        for encoded in [
            frame(60, true, true, 1),
            frame(61, false, false, 1),
            frame(62, false, false, 1),
        ] {
            publish_encoded_frame(&frame_sender, &current_gop_sender, encoded, 2, 8);
        }
        assert!(current_gop_receiver.borrow().is_none());
    }

    #[test]
    fn builds_bounded_low_latency_gamescope_pipeline() {
        let config = gamescope_config();
        let args: Vec<String> = gamescope_pipeline_args(&config, "1234", configured_video_mode())
            .unwrap()
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect();

        assert_eq!(args[0], "--quiet");
        assert!(
            args.windows(2)
                .any(|args| { args == ["pipewiresrc".to_owned(), "do-timestamp=true".to_owned()] })
        );
        assert!(args.contains(&"target-object=1234".to_owned()));
        assert!(args.contains(&"min-buffers=1".to_owned()));
        assert!(args.contains(&"max-buffers=4".to_owned()));
        assert!(args.contains(&"use-bufferpool=false".to_owned()));
        assert!(args.contains(&"max-size-buffers=1".to_owned()));
        assert!(args.contains(&"leaky=downstream".to_owned()));
        assert!(
            args.windows(2)
                .any(|args| { args == ["videorate".to_owned(), "drop-only=true".to_owned()] })
        );
        assert!(
            args.contains(
                &"video/x-raw,format=NV12,width=1280,height=800,framerate=60/1".to_owned()
            )
        );
        assert!(args.contains(&"vah264enc".to_owned()));
        assert!(args.contains(&"bitrate=12000".to_owned()));
        assert!(args.contains(&"rate-control=cbr".to_owned()));
        assert!(args.contains(&"target-usage=7".to_owned()));
        assert!(args.contains(&"key-int-max=30".to_owned()));
        assert!(args.contains(&"b-frames=0".to_owned()));
        assert!(args.contains(&"aud=true".to_owned()));
        assert!(args.contains(&"config-interval=-1".to_owned()));
        assert!(args.contains(&"video/x-h264,stream-format=byte-stream,alignment=au".to_owned()));
        assert!(args.ends_with(&[
            "fdsink".to_owned(),
            "fd=1".to_owned(),
            "sync=false".to_owned()
        ]));
        let videorate = args.iter().position(|arg| arg == "videorate").unwrap();
        let queue = args.iter().position(|arg| arg == "queue").unwrap();
        let videoconvert = args.iter().position(|arg| arg == "videoconvert").unwrap();
        let videoscale = args.iter().position(|arg| arg == "videoscale").unwrap();
        assert!(queue < videorate && videorate < videoconvert && videoconvert < videoscale);
        assert!(gamescope_pipeline_args(&config, "gamescope", configured_video_mode()).is_err());
    }

    #[test]
    fn builds_pipeline_for_resolved_non_fixture_mode() {
        let config = gamescope_config();
        let mode = CaptureVideoMode {
            width: 1_920,
            height: 1_080,
            framerate: 72,
        };
        let args: Vec<String> = gamescope_pipeline_args(&config, "1234", mode)
            .unwrap()
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect();

        assert!(
            args.contains(
                &"video/x-raw,format=NV12,width=1920,height=1080,framerate=72/1".to_owned()
            )
        );
        assert!(args.contains(&"max-rate=72".to_owned()));
        assert!(args.contains(&"key-int-max=36".to_owned()));
    }

    #[test]
    fn recovery_gop_is_bounded_to_half_a_second() {
        assert_eq!(interactive_gop_frames(60), 30);
        assert_eq!(interactive_gop_frames(59), 30);
        assert_eq!(interactive_gop_frames(1), 1);
    }

    #[test]
    fn builds_explicit_low_power_cqp_pipeline() {
        let mut config = gamescope_config();
        let pipewire = config.gamescope_pipewire.as_mut().unwrap();
        pipewire.vaapi_encoder = "vah264lpenc".into();
        pipewire.rate_control = VaapiRateControl::Cqp;
        pipewire.bitrate_kbps = None;
        pipewire.quantizer = Some(24);
        config.validate().unwrap();

        let args: Vec<String> = gamescope_pipeline_args(&config, "1234", configured_video_mode())
            .unwrap()
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect();
        assert!(args.contains(&"vah264lpenc".to_owned()));
        assert!(args.contains(&"rate-control=cqp".to_owned()));
        assert!(args.contains(&"qpi=24".to_owned()));
        assert!(args.contains(&"qpp=24".to_owned()));
        assert!(!args.iter().any(|arg| arg.starts_with("bitrate=")));
    }

    #[test]
    fn resolves_exactly_one_pipewire_node_by_properties() {
        let config = gamescope_config();
        let dump = br#"[
          {"type":"PipeWire:Interface:Node","info":{"props":{
            "node.name":"gamescope","media.class":"Video/Source",
            "device.bus-path":"pci-0000:04:00.0","object.serial":731
          },"params":{"EnumFormat":[
            {"mediaType":"video","mediaSubtype":"raw","format":"BGRx",
             "size":{"width":2560,"height":1600}},
            {"mediaType":"video","mediaSubtype":"raw","format":"NV12",
             "size":{"width":2560,"height":1600}}
          ]}}},
          {"type":"PipeWire:Interface:Node","info":{"props":{
            "node.name":"gamescope","media.class":"Audio/Source",
            "device.bus-path":"pci-0000:04:00.0","object.serial":732
          }}}
        ]"#;
        assert_eq!(
            resolve_pipewire_node(dump, &config).unwrap(),
            GamescopePreflight {
                target_object: "731".into(),
                pointer_surface_dimensions: PointerSurfaceDimensions::new(2_560, 1_600).unwrap(),
                video_mode: configured_video_mode(),
            }
        );

        let duplicate = br#"[
          {"type":"PipeWire:Interface:Node","info":{"props":{
            "node.name":"gamescope","media.class":"Video/Source",
            "device.bus-path":"pci-0000:04:00.0","object.serial":731
          }}},
          {"type":"PipeWire:Interface:Node","info":{"props":{
            "node.name":"gamescope","media.class":"Video/Source",
            "device.bus-path":"pci-0000:04:00.0","object.serial":"732"
          }}}
        ]"#;
        assert!(resolve_pipewire_node(duplicate, &config).is_err());

        let mut native_config = config.clone();
        native_config.width = None;
        native_config.height = None;
        let native = resolve_pipewire_node(dump, &native_config).unwrap();
        assert_eq!(
            native.video_mode,
            CaptureVideoMode {
                width: 2_560,
                height: 1_600,
                framerate: 60,
            }
        );
        assert_eq!(native.pointer_surface_dimensions.width, 2_560);
        assert_eq!(native.pointer_surface_dimensions.height, 1_600);

        let mut distorted = config.clone();
        distorted.width = Some(1_920);
        distorted.height = Some(1_080);
        assert!(
            resolve_pipewire_node(dump, &distorted)
                .unwrap_err()
                .to_string()
                .contains("must preserve native 2560x1600 aspect ratio")
        );

        let portrait_dump = std::str::from_utf8(dump)
            .unwrap()
            .replace("2560", "WIDTH")
            .replace("1600", "2560")
            .replace("WIDTH", "1600");
        let portrait = resolve_pipewire_node(portrait_dump.as_bytes(), &native_config).unwrap();
        assert_eq!(
            (portrait.video_mode.width, portrait.video_mode.height),
            (1_600, 2_560)
        );
    }

    #[test]
    fn resolves_pinned_upstream_gamescope_pipewire_contract() {
        // Sanitized from the standard interface emitted by Valve Gamescope
        // commit 17baf4abd1ab3353fb705e4d0d023f84e870f7e8, src/pipewire.cpp
        // blob 76b3ea8cc8ff01c6635498cbafe38130e33215f2. This fixture proves
        // interface compatibility; it is not SteamOS hardware evidence.
        let dump = include_bytes!("../tests/fixtures/upstream-gamescope-pipewire-contract.json");
        let mut config = gamescope_config();
        config.width = None;
        config.height = None;
        config
            .gamescope_pipewire
            .as_mut()
            .unwrap()
            .match_properties
            .clear();

        let resolved = resolve_pipewire_node(dump, &config).unwrap();
        assert_eq!(resolved.target_object, "8842");
        assert_eq!(
            resolved.pointer_surface_dimensions,
            PointerSurfaceDimensions::new(1_920, 1_080).unwrap()
        );
        assert_eq!(
            resolved.video_mode,
            CaptureVideoMode {
                width: 1_920,
                height: 1_080,
                framerate: 60,
            }
        );

        let args: Vec<String> =
            gamescope_pipeline_args(&config, &resolved.target_object, resolved.video_mode)
                .unwrap()
                .into_iter()
                .map(|arg| arg.into_string().unwrap())
                .collect();
        assert!(args.contains(&"target-object=8842".to_owned()));
        assert!(
            args.contains(
                &"video/x-raw,format=NV12,width=1920,height=1080,framerate=60/1".to_owned()
            )
        );
        assert!(args.contains(&"max-rate=60".to_owned()));
    }

    #[test]
    fn rejects_ambiguous_or_unbounded_gamescope_native_size() {
        let config = gamescope_config();
        let dump = br#"[{"type":"PipeWire:Interface:Node","info":{"props":{
          "node.name":"gamescope","media.class":"Video/Source",
          "device.bus-path":"pci-0000:04:00.0","object.serial":731
        },"params":{"EnumFormat":[
          {"mediaType":"video","mediaSubtype":"raw","size":{"width":2560,"height":1600}},
          {"mediaType":"video","mediaSubtype":"raw","size":{"width":1280,"height":800}}
        ]}}}]"#;
        assert!(resolve_pipewire_node(dump, &config).is_err());

        let unbounded = std::str::from_utf8(dump)
            .unwrap()
            .replace("2560", "7681")
            .replace(
                r#",{"mediaType":"video","mediaSubtype":"raw","size":{"width":1280,"height":800}}"#,
                "",
            );
        assert!(resolve_pipewire_node(unbounded.as_bytes(), &config).is_err());

        let mut native_config = config.clone();
        native_config.width = None;
        native_config.height = None;
        let odd = br#"[{"type":"PipeWire:Interface:Node","info":{"props":{
          "node.name":"gamescope","media.class":"Video/Source",
          "device.bus-path":"pci-0000:04:00.0","object.serial":731
        },"params":{"EnumFormat":[
          {"mediaType":"video","mediaSubtype":"raw","size":{"width":1365,"height":768}}
        ]}}}]"#;
        let error = resolve_pipewire_node(odd, &native_config).unwrap_err();
        assert!(error.to_string().contains("must be even"), "{error:#}");
    }

    #[test]
    fn requires_encoder_to_report_configured_render_node() {
        let config = gamescope_config();
        let pipewire = config.gamescope_pipewire.as_ref().unwrap();
        let inspect = br#"
Element Properties:
  device-path         : The DRM device path used for VA operation
                        flags: readable
                        String. Default: "/dev/dri/renderD128"
  aud                 : Insert an AU delimiter for each frame
                        flags: readable, writable
  b-frames            : Number of B-frames
                        flags: readable, writable
  bitrate             : Target bitrate
                        flags: readable, writable
  key-int-max         : Maximum keyframe distance
                        flags: readable, writable
  rate-control        : Rate control mode
                        flags: readable, writable
                        Enum. Default: cbr
                          (2): cbr - Constant Bitrate
  ref-frames          : Number of reference frames
                        flags: readable, writable
  target-usage        : Speed and quality tradeoff
                        flags: readable, writable
"#;
        validate_encoder_inspection(inspect, pipewire).unwrap();

        let index = inspect
            .windows(b"renderD128".len())
            .position(|window| window == b"renderD128")
            .unwrap();
        let mut wrong = inspect.to_vec();
        wrong[index..index + b"renderD128".len()].copy_from_slice(b"renderD129");
        assert!(validate_encoder_inspection(&wrong, pipewire).is_err());

        let mut cqp = gamescope_config();
        let pipewire = cqp.gamescope_pipewire.as_mut().unwrap();
        pipewire.vaapi_encoder = "vah264lpenc".into();
        pipewire.rate_control = VaapiRateControl::Cqp;
        pipewire.bitrate_kbps = None;
        pipewire.quantizer = Some(24);
        let cqp_inspect = String::from_utf8_lossy(inspect)
            .replace("Default: cbr", "Default: cqp")
            .replace("(2): cbr", "(16): cqp")
            .replace(
                "  ref-frames",
                "  qpi                 : I-frame quantizer\n                        flags: readable, writable\n  qpp                 : P-frame quantizer\n                        flags: readable, writable\n  ref-frames",
            );
        validate_encoder_inspection(cqp_inspect.as_bytes(), pipewire).unwrap();
    }
}
