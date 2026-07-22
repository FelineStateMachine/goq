use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VideoSource {
    TestPattern,
    GamescopePipewire,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum InputMode {
    Log,
    Disabled,
    Uinput,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VaapiRateControl {
    Cbr,
    Cqp,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum GamescopeEncoderBackend {
    /// Preserve the proven child-process pipeline and its natural-IDR control
    /// limitation for existing configurations.
    #[default]
    ExternalGstLaunch,
    /// Run the video pipeline in-process so Sigil can apply bounded encoder
    /// controls. This remains an explicit opt-in until hardware acceptance.
    InProcessGstreamer,
}

fn default_framerate() -> u32 {
    60
}

fn default_codec() -> String {
    "h264".to_owned()
}

fn default_source() -> VideoSource {
    VideoSource::TestPattern
}

fn default_input_mode() -> InputMode {
    InputMode::Disabled
}

fn default_ffmpeg() -> PathBuf {
    PathBuf::from("ffmpeg")
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GamescopePipewireConfig {
    /// Exact PipeWire `node.name` value to match.
    pub node_name: String,
    /// Exact PipeWire `media.class` value. This proof only accepts Video/Source.
    pub media_class: String,
    /// Optional additional exact PipeWire property matches, for example a stable GPU identity.
    #[serde(default)]
    pub match_properties: BTreeMap<String, String>,
    /// Bootstrap Gamescope Xwayland display used to discover the compositor's
    /// current mouse-focus display. Absence disables cursor feedback.
    #[serde(default)]
    pub xwayland_display: Option<String>,
    /// Absolute paths avoid PATH-dependent executable selection in the daemon.
    pub pw_dump_path: PathBuf,
    pub gst_launch_path: PathBuf,
    pub gst_inspect_path: PathBuf,
    /// External gst-launch remains the compatibility default. The in-process
    /// backend is accepted only by Linux builds that contain its feature.
    #[serde(default)]
    pub encoder_backend: GamescopeEncoderBackend,
    /// Exact dynamically registered VA encoder factory, such as `vah264enc`.
    pub vaapi_encoder: String,
    /// Exact AMD DRM render node expected to back `vaapi_encoder`.
    pub vaapi_render_node: PathBuf,
    /// Explicit mode must be advertised by the selected factory.
    pub rate_control: VaapiRateControl,
    /// Required only for CBR, in kilobits per second.
    pub bitrate_kbps: Option<u32>,
    /// Required only for CQP, applied to I and P frames.
    pub quantizer: Option<u8>,
}

/// Exact PipeWire sink monitor used for optional game-audio capture.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PipewireAudioConfig {
    /// Exact PipeWire `node.name` for the sink whose monitor is captured.
    pub node_name: String,
    /// V1 accepts only an Audio/Sink monitor, never a microphone source.
    pub media_class: String,
    #[serde(default)]
    pub match_properties: BTreeMap<String, String>,
    /// V1 is deliberately fixed at 96 kbit/s stereo Opus.
    pub bitrate_bps: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UinputConfig {
    /// Explicit device node opened by the daemon. No symlinks are followed.
    pub device_path: PathBuf,
    /// Exact ownership and permission metadata expected on the opened device.
    pub expected_owner_uid: u32,
    pub expected_group_gid: u32,
    pub expected_mode: u32,
}

impl UinputConfig {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.device_path == Path::new("/dev/uinput")
                || self.device_path == Path::new("/dev/input/uinput"),
            "uinput.device_path must be /dev/uinput or /dev/input/uinput"
        );
        ensure!(
            self.expected_mode <= 0o777,
            "uinput.expected_mode must contain only Unix permission bits"
        );
        ensure!(
            self.expected_mode & 0o600 == 0o600,
            "uinput.expected_mode must grant owner read/write access"
        );
        ensure!(
            self.expected_mode & 0o111 == 0,
            "uinput.expected_mode must not contain execute bits"
        );
        ensure!(
            self.expected_mode & 0o007 == 0,
            "uinput.expected_mode must not grant access to other users"
        );
        Ok(())
    }
}

impl GamescopePipewireConfig {
    fn validate(&self) -> Result<()> {
        validate_pipewire_property("node_name", &self.node_name)?;
        ensure!(
            self.media_class == "Video/Source",
            "gamescope_pipewire.media_class must be Video/Source"
        );
        for (key, value) in &self.match_properties {
            validate_pipewire_property("match_properties key", key)?;
            validate_pipewire_property("match_properties value", value)?;
            ensure!(
                key != "node.name" && key != "media.class" && key != "object.serial",
                "gamescope_pipewire.match_properties must not override {key}"
            );
        }
        if let Some(display) = &self.xwayland_display {
            let number = display.strip_prefix(':').unwrap_or_default();
            ensure!(
                !number.is_empty()
                    && number.len() <= 3
                    && number.bytes().all(|byte| byte.is_ascii_digit()),
                "gamescope_pipewire.xwayland_display must be : followed by 1 to 3 digits"
            );
        }
        for (name, path) in [
            ("pw_dump_path", &self.pw_dump_path),
            ("gst_launch_path", &self.gst_launch_path),
            ("gst_inspect_path", &self.gst_inspect_path),
            ("vaapi_render_node", &self.vaapi_render_node),
        ] {
            ensure!(
                path.is_absolute(),
                "gamescope_pipewire.{name} must be an absolute path"
            );
        }
        ensure!(
            self.encoder_backend != GamescopeEncoderBackend::InProcessGstreamer
                || cfg!(all(target_os = "linux", feature = "in-process-gstreamer")),
            "gamescope_pipewire.encoder_backend=in-process-gstreamer requires a Linux Sigil build with the in-process-gstreamer feature"
        );
        ensure!(
            self.encoder_backend != GamescopeEncoderBackend::InProcessGstreamer
                || self.rate_control == VaapiRateControl::Cbr,
            "gamescope_pipewire.encoder_backend=in-process-gstreamer currently requires CBR"
        );
        ensure!(
            self.vaapi_render_node
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.strip_prefix("renderD").is_some_and(|suffix| {
                        !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
                    })
                }),
            "gamescope_pipewire.vaapi_render_node must name a DRM renderD device"
        );
        let generic_encoder = matches!(self.vaapi_encoder.as_str(), "vah264enc" | "vah264lpenc");
        let per_device_suffix = self
            .vaapi_encoder
            .strip_prefix("varenderD")
            .and_then(|name| {
                name.strip_suffix("h264enc")
                    .or_else(|| name.strip_suffix("h264lpenc"))
            });
        ensure!(
            generic_encoder
                || per_device_suffix.is_some_and(|device| {
                    !device.is_empty() && device.bytes().all(|byte| byte.is_ascii_digit())
                }),
            "gamescope_pipewire.vaapi_encoder must be a GstVA H.264 factory for normal or low-power encoding"
        );
        match self.rate_control {
            VaapiRateControl::Cbr => {
                ensure!(
                    self.bitrate_kbps
                        .is_some_and(|bitrate| (1_000..=100_000).contains(&bitrate)),
                    "gamescope_pipewire.bitrate_kbps must be between 1000 and 100000 for CBR"
                );
                ensure!(
                    self.quantizer.is_none(),
                    "gamescope_pipewire.quantizer must be absent for CBR"
                );
            }
            VaapiRateControl::Cqp => {
                ensure!(
                    self.quantizer
                        .is_some_and(|quantizer| (1..=51).contains(&quantizer)),
                    "gamescope_pipewire.quantizer must be between 1 and 51 for CQP"
                );
                ensure!(
                    self.bitrate_kbps.is_none(),
                    "gamescope_pipewire.bitrate_kbps must be absent for CQP"
                );
            }
        }
        Ok(())
    }
}

