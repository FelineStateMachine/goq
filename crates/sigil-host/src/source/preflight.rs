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

#[cfg(test)]
mod tests {
    use super::super::tests::gamescope_config;
    use super::*;

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
