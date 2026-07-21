use std::borrow::Cow;
use std::ffi::OsString;
use std::fs;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use sigil_protocol::PointerSurfaceDimensions;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::watch;
use tracing::{debug, warn};

use crate::clock::SessionClock;
use crate::config::{GamescopePipewireConfig, HostConfig, VaapiRateControl};

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, PermissionsExt};

const MAX_ENCODE_BUFFER: usize = 16 * 1024 * 1024;
const MAX_CURRENT_GOP_BYTES: usize = 32 * 1024 * 1024;
const MAX_PW_DUMP_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_INSPECT_OUTPUT: usize = 1024 * 1024;
const MAX_DIAGNOSTIC_OUTPUT: usize = 512 * 1024;
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct EncodedFrame {
    pub sequence: u64,
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
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GamescopePreflight {
    /// PipeWire object.serial, resolved from exact configured properties.
    pub target_object: String,
    /// Native Gamescope surface used by the relative-pointer coordinate space.
    pub pointer_surface_dimensions: PointerSurfaceDimensions,
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
        ("gst_launch_path", &pipewire.gst_launch_path),
        ("gst_inspect_path", &pipewire.gst_inspect_path),
    ] {
        validate_executable(name, path)?;
    }
    validate_amd_render_node(&pipewire.vaapi_render_node)?;

    for element in [
        "pipewiresrc",
        "queue",
        "videoconvert",
        "videoscale",
        "videorate",
        pipewire.vaapi_encoder.as_str(),
        "h264parse",
        "fdsink",
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
    resolve_pipewire_node(&output.stdout, pipewire)
}

pub async fn spawn_gamescope_pipewire(
    config: HostConfig,
    session_clock: SessionClock,
) -> Result<EncodedSource> {
    let preflight = preflight_gamescope_pipewire(&config).await?;
    spawn_gamescope_pipewire_with_target(config, session_clock, preflight)
}

/// Start capture after `serve` has completed the static executable/GstVA
/// preflight. The PipeWire node is still resolved immediately before each
/// session because its object serial can change when Gamescope restarts.
pub async fn spawn_gamescope_pipewire_after_static_preflight(
    config: HostConfig,
    session_clock: SessionClock,
) -> Result<EncodedSource> {
    let preflight = resolve_gamescope_pipewire_target(&config).await?;
    spawn_gamescope_pipewire_with_target(config, session_clock, preflight)
}

fn spawn_gamescope_pipewire_with_target(
    config: HostConfig,
    session_clock: SessionClock,
    preflight: GamescopePreflight,
) -> Result<EncodedSource> {
    let (sender, receiver) = watch::channel(None);
    let (current_gop_sender, current_gop) = watch::channel(None);
    let target_object = preflight.target_object;
    let task = tokio::spawn(async move {
        run_gamescope_pipewire(
            config,
            session_clock,
            target_object,
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
    })
}

async fn run_gamescope_pipewire(
    config: HostConfig,
    session_clock: SessionClock,
    target_object: String,
    sender: watch::Sender<Option<EncodedFrame>>,
    current_gop_sender: watch::Sender<Option<EncodedGop>>,
) -> Result<()> {
    let pipewire = gamescope_config(&config)?;
    let args = gamescope_pipeline_args(&config, &target_object)?;
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
        interactive_gop_frames(config.framerate) as usize,
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
    let input = format!(
        "testsrc2=size={}x{}:rate={}",
        config.width, config.height, config.framerate
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
) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut sequence = 0_u64;
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
            let observed_at = Instant::now();
            let capture_timestamp_micros = session_clock.micros_at(observed_at);
            let presentation_timestamp_micros =
                i64::try_from(capture_timestamp_micros).unwrap_or(i64::MAX);
            let frame = EncodedFrame {
                sequence,
                capture_timestamp_micros,
                presentation_timestamp_micros,
                observed_at,
                keyframe: is_h264_keyframe(&access_unit),
                codec_config: has_h264_codec_config(&access_unit),
                data: Arc::from(access_unit),
            };
            sequence = sequence.saturating_add(1);
            publish_encoded_frame(
                sender,
                current_gop_sender,
                frame,
                max_gop_frames,
                MAX_CURRENT_GOP_BYTES,
            );
            if sender.is_closed() && current_gop_sender.is_closed() {
                debug!("encoded source has no receivers");
                return Ok(());
            }
        }
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

fn gamescope_pipeline_args(config: &HostConfig, target_object: &str) -> Result<Vec<OsString>> {
    let pipewire = gamescope_config(config)?;
    ensure!(
        !target_object.is_empty()
            && target_object.len() <= 32
            && target_object.bytes().all(|byte| byte.is_ascii_digit()),
        "resolved PipeWire object.serial is invalid"
    );
    let caps = format!(
        "video/x-raw,format=NV12,width={},height={},framerate={}/1",
        config.width, config.height, config.framerate
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
        format!("max-rate={}", config.framerate),
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
        format!("key-int-max={}", interactive_gop_frames(config.framerate)),
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
        // Gamescope advertises a variable PipeWire rate and can deliver on
        // every display vblank. Keep the terminal sink clocked so negotiated
        // 60 fps timestamps cannot be written to the daemon in a 144 Hz burst.
        "sync=true".to_owned(),
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

fn resolve_pipewire_node(
    output: &[u8],
    config: &GamescopePipewireConfig,
) -> Result<GamescopePreflight> {
    let target_object = resolve_pipewire_node_exact(
        output,
        &config.node_name,
        &config.media_class,
        &config.match_properties,
    )?;
    let pointer_surface_dimensions = resolve_pointer_surface_dimensions(
        output,
        &target_object,
        &config.node_name,
        &config.media_class,
        &config.match_properties,
    )?;
    Ok(GamescopePreflight {
        target_object,
        pointer_surface_dimensions,
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
    use crate::config::{InputMode, VideoSource};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn gamescope_config() -> HostConfig {
        HostConfig {
            identity_path: PathBuf::from("/tmp/host.key"),
            state_path: PathBuf::from("/tmp/state"),
            source: VideoSource::GamescopePipewire,
            width: 1280,
            height: 800,
            framerate: 60,
            codec: "h264".into(),
            input_mode: InputMode::Disabled,
            uinput: None,
            ffmpeg_path: PathBuf::from("/usr/bin/ffmpeg"),
            gamescope_pipewire: Some(GamescopePipewireConfig {
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
    fn current_gop_retains_only_a_bounded_contiguous_decodable_chain() {
        let (frame_sender, frame_receiver) = watch::channel(None);
        let (current_gop_sender, current_gop_receiver) = watch::channel(None);
        let frame = |sequence, keyframe, codec_config, payload_len| EncodedFrame {
            sequence,
            capture_timestamp_micros: sequence,
            presentation_timestamp_micros: sequence as i64,
            observed_at: Instant::now(),
            keyframe,
            codec_config,
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
        let args: Vec<String> = gamescope_pipeline_args(&config, "1234")
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
            "sync=true".to_owned()
        ]));
        let videorate = args.iter().position(|arg| arg == "videorate").unwrap();
        let queue = args.iter().position(|arg| arg == "queue").unwrap();
        let videoconvert = args.iter().position(|arg| arg == "videoconvert").unwrap();
        let videoscale = args.iter().position(|arg| arg == "videoscale").unwrap();
        assert!(queue < videorate && videorate < videoconvert && videoconvert < videoscale);
        assert!(gamescope_pipeline_args(&config, "gamescope").is_err());
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

        let args: Vec<String> = gamescope_pipeline_args(&config, "1234")
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
        let pipewire = config.gamescope_pipewire.as_ref().unwrap();
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
            resolve_pipewire_node(dump, pipewire).unwrap(),
            GamescopePreflight {
                target_object: "731".into(),
                pointer_surface_dimensions: PointerSurfaceDimensions::new(2_560, 1_600).unwrap(),
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
        assert!(resolve_pipewire_node(duplicate, pipewire).is_err());
    }

    #[test]
    fn rejects_ambiguous_or_unbounded_gamescope_native_size() {
        let config = gamescope_config();
        let pipewire = config.gamescope_pipewire.as_ref().unwrap();
        let dump = br#"[{"type":"PipeWire:Interface:Node","info":{"props":{
          "node.name":"gamescope","media.class":"Video/Source",
          "device.bus-path":"pci-0000:04:00.0","object.serial":731
        },"params":{"EnumFormat":[
          {"mediaType":"video","mediaSubtype":"raw","size":{"width":2560,"height":1600}},
          {"mediaType":"video","mediaSubtype":"raw","size":{"width":1280,"height":800}}
        ]}}}]"#;
        assert!(resolve_pipewire_node(dump, pipewire).is_err());

        let unbounded = std::str::from_utf8(dump)
            .unwrap()
            .replace("2560", "7681")
            .replace(
                r#",{"mediaType":"video","mediaSubtype":"raw","size":{"width":1280,"height":800}}"#,
                "",
            );
        assert!(resolve_pipewire_node(unbounded.as_bytes(), pipewire).is_err());
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