impl PipewireAudioConfig {
    fn validate(&self) -> Result<()> {
        validate_pipewire_property("audio.node_name", &self.node_name)?;
        ensure!(
            self.media_class == "Audio/Sink",
            "audio.media_class must be Audio/Sink"
        );
        for (key, value) in &self.match_properties {
            validate_pipewire_property("audio.match_properties key", key)?;
            validate_pipewire_property("audio.match_properties value", value)?;
            ensure!(
                key != "node.name" && key != "media.class" && key != "object.serial",
                "audio.match_properties must not override {key}"
            );
        }
        ensure!(
            self.bitrate_bps == 96_000,
            "audio.bitrate_bps must be 96000 for the v1 Opus target"
        );
        Ok(())
    }
}

fn validate_pipewire_property(name: &str, value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 512 && !value.contains('\0'),
        "gamescope_pipewire.{name} must contain 1 to 512 non-NUL bytes"
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub identity_path: PathBuf,
    pub state_path: PathBuf,
    #[serde(default = "default_source")]
    pub source: VideoSource,
    /// Optional encoded-size override. Gamescope uses its advertised native
    /// size when both fields are absent; the test-pattern proof retains its
    /// 1280x800 fixture default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default = "default_framerate")]
    pub framerate: u32,
    #[serde(default = "default_codec")]
    pub codec: String,
    #[serde(default = "default_input_mode")]
    pub input_mode: InputMode,
    /// Required only when input_mode is uinput.
    #[serde(default)]
    pub uinput: Option<UinputConfig>,
    #[serde(default = "default_ffmpeg")]
    pub ffmpeg_path: PathBuf,
    /// Required only for the production Gamescope PipeWire source.
    #[serde(default)]
    pub gamescope_pipewire: Option<GamescopePipewireConfig>,
    /// Optional exact PipeWire sink monitor. Absence keeps the host video-only.
    #[serde(default)]
    pub audio: Option<PipewireAudioConfig>,
}

