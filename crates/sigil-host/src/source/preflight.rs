use std::fs;

use anyhow::{Context, Result, ensure};
use tokio::process::Command;

use crate::config::{
    GamescopeEncoderBackend, GamescopePipewireConfig, HostConfig, VaapiRateControl,
};

use super::{diagnostic, gamescope_config, run_bounded_command};

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, PermissionsExt};

const MAX_INSPECT_OUTPUT: usize = 1024 * 1024;

pub(super) async fn preflight_gamescope_static(config: &HostConfig) -> Result<()> {
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
    validate_render_node(&pipewire.vaapi_render_node)?;

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
    }
    preflight_encoder_properties(pipewire).await?;

    Ok(())
}

async fn preflight_encoder_properties(config: &GamescopePipewireConfig) -> Result<()> {
    let executable = std::env::current_exe().context("resolving the running Sigil executable")?;
    let rate_control = match config.rate_control {
        VaapiRateControl::Cbr => "cbr",
        VaapiRateControl::Cqp => "cqp",
    };
    let mut command = Command::new(executable);
    command.args([
        "capture",
        "encoder-preflight",
        "--vaapi-encoder",
        config.vaapi_encoder.as_str(),
        "--vaapi-render-node",
    ]);
    command.arg(&config.vaapi_render_node);
    command.args(["--rate-control", rate_control]);
    let output = run_bounded_command(command, MAX_INSPECT_OUTPUT).await?;
    ensure!(
        output.status.success(),
        "configured VA encoder {:?} failed programmatic property preflight: {}",
        config.vaapi_encoder,
        diagnostic(&output.stderr)
    );
    Ok(())
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

fn validate_render_node(path: &std::path::Path) -> Result<()> {
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
    Ok(())
}
