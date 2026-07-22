use std::ffi::OsString;
use std::process::Stdio;

use anyhow::{Context, Result, bail, ensure};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::watch;
use tracing::debug;

use crate::clock::SessionClock;
use crate::config::{HostConfig, VaapiRateControl};

use super::annexb::AnnexBAccessUnitParser;
use super::{
    AccessUnitPublisher, CaptureVideoMode, EncodedFrame, EncodedGop, EncodedSource,
    GamescopePreflight, gamescope_config, interactive_gop_frames, log_stderr_chunks,
};

pub(super) fn spawn_gamescope_pipewire_with_target(
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

pub(super) async fn forward_annex_b_stream<R>(
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

pub(super) fn gamescope_pipeline_args(
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

#[cfg(test)]
mod tests {
    use super::super::tests::{configured_video_mode, gamescope_config};
    use super::*;

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
}