impl HostConfig {
    pub const TEST_PATTERN_WIDTH: u32 = 1_280;
    pub const TEST_PATTERN_HEIGHT: u32 = 800;

    pub fn load(path: &Path) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let mut file = options.open(path).with_context(|| {
            format!(
                "opening config {} without following symlinks",
                path.display()
            )
        })?;
        let metadata = file
            .metadata()
            .with_context(|| format!("inspecting opened config {}", path.display()))?;
        validate_file_security(path, &metadata)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Self = toml::from_str(&contents)
            .with_context(|| format!("parsing config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.identity_path.as_os_str().is_empty(),
            "identity_path is required"
        );
        ensure!(
            !self.state_path.as_os_str().is_empty(),
            "state_path is required"
        );
        match (self.width, self.height) {
            (Some(width), Some(height)) => validate_video_dimensions(width, height)?,
            (None, None) => {}
            _ => anyhow::bail!("width and height must either both be set or both be absent"),
        }
        ensure!(
            (1..=240).contains(&self.framerate),
            "framerate must be between 1 and 240"
        );
        ensure!(
            self.codec == "h264",
            "only h264 is supported during the first proof"
        );
        ensure!(
            !self.ffmpeg_path.as_os_str().is_empty(),
            "ffmpeg_path is required"
        );
        match (&self.input_mode, &self.uinput) {
            (InputMode::Uinput, Some(config)) => config.validate()?,
            (InputMode::Uinput, None) => {
                anyhow::bail!("uinput configuration is required when input_mode is uinput");
            }
            (InputMode::Log | InputMode::Disabled, None) => {}
            (InputMode::Log | InputMode::Disabled, Some(_)) => {
                anyhow::bail!("uinput configuration must be absent unless input_mode is uinput");
            }
        }
        match (&self.source, &self.gamescope_pipewire) {
            (VideoSource::TestPattern, None) => {}
            (VideoSource::TestPattern, Some(_)) => {
                anyhow::bail!("gamescope_pipewire must be absent when source is test-pattern");
            }
            (VideoSource::GamescopePipewire, Some(config)) => config.validate()?,
            (VideoSource::GamescopePipewire, None) => {
                anyhow::bail!(
                    "gamescope_pipewire configuration is required when source is gamescope-pipewire"
                );
            }
        }
        match (&self.source, &self.gamescope_pipewire, &self.audio) {
            (_, _, None) => {}
            (VideoSource::GamescopePipewire, Some(_), Some(audio)) => audio.validate()?,
            _ => anyhow::bail!(
                "audio requires source=gamescope-pipewire and gamescope_pipewire configuration"
            ),
        }
        Ok(())
    }

    pub fn configured_dimensions(&self) -> Option<(u32, u32)> {
        self.width.zip(self.height)
    }

    pub fn resolved_dimensions(&self) -> Result<(u32, u32)> {
        self.configured_dimensions()
            .context("video dimensions have not been resolved")
    }

    pub fn test_pattern_dimensions(&self) -> Result<(u32, u32)> {
        ensure!(
            self.source == VideoSource::TestPattern,
            "test-pattern dimensions requested for a non-test source"
        );
        Ok(self
            .configured_dimensions()
            .unwrap_or((Self::TEST_PATTERN_WIDTH, Self::TEST_PATTERN_HEIGHT)))
    }

    pub fn apply_resolved_dimensions(&mut self, width: u32, height: u32) -> Result<()> {
        validate_video_dimensions(width, height)?;
        self.width = Some(width);
        self.height = Some(height);
        Ok(())
    }

