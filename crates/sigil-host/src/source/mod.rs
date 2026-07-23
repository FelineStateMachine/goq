use std::borrow::Cow;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use sigil_protocol::PointerSurfaceDimensions;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::{oneshot, watch};
use tracing::warn;

use crate::clock::SessionClock;
#[cfg(test)]
use crate::config::VaapiRateControl;
use crate::config::{GamescopeEncoderBackend, GamescopePipewireConfig, HostConfig};

mod annexb;
mod external;
mod gstreamer_properties;
mod in_process;
mod preflight;

#[cfg(test)]
use external::gamescope_pipeline_args;
use external::{
    forward_annex_b_stream, spawn_gamescope_pipewire_with_target,
    spawn_gamescope_pipewire_with_target_and_shutdown,
};
pub(crate) use gstreamer_properties::probe_encoder_properties;
#[cfg(test)]
pub(crate) use in_process::EncoderControlTestHarness;
#[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
use in_process::spawn_gamescope_pipewire_in_process_with_target;
pub use in_process::{EncoderControl, EncoderControlStatus};
use preflight::preflight_gamescope_static;
pub(crate) use preflight::{preflight_gstreamer_element, validate_executable};

const MAX_ENCODE_BUFFER: usize = 16 * 1024 * 1024;
const MAX_CURRENT_GOP_BYTES: usize = 32 * 1024 * 1024;
const MAX_PW_DUMP_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_DIAGNOSTIC_OUTPUT: usize = 512 * 1024;
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);

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

pub struct ProbeEncodedSource {
    pub source: EncodedSource,
    pub shutdown: Option<oneshot::Sender<()>>,
}

pub fn spawn_gamescope_pipewire_for_probe(
    config: HostConfig,
    session_clock: SessionClock,
    preflight: GamescopePreflight,
) -> Result<ProbeEncodedSource> {
    match gamescope_config(&config)?.encoder_backend {
        GamescopeEncoderBackend::ExternalGstLaunch => {
            let (shutdown, shutdown_receiver) = oneshot::channel();
            let source = spawn_gamescope_pipewire_with_target_and_shutdown(
                config,
                session_clock,
                preflight,
                Some(shutdown_receiver),
            )?;
            Ok(ProbeEncodedSource {
                source,
                shutdown: Some(shutdown),
            })
        }
        GamescopeEncoderBackend::InProcessGstreamer => {
            #[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
            {
                let source = spawn_gamescope_pipewire_in_process_with_target(
                    config,
                    session_clock,
                    preflight,
                )?;
                Ok(ProbeEncodedSource {
                    source,
                    shutdown: None,
                })
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
    let _ = child.kill().await;
    let status = child.wait().await.context("waiting for ffmpeg")?;
    stderr_task.abort();
    match result {
        Ok(()) => Ok(()),
        Err(error) => Err(error).with_context(|| format!("ffmpeg exited with {status}")),
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

    pub(super) fn gamescope_config() -> HostConfig {
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

    pub(super) fn configured_video_mode() -> CaptureVideoMode {
        CaptureVideoMode {
            width: 1_280,
            height: 800,
            framerate: 60,
        }
    }

    fn gamescope_dump(serial: u64, width: u16, height: u16) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!([{
            "type": "PipeWire:Interface:Node",
            "info": {
                "props": {
                    "node.name": "gamescope",
                    "media.class": "Video/Source",
                    "device.bus-path": "pci-0000:04:00.0",
                    "object.serial": serial,
                },
                "params": {
                    "EnumFormat": [{
                        "mediaType": "video",
                        "mediaSubtype": "raw",
                        "format": "NV12",
                        "size": { "width": width, "height": height },
                    }],
                },
            },
        }]))
        .unwrap()
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
    fn recovery_gop_is_bounded_to_half_a_second() {
        assert_eq!(interactive_gop_frames(60), 30);
        assert_eq!(interactive_gop_frames(59), 30);
        assert_eq!(interactive_gop_frames(1), 1);
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
    fn unconfigured_gamescope_mode_follows_each_resolved_session() {
        let mut config = gamescope_config();
        config.width = None;
        config.height = None;

        for (serial, width, height) in [
            (731, 2_560, 1_600),
            (900, 1_280, 800),
            (901, 1_920, 1_080),
            (902, 1_366, 768),
        ] {
            let resolved =
                resolve_pipewire_node(&gamescope_dump(serial, width, height), &config).unwrap();
            assert_eq!(resolved.target_object, serial.to_string());
            assert_eq!(
                resolved.pointer_surface_dimensions,
                PointerSurfaceDimensions::new(width, height).unwrap()
            );
            assert_eq!(
                resolved.video_mode,
                CaptureVideoMode {
                    width,
                    height,
                    framerate: 60,
                }
            );
            assert_eq!(config.configured_dimensions(), None);
        }
    }

    #[test]
    fn resolves_pinned_upstream_gamescope_pipewire_contract() {
        // Sanitized from the standard interface emitted by Valve Gamescope
        // commit 17baf4abd1ab3353fb705e4d0d023f84e870f7e8, src/pipewire.cpp
        // blob 76b3ea8cc8ff01c6635498cbafe38130e33215f2. This fixture proves
        // interface compatibility; it is not SteamOS hardware evidence.
        let dump = include_bytes!("../../tests/fixtures/upstream-gamescope-pipewire-contract.json");
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
}