    pub fn ensure_runtime_directory(&self) -> Result<()> {
        if self.state_path.exists() {
            let metadata = fs::symlink_metadata(&self.state_path)
                .with_context(|| format!("inspecting state_path {}", self.state_path.display()))?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "state_path must not be a symlink"
            );
            ensure!(metadata.is_dir(), "state_path must be a directory");
            #[cfg(unix)]
            {
                ensure!(
                    metadata.mode() & 0o077 == 0,
                    "state_path {} must not be accessible by group or other users",
                    self.state_path.display()
                );
                ensure!(
                    metadata.uid() == unsafe { libc::geteuid() },
                    "state_path {} is not owned by the current user",
                    self.state_path.display()
                );
            }
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = fs::DirBuilder::new();
                builder
                    .recursive(true)
                    .mode(0o700)
                    .create(&self.state_path)?;
            }
            #[cfg(not(unix))]
            fs::create_dir_all(&self.state_path)?;
        }
        Ok(())
    }
}

fn validate_video_dimensions(width: u32, height: u32) -> Result<()> {
    ensure!(
        (64..=7680).contains(&width),
        "width must be between 64 and 7680"
    );
    ensure!(
        (64..=4320).contains(&height),
        "height must be between 64 and 4320"
    );
    ensure!(
        width.is_multiple_of(2) && height.is_multiple_of(2),
        "H.264 dimensions must be even"
    );
    Ok(())
}

fn validate_file_security(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    ensure!(metadata.is_file(), "config must be a regular file");
    #[cfg(unix)]
    {
        ensure!(
            metadata.permissions().mode() & 0o022 == 0,
            "config {} must not be writable by group or other users",
            path.display()
        );
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "config {} is not owned by the current user",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_fields() {
        let input = r#"
identity_path = "/tmp/host.key"
state_path = "/tmp/state"
surprise = true
"#;
        assert!(toml::from_str::<HostConfig>(input).is_err());
    }

    #[test]
    fn validates_first_target() {
        let input = r#"
identity_path = "/tmp/host.key"
state_path = "/tmp/state"
source = "test-pattern"
width = 1280
height = 800
framerate = 60
codec = "h264"
"#;
        let config: HostConfig = toml::from_str(input).unwrap();
        config.validate().unwrap();
        assert_eq!(config.test_pattern_dimensions().unwrap(), (1_280, 800));
    }

    #[test]
    fn test_pattern_omission_retains_the_proof_fixture() {
        let config: HostConfig = toml::from_str(
            r#"
identity_path = "/tmp/host.key"
state_path = "/tmp/state"
"#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.configured_dimensions(), None);
        assert_eq!(config.test_pattern_dimensions().unwrap(), (1_280, 800));
    }

    #[test]
    fn validates_strict_gamescope_pipewire_configuration() {
        let input = r#"
identity_path = "/tmp/host.key"
state_path = "/tmp/state"
source = "gamescope-pipewire"
framerate = 60
codec = "h264"

[gamescope_pipewire]
node_name = "gamescope"
media_class = "Video/Source"
xwayland_display = ":0"
pw_dump_path = "/usr/bin/pw-dump"
gst_launch_path = "/usr/bin/gst-launch-1.0"
gst_inspect_path = "/usr/bin/gst-inspect-1.0"
encoder_backend = "external-gst-launch"
vaapi_encoder = "vah264enc"
vaapi_render_node = "/dev/dri/renderD128"
rate_control = "cbr"
bitrate_kbps = 12000

[gamescope_pipewire.match_properties]
"device.bus-path" = "pci-0000:04:00.0"

[audio]
node_name = "sigil-game-audio"
media_class = "Audio/Sink"
bitrate_bps = 96000

[audio.match_properties]
"device.profile.name" = "stereo"
"#;
        let config: HostConfig = toml::from_str(input).unwrap();
        config.validate().unwrap();
        assert_eq!(config.configured_dimensions(), None);
        assert_eq!(
            config
                .gamescope_pipewire
                .as_ref()
                .unwrap()
                .xwayland_display
                .as_deref(),
            Some(":0")
        );
        assert_eq!(
            config.gamescope_pipewire.as_ref().unwrap().encoder_backend,
            GamescopeEncoderBackend::ExternalGstLaunch
        );
    }

    #[test]
    fn audio_is_optional_and_strictly_tied_to_gamescope() {
        let mut config: HostConfig = toml::from_str(
            r#"
identity_path = "/tmp/host.key"
state_path = "/tmp/state"
source = "gamescope-pipewire"

[gamescope_pipewire]
node_name = "gamescope"
media_class = "Video/Source"
pw_dump_path = "/usr/bin/pw-dump"
gst_launch_path = "/usr/bin/gst-launch-1.0"
gst_inspect_path = "/usr/bin/gst-inspect-1.0"
vaapi_encoder = "vah264enc"
vaapi_render_node = "/dev/dri/renderD128"
rate_control = "cbr"
bitrate_kbps = 12000

[audio]
node_name = "sigil-game-audio"
media_class = "Audio/Sink"
bitrate_bps = 96000
"#,
        )
        .unwrap();
        config.validate().unwrap();

        config.source = VideoSource::TestPattern;
        assert!(config.validate().is_err());
        config.source = VideoSource::GamescopePipewire;

        config.audio.as_mut().unwrap().media_class = "Audio/Source".into();
        assert!(config.validate().is_err());
        config.audio.as_mut().unwrap().media_class = "Audio/Sink".into();
        config.audio.as_mut().unwrap().bitrate_bps = 128_000;
        assert!(config.validate().is_err());
        config.audio.as_mut().unwrap().bitrate_bps = 96_000;
        config
            .audio
            .as_mut()
            .unwrap()
            .match_properties
            .insert("object.serial".into(), "50".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn gamescope_configuration_fails_closed() {
        let mut config: HostConfig = toml::from_str(
            r#"
identity_path = "/tmp/host.key"
state_path = "/tmp/state"
source = "gamescope-pipewire"
"#,
        )
        .unwrap();
        assert!(config.validate().is_err());

        config.source = VideoSource::TestPattern;
        config.gamescope_pipewire = Some(GamescopePipewireConfig {
            xwayland_display: None,
            node_name: "gamescope".into(),
            media_class: "Video/Source".into(),
            match_properties: BTreeMap::new(),
            pw_dump_path: "/usr/bin/pw-dump".into(),
            gst_launch_path: "/usr/bin/gst-launch-1.0".into(),
            gst_inspect_path: "/usr/bin/gst-inspect-1.0".into(),
            encoder_backend: GamescopeEncoderBackend::ExternalGstLaunch,
            vaapi_encoder: "vah264enc".into(),
            vaapi_render_node: "/dev/dri/renderD128".into(),
            rate_control: VaapiRateControl::Cbr,
            bitrate_kbps: Some(12_000),
            quantizer: None,
        });
        assert!(config.validate().is_err());

        config.source = VideoSource::GamescopePipewire;
        config.gamescope_pipewire.as_mut().unwrap().xwayland_display = Some("localhost:0".into());
        assert!(config.validate().is_err());
        config.gamescope_pipewire.as_mut().unwrap().xwayland_display = Some(":0".into());
        config.validate().unwrap();
        config.gamescope_pipewire.as_mut().unwrap().vaapi_encoder = "x264enc".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn gamescope_encoder_backend_defaults_external() {
        let config: HostConfig = toml::from_str(
            r#"
identity_path = "/tmp/host.key"
state_path = "/tmp/state"
source = "gamescope-pipewire"

[gamescope_pipewire]
node_name = "gamescope"
media_class = "Video/Source"
pw_dump_path = "/usr/bin/pw-dump"
gst_launch_path = "/usr/bin/gst-launch-1.0"
gst_inspect_path = "/usr/bin/gst-inspect-1.0"
vaapi_encoder = "vah264enc"
vaapi_render_node = "/dev/dri/renderD128"
rate_control = "cbr"
bitrate_kbps = 12000
"#,
        )
        .unwrap();

        assert_eq!(
            config.gamescope_pipewire.unwrap().encoder_backend,
            GamescopeEncoderBackend::ExternalGstLaunch
        );
    }

    #[cfg(not(all(target_os = "linux", feature = "in-process-gstreamer")))]
    #[test]
    fn unavailable_in_process_backend_fails_closed() {
        let mut config: HostConfig = toml::from_str(
            r#"
identity_path = "/tmp/host.key"
state_path = "/tmp/state"
source = "gamescope-pipewire"

[gamescope_pipewire]
node_name = "gamescope"
media_class = "Video/Source"
pw_dump_path = "/usr/bin/pw-dump"
gst_launch_path = "/usr/bin/gst-launch-1.0"
gst_inspect_path = "/usr/bin/gst-inspect-1.0"
vaapi_encoder = "vah264enc"
vaapi_render_node = "/dev/dri/renderD128"
rate_control = "cbr"
bitrate_kbps = 12000
"#,
        )
        .unwrap();
        config.gamescope_pipewire.as_mut().unwrap().encoder_backend =
            GamescopeEncoderBackend::InProcessGstreamer;

        assert!(config.validate().is_err());
    }

    #[cfg(all(target_os = "linux", feature = "in-process-gstreamer"))]
    #[test]
    fn feature_built_linux_accepts_in_process_backend() {
        let mut config: HostConfig = toml::from_str(
            r#"
identity_path = "/tmp/host.key"
state_path = "/tmp/state"
source = "gamescope-pipewire"

[gamescope_pipewire]
node_name = "gamescope"
media_class = "Video/Source"
pw_dump_path = "/usr/bin/pw-dump"
gst_launch_path = "/usr/bin/gst-launch-1.0"
gst_inspect_path = "/usr/bin/gst-inspect-1.0"
vaapi_encoder = "vah264enc"
vaapi_render_node = "/dev/dri/renderD128"
rate_control = "cbr"
bitrate_kbps = 12000
"#,
        )
        .unwrap();
        let pipewire = config.gamescope_pipewire.as_mut().unwrap();
        pipewire.encoder_backend = GamescopeEncoderBackend::InProcessGstreamer;

        config.validate().unwrap();
        let pipewire = config.gamescope_pipewire.unwrap();
        assert_eq!(
            pipewire.encoder_backend,
            GamescopeEncoderBackend::InProcessGstreamer
        );
        assert_eq!(pipewire.rate_control, VaapiRateControl::Cbr);
    }

    #[test]
    fn in_process_cqp_fails_closed() {
        let pipewire = GamescopePipewireConfig {
            node_name: "gamescope".into(),
            media_class: "Video/Source".into(),
            match_properties: BTreeMap::new(),
            xwayland_display: None,
            pw_dump_path: "/usr/bin/pw-dump".into(),
            gst_launch_path: "/usr/bin/gst-launch-1.0".into(),
            gst_inspect_path: "/usr/bin/gst-inspect-1.0".into(),
            encoder_backend: GamescopeEncoderBackend::InProcessGstreamer,
            vaapi_encoder: "vah264enc".into(),
            vaapi_render_node: "/dev/dri/renderD128".into(),
            rate_control: VaapiRateControl::Cqp,
            bitrate_kbps: None,
            quantizer: Some(24),
        };

        assert!(pipewire.validate().is_err());
    }

    #[test]
    fn rejects_odd_dimensions_and_codec_breadth() {
        let mut config: HostConfig = toml::from_str(
            r#"
identity_path = "/tmp/host.key"
state_path = "/tmp/state"
"#,
        )
        .unwrap();
        config.width = Some(1279);
        assert!(config.validate().is_err());
        config.width = Some(1280);
        config.codec = "av1".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_partial_dimension_override() {
        let config: HostConfig = toml::from_str(
            r#"
identity_path = "/tmp/host.key"
state_path = "/tmp/state"
width = 1280
"#,
        )
        .unwrap();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("width and height must either both be set or both be absent")
        );
    }

    #[test]
    fn validates_strict_uinput_configuration() {
        let config: HostConfig = toml::from_str(
            r#"
identity_path = "/tmp/host.key"
state_path = "/tmp/state"
input_mode = "uinput"

[uinput]
device_path = "/dev/uinput"
expected_owner_uid = 0
expected_group_gid = 986
expected_mode = 0o660
"#,
        )
        .unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn uinput_configuration_fails_closed() {
        let mut config: HostConfig = toml::from_str(
            r#"
identity_path = "/tmp/host.key"
state_path = "/tmp/state"
input_mode = "uinput"

[uinput]
device_path = "/tmp/uinput"
expected_owner_uid = 0
expected_group_gid = 986
expected_mode = 0o666
"#,
        )
        .unwrap();
        assert!(config.validate().is_err());

        config.uinput.as_mut().unwrap().device_path = "/dev/uinput".into();
        assert!(config.validate().is_err());

        config.uinput.as_mut().unwrap().expected_mode = 0o660;
        config.input_mode = InputMode::Disabled;
        assert!(config.validate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_and_permissive_runtime_directories() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let link = temp.path().join("link");
        symlink(&target, &link).unwrap();

        let mut config: HostConfig = toml::from_str(
            r#"
identity_path = "/tmp/host.key"
state_path = "/tmp/state"
"#,
        )
        .unwrap();
        config.state_path = link;
        assert!(config.ensure_runtime_directory().is_err());

        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        config.state_path = target;
        assert!(config.ensure_runtime_directory().is_err());
    }
}
