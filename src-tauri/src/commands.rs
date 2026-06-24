//! Tauri backend commands for Keyhome.
//!
//! Commands:
//! - fido_device_info: enumerate FIDO2 devices and get info
//! - fido_pin_retries: check PIN retry count
//! - titan_derive_identity: derive Iroh node ID from Titan (no network)
//! - iroh_host_start: derive identity from Titan, start hosting (frames + input)
//! - iroh_host_status: check if host endpoint is running
//! - iroh_client_connect: derive host ID from Titan, connect via relay (no address)
//! - iroh_client_send_input: send input event to host for injection

use anyhow::Context as _;
use base64::Engine;
use ctap_hid_fido2::fidokey::{
    GetAssertionArgsBuilder, MakeCredentialArgsBuilder,
    get_assertion::Extension as Gext,
    get_assertion::get_assertion_params::Assertion,
    make_credential::Extension as Mext,
};
use ctap_hid_fido2::public_key_credential_user_entity::PublicKeyCredentialUserEntity;
use ctap_hid_fido2::{verifier, Cfg, FidoKeyHidFactory};
use enigo::{Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use iroh::endpoint::{presets, Connection};
use iroh::protocol::{ProtocolHandler, Router};
use iroh::{Endpoint, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use openh264::encoder::Encoder;
use openh264::formats::{RgbSliceU8, YUVBuffer, YUVSource};
use openh264::nal_units;
use std::io::Cursor;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex as TokioMutex;

const FRAME_ALPN: &[u8] = b"keyhome/frame-stream/0";
const INPUT_ALPN: &[u8] = b"keyhome/input-stream/0";
const RPID: &str = "keyhome";
const SALT_MESSAGE: &str = "keyhome-iroh-identity-v1";
const N0_RELAY: &str = "https://usw1-1.relay.n0.iroh.link./";
const KEYRING_SERVICE: &str = "keyhome";
const KEYRING_ENTRY: &str = "host-identity";

fn codec_to_byte(codec: &str) -> u8 {
    match codec {
        "h265" => 1,
        "av1" => 2,
        _ => 0,
    }
}

fn byte_to_codec(b: u8) -> &'static str {
    match b {
        1 => "h265",
        2 => "av1",
        _ => "h264",
    }
}

// ─── Keyring Persistence ─────────────────────────────────────────────────────

fn store_identity_in_keyring(secret: &[u8; 32]) -> anyhow::Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ENTRY)
        .context("Failed to create keyring entry")?;
    entry
        .set_secret(secret)
        .context("Failed to store identity in keyring")
}

fn load_identity_from_keyring() -> anyhow::Result<Option<[u8; 32]>> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ENTRY)
        .context("Failed to create keyring entry")?;
    match entry.get_secret() {
        Ok(bytes) => {
            if bytes.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                Ok(Some(arr))
            } else {
                anyhow::bail!("Keyring entry has wrong length: {}", bytes.len())
            }
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => anyhow::bail!("Failed to read keyring: {:?}", e),
    }
}

fn clear_identity_from_keyring() -> anyhow::Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ENTRY)
        .context("Failed to create keyring entry")?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => anyhow::bail!("Failed to clear keyring: {:?}", e),
    }
}

// ─── Titan HMAC-Secret Derivation ────────────────────────────────────────────

fn derive_secret_from_titan(pin: &str) -> anyhow::Result<[u8; 32]> {
    let cfg = Cfg::init();
    let device = FidoKeyHidFactory::create(&cfg)
        .context("Failed to open FIDO2 device")?;

    let salt: [u8; 32] = {
        let mut hasher = Sha256::new();
        hasher.update(SALT_MESSAGE.as_bytes());
        let result = hasher.finalize();
        let mut s = [0u8; 32];
        s.copy_from_slice(&result);
        s
    };

    // Try get_assertion without credential ID (uses resident key)
    let challenge = verifier::create_challenge();
    let get_args = GetAssertionArgsBuilder::new(RPID, &challenge)
        .pin(pin)
        .extensions(&[Gext::HmacSecret(Some(salt))])
        .build();

    match device.get_assertion_with_args(&get_args) {
        Ok(assertions) => {
            return extract_hmac_secret(&assertions);
        }
        Err(_) => {}
    }

    // No resident key — create one
    let user_entity = PublicKeyCredentialUserEntity::new(
        Some(b"keyhome-user"),
        Some("keyhome"),
        Some("Keyhome"),
    );

    let challenge = verifier::create_challenge();
    let make_args = MakeCredentialArgsBuilder::new(RPID, &challenge)
        .pin(pin)
        .user_entity(&user_entity)
        .resident_key()
        .extensions(&[Mext::HmacSecret(Some(true))])
        .build();

    let attestation = device.make_credential_with_args(&make_args)
        .context("make_credential failed")?;

    let verify_result = verifier::verify_attestation(RPID, &challenge, &attestation);
    if !verify_result.is_success {
        anyhow::bail!("Attestation verification failed");
    }
    let credential_id = verify_result.credential_id;

    // Get assertion with the new credential
    let challenge2 = verifier::create_challenge();
    let get_args = GetAssertionArgsBuilder::new(RPID, &challenge2)
        .pin(pin)
        .credential_id(&credential_id)
        .extensions(&[Gext::HmacSecret(Some(salt))])
        .build();

    let assertions = device.get_assertion_with_args(&get_args)
        .context("get_assertion failed")?;

    extract_hmac_secret(&assertions)
}

fn extract_hmac_secret(assertions: &[Assertion]) -> anyhow::Result<[u8; 32]> {
    for ext in &assertions[0].extensions {
        if let Gext::HmacSecret(Some(output)) = ext {
            let mut secret = [0u8; 32];
            secret.copy_from_slice(&output[..]);
            return Ok(secret);
        }
    }
    anyhow::bail!("No hmac-secret in assertion response")
}

fn derive_iroh_secret_from_titan(pin: &str) -> anyhow::Result<SecretKey> {
    let secret_bytes = derive_secret_from_titan(pin)?;
    Ok(SecretKey::from_bytes(&secret_bytes))
}

// ─── Input Event Protocol ───────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t")]
pub enum InputEvent {
    #[serde(rename = "mm")]
    MouseMove { x: i32, y: i32 },
    #[serde(rename = "mc")]
    MouseClick { b: u8 },
    #[serde(rename = "md")]
    MouseDown { b: u8 },
    #[serde(rename = "mu")]
    MouseUp { b: u8 },
    #[serde(rename = "ms")]
    MouseScroll { dx: i32, dy: i32 },
    #[serde(rename = "kd")]
    KeyDown { k: String },
    #[serde(rename = "ku")]
    KeyUp { k: String },
    #[serde(rename = "kt")]
    KeyClick { k: String },
    #[serde(rename = "tx")]
    Text { s: String },
}

impl InputEvent {
    fn apply(&self, enigo: &mut Enigo) -> anyhow::Result<()> {
        match self {
            InputEvent::MouseMove { x, y } => {
                enigo
                    .move_mouse(*x, *y, Coordinate::Abs)
                    .map_err(|e| anyhow::anyhow!("mouse move: {:?}", e))?;
            }
            InputEvent::MouseClick { b } => {
                let btn = button_from_code(*b);
                enigo
                    .button(btn, Direction::Click)
                    .map_err(|e| anyhow::anyhow!("mouse click: {:?}", e))?;
            }
            InputEvent::MouseDown { b } => {
                let btn = button_from_code(*b);
                enigo
                    .button(btn, Direction::Press)
                    .map_err(|e| anyhow::anyhow!("mouse down: {:?}", e))?;
            }
            InputEvent::MouseUp { b } => {
                let btn = button_from_code(*b);
                enigo
                    .button(btn, Direction::Release)
                    .map_err(|e| anyhow::anyhow!("mouse up: {:?}", e))?;
            }
            InputEvent::MouseScroll { dx, dy } => {
                if *dy != 0 {
                    enigo
                        .scroll(*dy, enigo::Axis::Vertical)
                        .map_err(|e| anyhow::anyhow!("scroll: {:?}", e))?;
                }
                if *dx != 0 {
                    enigo
                        .scroll(*dx, enigo::Axis::Horizontal)
                        .map_err(|e| anyhow::anyhow!("scroll: {:?}", e))?;
                }
            }
            InputEvent::KeyDown { k } => {
                if let Some(key) = key_from_str(k) {
                    enigo
                        .key(key, Direction::Press)
                        .map_err(|e| anyhow::anyhow!("key down: {:?}", e))?;
                }
            }
            InputEvent::KeyUp { k } => {
                if let Some(key) = key_from_str(k) {
                    enigo
                        .key(key, Direction::Release)
                        .map_err(|e| anyhow::anyhow!("key up: {:?}", e))?;
                }
            }
            InputEvent::KeyClick { k } => {
                if let Some(key) = key_from_str(k) {
                    enigo
                        .key(key, Direction::Click)
                        .map_err(|e| anyhow::anyhow!("key click: {:?}", e))?;
                }
            }
            InputEvent::Text { s } => {
                enigo
                    .text(s)
                    .map_err(|e| anyhow::anyhow!("text: {:?}", e))?;
            }
        }
        Ok(())
    }
}

fn button_from_code(b: u8) -> Button {
    match b {
        2 => Button::Right,
        3 => Button::Middle,
        _ => Button::Left,
    }
}

fn key_from_str(s: &str) -> Option<Key> {
    match s {
        "Enter" => Some(Key::Return),
        "Tab" => Some(Key::Tab),
        "Space" => Some(Key::Space),
        "Backspace" => Some(Key::Backspace),
        "Escape" => Some(Key::Escape),
        "Shift" => Some(Key::Shift),
        "Control" => Some(Key::Control),
        "Alt" => Some(Key::Alt),
        "Meta" => Some(Key::Meta),
        "Up" => Some(Key::UpArrow),
        "Down" => Some(Key::DownArrow),
        "Left" => Some(Key::LeftArrow),
        "Right" => Some(Key::RightArrow),
        "Home" => Some(Key::Home),
        "End" => Some(Key::End),
        "PageUp" => Some(Key::PageUp),
        "PageDown" => Some(Key::PageDown),
        "Delete" => Some(Key::Delete),
        _ => {
            let c = s.chars().next()?;
            if c.is_ascii() {
                Some(Key::Unicode(c))
            } else {
                None
            }
        }
    }
}

// ─── AppState ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EncoderConfig {
    pub codec: String,       // "h264", "h265", "av1"
    pub backend: String,     // "auto", "nvenc", "vaapi", "videotoolbox", "software"
    pub bitrate: String,     // e.g. "8M"
    pub framerate: u32,      // e.g. 30
    pub gop: u32,            // keyframe interval
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            codec: "h264".to_string(),
            backend: "auto".to_string(),
            bitrate: "8M".to_string(),
            framerate: 30,
            gop: 30,
        }
    }
}

pub struct AppState {
    pub host: Mutex<Option<HostState>>,
    pub host_connections: Arc<AtomicU32>,
    pub host_endpoint: TokioMutex<Option<Endpoint>>,
    pub input_send: TokioMutex<Option<tokio::sync::mpsc::UnboundedSender<InputEvent>>>,
    pub client_endpoint: TokioMutex<Option<Endpoint>>,
    pub webcodecs: std::sync::atomic::AtomicBool,
    pub daemon: bool,
    pub encoder_config: Mutex<EncoderConfig>,
}

pub struct HostState {
    pub node_id: String,
}

impl Default for AppState {
    fn default() -> Self {
        let daemon = std::env::args().any(|a| a == "--daemon");
        Self {
            host: Mutex::new(None),
            host_connections: Arc::new(AtomicU32::new(0)),
            host_endpoint: TokioMutex::new(None),
            input_send: TokioMutex::new(None),
            client_endpoint: TokioMutex::new(None),
            webcodecs: std::sync::atomic::AtomicBool::new(false),
            daemon,
            encoder_config: Mutex::new(EncoderConfig::default()),
        }
    }
}

// ─── FIDO2 ───────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct FidoDeviceInfo {
    pub found: bool,
    pub vid: u16,
    pub pid: u16,
    pub product: String,
    pub versions: Vec<String>,
    pub extensions: Vec<String>,
    pub options: Vec<(String, bool)>,
    pub max_msg_size: u32,
    pub pin_retries: u32,
    pub error: Option<String>,
}

impl Default for FidoDeviceInfo {
    fn default() -> Self {
        Self {
            found: false,
            vid: 0,
            pid: 0,
            product: String::new(),
            versions: vec![],
            extensions: vec![],
            options: vec![],
            max_msg_size: 0,
            pin_retries: 0,
            error: None,
        }
    }
}

#[tauri::command]
pub fn fido_device_info() -> FidoDeviceInfo {
    let devices = ctap_hid_fido2::get_fidokey_devices();
    if devices.is_empty() {
        return FidoDeviceInfo {
            found: false,
            ..Default::default()
        };
    }

    let dev = &devices[0];
    let vid = dev.vid;
    let pid = dev.pid;
    let product = format!("{:?}", dev.info);

    let cfg = Cfg::init();
    match FidoKeyHidFactory::create(&cfg) {
        Ok(device) => {
            let info = match device.get_info() {
                Ok(i) => i,
                Err(e) => {
                    return FidoDeviceInfo {
                        found: true,
                        vid,
                        pid,
                        product,
                        error: Some(format!("get_info failed: {:?}", e)),
                        ..Default::default()
                    };
                }
            };

            let pin_retries = device.get_pin_retries().unwrap_or(0);

            FidoDeviceInfo {
                found: true,
                vid,
                pid,
                product,
                versions: info.versions.clone(),
                extensions: info.extensions.clone(),
                options: info.options.clone(),
                max_msg_size: info.max_msg_size as u32,
                pin_retries: pin_retries as u32,
                error: None,
            }
        }
        Err(e) => FidoDeviceInfo {
            found: true,
            vid,
            pid,
            product,
            error: Some(format!("Failed to open device: {:?}", e)),
            ..Default::default()
        },
    }
}

#[derive(Serialize)]
pub struct PinRetries {
    pub retries: u32,
    pub error: Option<String>,
}

#[tauri::command]
pub fn fido_pin_retries() -> PinRetries {
    let cfg = Cfg::init();
    match FidoKeyHidFactory::create(&cfg) {
        Ok(device) => match device.get_pin_retries() {
            Ok(n) => PinRetries {
                retries: n as u32,
                error: None,
            },
            Err(e) => PinRetries {
                retries: 0,
                error: Some(format!("{:?}", e)),
            },
        },
        Err(e) => PinRetries {
            retries: 0,
            error: Some(format!("Device not found: {:?}", e)),
        },
    }
}

// ─── Titan Identity Derivation ───────────────────────────────────────────────

#[derive(Serialize)]
pub struct TitanIdentity {
    pub node_id: String,
    pub error: Option<String>,
}

#[tauri::command]
pub fn titan_derive_identity(pin: String) -> TitanIdentity {
    match derive_iroh_secret_from_titan(&pin) {
        Ok(secret) => TitanIdentity {
            node_id: secret.public().to_string(),
            error: None,
        },
        Err(e) => TitanIdentity {
            node_id: String::new(),
            error: Some(format!("{:?}", e)),
        },
    }
}

// ─── Daemon mode ─────────────────────────────────────────────────────────────

#[tauri::command]
pub fn is_daemon_mode(state: State<'_, AppState>) -> bool {
    state.daemon
}

#[tauri::command]
pub fn set_webcodecs_available(state: State<'_, AppState>, available: bool) {
    state.webcodecs.store(available, Ordering::SeqCst);
}

#[tauri::command]
pub fn is_webcodecs_available(state: State<'_, AppState>) -> bool {
    state.webcodecs.load(Ordering::SeqCst)
}

#[tauri::command]
pub fn get_encoder_config(state: State<'_, AppState>) -> EncoderConfig {
    state.encoder_config.lock().unwrap().clone()
}

#[tauri::command]
pub fn set_encoder_config(state: State<'_, AppState>, config: EncoderConfig) {
    *state.encoder_config.lock().unwrap() = config;
}

#[tauri::command]
pub fn detect_available_encoders() -> Vec<String> {
    let encoders = std::process::Command::new("ffmpeg")
        .arg("-encoders")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    let all = [
        // H.264
        "h264_nvenc", "h264_vaapi", "h264_qsv", "h264_amf", "h264_videotoolbox", "libx264",
        // H.265
        "hevc_nvenc", "hevc_vaapi", "hevc_qsv", "hevc_amf", "hevc_videotoolbox", "libx265",
        // AV1
        "av1_nvenc", "av1_vaapi", "av1_qsv", "av1_amf", "av1_videotoolbox", "libsvtav1", "libaom-av1",
    ];

    all.iter()
        .filter(|name| encoders.contains(*name))
        .map(|s| s.to_string())
        .collect()
}

// ─── Host Registration (keyring) ─────────────────────────────────────────────

#[derive(Serialize)]
pub struct RegistrationStatus {
    pub registered: bool,
    pub node_id: Option<String>,
    pub error: Option<String>,
}

#[tauri::command]
pub fn host_registration_status() -> RegistrationStatus {
    match load_identity_from_keyring() {
        Ok(Some(bytes)) => {
            let secret = SecretKey::from_bytes(&bytes);
            RegistrationStatus {
                registered: true,
                node_id: Some(secret.public().to_string()),
                error: None,
            }
        }
        Ok(None) => RegistrationStatus {
            registered: false,
            node_id: None,
            error: None,
        },
        Err(e) => RegistrationStatus {
            registered: false,
            node_id: None,
            error: Some(format!("{:?}", e)),
        },
    }
}

#[tauri::command]
pub async fn titan_register_host(pin: String) -> Result<RegistrationStatus, String> {
    let secret = tokio::task::spawn_blocking(move || derive_secret_from_titan(&pin))
        .await
        .map_err(|e| format!("Titan derivation task failed: {}", e))?
        .map_err(|e| format!("Titan derivation failed: {:?}", e))?;

    let node_id = SecretKey::from_bytes(&secret).public().to_string();

    store_identity_in_keyring(&secret).map_err(|e| format!("Keyring store failed: {:?}", e))?;

    Ok(RegistrationStatus {
        registered: true,
        node_id: Some(node_id),
        error: None,
    })
}

#[tauri::command]
pub fn host_unregister() -> Result<bool, String> {
    clear_identity_from_keyring().map_err(|e| format!("Keyring clear failed: {:?}", e))?;
    Ok(true)
}

// ─── Iroh Host ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct HostStatus {
    pub running: bool,
    pub node_id: Option<String>,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn iroh_host_start(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<HostStatus, String> {
    // Read identity from keyring — no Titan needed
    let secret_bytes = load_identity_from_keyring()
        .map_err(|e| format!("Keyring read failed: {:?}", e))?
        .ok_or_else(|| "Not registered. Click 'Register' first.".to_string())?;

    let secret = SecretKey::from_bytes(&secret_bytes);
    let node_id = secret.public().to_string();

    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .bind()
        .await
        .map_err(|e| format!("Failed to bind endpoint: {}", e))?;

    let _ = tokio::time::timeout(Duration::from_secs(5), endpoint.online()).await;

    let connections = state.host_connections.clone();
    let frame_handler = Arc::new(FrameStreamHandler {
        connections: connections.clone(),
        app: app.clone(),
    });
    let input_handler = Arc::new(InputStreamHandler {
        connections: connections.clone(),
        app: app.clone(),
    });

    let router = Router::builder(endpoint.clone())
        .accept(FRAME_ALPN, frame_handler)
        .accept(INPUT_ALPN, input_handler)
        .spawn();

    // Store endpoint and router for clean shutdown
    {
        let mut he = state.host_endpoint.lock().await;
        *he = Some(endpoint);
    }
    std::mem::forget(router);

    let mut host = state.host.lock().map_err(|e| format!("Lock error: {}", e))?;
    *host = Some(HostState {
        node_id: node_id.clone(),
    });

    Ok(HostStatus {
        running: true,
        node_id: Some(node_id),
        error: None,
    })
}

#[tauri::command]
pub async fn iroh_host_stop(state: State<'_, AppState>) -> Result<bool, String> {
    {
        let mut he = state.host_endpoint.lock().await;
        if let Some(endpoint) = he.take() {
            endpoint.close().await;
        }
    }
    {
        let mut host = state.host.lock().map_err(|e| format!("Lock error: {}", e))?;
        *host = None;
    }
    Ok(true)
}

#[tauri::command]
pub fn iroh_host_status(state: State<'_, AppState>) -> HostStatus {
    let host = state.host.lock().ok();
    match host {
        Some(h) => match &*h {
            Some(hs) => HostStatus {
                running: true,
                node_id: Some(hs.node_id.clone()),
                error: None,
            },
            None => HostStatus {
                running: false,
                node_id: None,
                error: None,
            },
        },
        None => HostStatus {
            running: false,
            node_id: None,
            error: Some("State lock poisoned".into()),
        },
    }
}

// ─── Frame Stream Handler (host side) ────────────────────────────────────────

#[derive(Debug)]
struct FrameStreamHandler {
    connections: Arc<AtomicU32>,
    app: AppHandle,
}

impl ProtocolHandler for FrameStreamHandler {
    async fn accept(&self, conn: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let count = self.connections.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.app.emit("host-connections", count);
        eprintln!("[host] client connected: {} (total: {})", conn.remote_id(), count);
        if let Err(e) = stream_frames(conn, &self.app).await {
            eprintln!("[host] stream error: {}", e);
        }
        let count = self.connections.fetch_sub(1, Ordering::SeqCst) - 1;
        let _ = self.app.emit("host-connections", count);
        eprintln!("[host] client disconnected (total: {})", count);
        Ok(())
    }
}

async fn stream_frames(conn: Connection, app: &AppHandle) -> anyhow::Result<()> {
    let (mut send, mut recv) = conn.accept_bi().await?;

    let mut start_buf = [0u8; 1];
    recv.read_exact(&mut start_buf).await?;
    if start_buf[0] != 1 {
        return Ok(());
    }

    // Detect screen dimensions (one-time xcap call)
    let (w, h) = {
        let monitors = xcap::Monitor::all().context("failed to enumerate monitors")?;
        let mon = monitors
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no monitors found"))?;
        let img = mon.capture_image()?;
        (img.width() as usize, img.height() as usize)
    };
    eprintln!("[host] screen resolution: {}x{}", w, h);

    // Try ffmpeg subprocess first
    if ffmpeg_available() {
        let config = {
            let app_state = app.state::<AppState>();
            app_state.encoder_config.lock().unwrap().clone()
        };
        eprintln!(
            "[host] using ffmpeg: codec={} backend={}",
            config.codec, config.backend
        );
        match stream_frames_ffmpeg(&mut send, &mut recv, app, w, h, &config).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                eprintln!("[host] ffmpeg failed: {}, falling back to xcap+openh264", e);
            }
        }
    }

    // Fallback: xcap + openh264
    eprintln!("[host] using xcap + openh264");
    stream_frames_xcap(&mut send, &mut recv, app).await
}

// ─── ffmpeg subprocess path ──────────────────────────────────────────────────

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// Resolve the ffmpeg encoder name based on codec + backend preference.
fn resolve_encoder(codec: &str, backend: &str) -> String {
    let encoders = std::process::Command::new("ffmpeg")
        .arg("-encoders")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    let has = |name: &str| encoders.contains(name);

    // Backend preference order by platform
    let hw_backends: &[&str] = if cfg!(target_os = "macos") {
        &["videotoolbox"]
    } else if cfg!(target_os = "windows") {
        &["nvenc", "qsv", "amf"]
    } else {
        &["nvenc", "vaapi"]
    };

    let codec_prefix = match codec {
        "h265" => "hevc",
        "av1" => "av1",
        _ => "h264",
    };

    // If specific backend requested, try it
    if backend != "auto" {
        let name = if backend == "software" {
            match codec {
                "h265" => "libx265",
                "av1" => "libsvtav1",
                _ => "libx264",
            }
        } else {
            &format!("{}_{}", codec_prefix, backend)
        };
        if has(name) {
            return name.to_string();
        }
    }

    // Auto: try hardware backends in order
    for hw in hw_backends {
        let name = format!("{}_{}", codec_prefix, hw);
        if has(&name) {
            return name;
        }
    }

    // Software fallback
    match codec {
        "h265" => "libx265".to_string(),
        "av1" => {
            if has("libsvtav1") {
                "libsvtav1".to_string()
            } else {
                "libaom-av1".to_string()
            }
        }
        _ => "libx264".to_string(),
    }
}

/// Get the ffmpeg output format and BSF for a given codec.
fn codec_format(codec: &str) -> (&'static str, &'static str) {
    match codec {
        "h265" => ("hevc", "hevc_metadata=aud=insert"),
        "av1" => ("av1", "av1_metadata=td=insert"),
        _ => ("h264", "h264_metadata=aud=insert"),
    }
}

/// Find the next frame delimiter in the NAL stream.
/// H.264/H.265: AUD start code (00 00 00 01 + NAL type 9 for H.264, type 35 for H.265)
/// AV1: temporal delimiter OBU
fn find_next_frame_delim(data: &[u8], from: usize, codec: &str) -> Option<usize> {
    let aud_nal_type = match codec {
        "h265" => 35, // HEVC AUD
        _ => 9,       // H.264 AUD
    };

    let mut i = from;
    while i + 4 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 0 && i + 3 < data.len() && data[i + 3] == 1 {
                // 4-byte start code
                if codec == "av1" {
                    // AV1 temporal delimiter: OBU type 2 (0x12 or 0x22 with extension flag)
                    if i + 4 < data.len() && (data[i + 4] & 0x38) == 0x10 {
                        return Some(i);
                    }
                } else if i + 4 < data.len() && (data[i + 4] & 0x7f) == aud_nal_type {
                    return Some(i);
                }
                i += 4;
                continue;
            }
            if data[i + 2] == 1 {
                // 3-byte start code
                if codec == "av1" {
                    if i + 3 < data.len() && (data[i + 3] & 0x38) == 0x10 {
                        return Some(i);
                    }
                } else if i + 3 < data.len() && (data[i + 3] & 0x7f) == aud_nal_type {
                    return Some(i);
                }
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    None
}

/// Check if a frame's NAL data contains a keyframe.
fn frame_is_keyframe(data: &[u8], codec: &str) -> bool {
    match codec {
        "h265" => {
            // HEVC: IDR (type 19, 20) or VPS (32) or SPS (33)
            let mut i = 0;
            while i + 3 < data.len() {
                if data[i] == 0 && data[i + 1] == 0 {
                    let (sc_len, nal_off) = if data[i + 2] == 0 && i + 3 < data.len() && data[i + 3] == 1 {
                        (4, 4)
                    } else if data[i + 2] == 1 {
                        (3, 3)
                    } else {
                        i += 1;
                        continue;
                    };
                    if i + nal_off < data.len() {
                        let nal_type = (data[i + nal_off] >> 1) & 0x3f;
                        if nal_type == 19 || nal_type == 20 || nal_type == 32 || nal_type == 33 {
                            return true;
                        }
                    }
                    i += sc_len;
                } else {
                    i += 1;
                }
            }
            false
        }
        "av1" => {
            // AV1: key frame has show_existing_frame=0 and frame_type=0 (KEY_FRAME)
            // Simplified: check for any OBU_FRAME with frame_type=0
            // For now, treat first frame as keyframe (AV1 temporal unit delimiters make this reliable)
            true
        }
        _ => {
            // H.264: IDR (type 5) or SPS (type 7)
            let mut i = 0;
            while i + 3 < data.len() {
                if data[i] == 0 && data[i + 1] == 0 {
                    let (sc_len, nal_off) = if data[i + 2] == 0 && i + 3 < data.len() && data[i + 3] == 1 {
                        (4, 4)
                    } else if data[i + 2] == 1 {
                        (3, 3)
                    } else {
                        i += 1;
                        continue;
                    };
                    if i + nal_off < data.len() {
                        let nal_type = data[i + nal_off] & 0x1f;
                        if nal_type == 5 || nal_type == 7 {
                            return true;
                        }
                    }
                    i += sc_len;
                } else {
                    i += 1;
                }
            }
            false
        }
    }
}

async fn stream_frames_ffmpeg(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    app: &AppHandle,
    width: usize,
    height: usize,
    config: &EncoderConfig,
) -> anyhow::Result<()> {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;
    use tokio::process::Command as TokioCommand;

    let encoder = resolve_encoder(&config.codec, &config.backend);
    let (fmt, bsf) = codec_format(&config.codec);
    eprintln!(
        "[host] ffmpeg: encoder={} format={} bsf={} bitrate={} fps={} gop={}",
        encoder, fmt, bsf, config.bitrate, config.framerate, config.gop
    );

    // Notify frontend of active codec for decoder configuration
    let _ = app.emit("host-codec", &config.codec);

    let mut cmd = TokioCommand::new("ffmpeg");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Input: screen capture
    if cfg!(target_os = "macos") {
        cmd.arg("-f").arg("avfoundation")
            .arg("-framerate").arg(config.framerate.to_string())
            .arg("-capture_cursor").arg("1")
            .arg("-i").arg("1:");
    } else if cfg!(target_os = "windows") {
        cmd.arg("-f").arg("gdigrab")
            .arg("-framerate").arg(config.framerate.to_string())
            .arg("-i").arg("desktop");
    } else {
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
        cmd.arg("-f").arg("x11grab")
            .arg("-framerate").arg(config.framerate.to_string())
            .arg("-video_size").arg(format!("{}x{}", width, height))
            .arg("-i").arg(&display);
    }

    // Encoder-specific settings
    match encoder.as_str() {
        e if e.ends_with("_nvenc") => {
            cmd.arg("-c:v").arg(&encoder)
                .arg("-preset").arg("p1")
                .arg("-tune").arg("ll")
                .arg("-rc").arg("cbr")
                .arg("-b:v").arg(&config.bitrate);
        }
        e if e.ends_with("_vaapi") => {
            cmd.arg("-c:v").arg(&encoder)
                .arg("-rc_mode").arg("CBR")
                .arg("-b:v").arg(&config.bitrate);
        }
        e if e.ends_with("_videotoolbox") => {
            cmd.arg("-c:v").arg(&encoder)
                .arg("-realtime").arg("1")
                .arg("-b:v").arg(&config.bitrate);
        }
        e if e.ends_with("_qsv") => {
            cmd.arg("-c:v").arg(&encoder)
                .arg("-preset").arg("veryfast")
                .arg("-b:v").arg(&config.bitrate);
        }
        e if e.ends_with("_amf") => {
            cmd.arg("-c:v").arg(&encoder)
                .arg("-usage").arg("ultralowlatency")
                .arg("-b:v").arg(&config.bitrate);
        }
        "libx264" => {
            cmd.arg("-c:v").arg("libx264")
                .arg("-preset").arg("ultrafast")
                .arg("-tune").arg("zerolatency")
                .arg("-b:v").arg(&config.bitrate);
        }
        "libx265" => {
            cmd.arg("-c:v").arg("libx265")
                .arg("-preset").arg("ultrafast")
                .arg("-tune").arg("zerolatency")
                .arg("-x265-params").arg("keyint=30:min-keyint=30")
                .arg("-b:v").arg(&config.bitrate);
        }
        "libsvtav1" => {
            cmd.arg("-c:v").arg("libsvtav1")
                .arg("-preset").arg("8")
                .arg("-crf").arg("35")
                .arg("-g").arg(config.gop.to_string());
        }
        "libaom-av1" => {
            cmd.arg("-c:v").arg("libaom-av1")
                .arg("-cpu-used").arg("8")
                .arg("-crf").arg("35")
                .arg("-b:v").arg(&config.bitrate);
        }
        _ => {
            return Err(anyhow::anyhow!("unsupported encoder: {}", encoder));
        }
    }

    // Common output settings (skip gop for libx265/libsvtav1 which set it above)
    if encoder != "libx265" && encoder != "libsvtav1" {
        cmd.arg("-g").arg(config.gop.to_string());
    }
    cmd.arg("-bf").arg("0")
        .arg("-pix_fmt").arg("yuv420p")
        .arg("-bsf:v").arg(bsf)
        .arg("-f").arg(fmt)
        .arg("-");

    let mut child = cmd.spawn().context("failed to spawn ffmpeg")?;
    let mut stdout = child.stdout.take().context("no stdout from ffmpeg")?;
    let mut stderr = child.stderr.take();
    let mut stderr_buf = String::new();

    let mut buf: Vec<u8> = Vec::with_capacity(65536);
    let mut frame_start: usize = 0;
    let mut first_delim_seen = false;
    let mut frame_count = 0u32;
    let start = Instant::now();
    let mut last_frame_time = Instant::now();

    loop {
        let mut tmp = [0u8; 16384];
        let n = stdout.read(&mut tmp).await?;
        if n == 0 {
            // Capture stderr for diagnostics
            if let Some(ref mut stderr) = stderr {
                use tokio::io::AsyncReadExt;
                let _ = stderr.read_to_string(&mut stderr_buf).await;
            }
            eprintln!("[host] ffmpeg stdout closed");
            if !stderr_buf.is_empty() {
                eprintln!("[host] ffmpeg stderr: {}", stderr_buf);
            }
            break;
        }
        buf.extend_from_slice(&tmp[..n]);

        // Extract complete frames (delimited by AUD/TD NAL units)
        loop {
            let search_from = if first_delim_seen { frame_start + 6 } else { 0 };
            match find_next_frame_delim(&buf, search_from, &config.codec) {
                Some(delim_pos) => {
                    if first_delim_seen {
                        let frame_data = &buf[frame_start..delim_pos];
                        let is_keyframe = frame_is_keyframe(frame_data, &config.codec);
                        let frame_size = frame_data.len();

                        let now = Instant::now();
                        let frame_ms = now.duration_since(last_frame_time).as_secs_f64() * 1000.0;
                        last_frame_time = now;

                        // Header: width(4) + height(4) + size(4) + keyframe(1) + codec(1) = 14 bytes
                        let header = [
                            (width as u32).to_be_bytes(),
                            (height as u32).to_be_bytes(),
                            (frame_size as u32).to_be_bytes(),
                        ]
                        .concat();
                        let kf_byte = if is_keyframe { 1u8 } else { 0u8 };
                        let codec_byte = codec_to_byte(&config.codec);

                        send.write_all(&header).await?;
                        send.write_all(&[kf_byte, codec_byte]).await?;
                        send.write_all(frame_data).await?;

                        frame_count += 1;
                        let elapsed = start.elapsed();
                        let fps = frame_count as f64 / elapsed.as_secs_f64().max(0.001);

                        let _ = app.emit(
                            "host-encode-stats",
                            serde_json::json!({
                                "frame": frame_count,
                                "encode_ms": (frame_ms * 10.0).round() / 10.0,
                                "capture_ms": 0.0,
                                "size_bytes": frame_size,
                                "fps": (fps * 10.0).round() / 10.0,
                                "keyframe": is_keyframe,
                                "encoder": encoder.clone(),
                            }),
                        );

                        eprintln!(
                            "[host] frame={} {}x{} {}={}B kf={} fps={:.1} ftime={:.1}ms",
                            frame_count, width, height, config.codec, frame_size, is_keyframe, fps, frame_ms
                        );
                    }
                    frame_start = delim_pos;
                    first_delim_seen = true;
                }
                None => break,
            }
        }

        // Trim consumed data from buffer
        if frame_start > 0 && frame_start >= buf.len() / 2 {
            buf.drain(..frame_start);
            frame_start = 0;
        }

        // Check for client disconnect
        match tokio::time::timeout(Duration::from_millis(1), recv.read(&mut [0u8; 1])).await {
            Ok(Ok(Some(_))) => {
                eprintln!("[host] client disconnected");
                break;
            }
            _ => {}
        }
    }

    let _ = child.kill().await;
    Ok(())
}

// ─── xcap + openh264 fallback path ───────────────────────────────────────────

async fn stream_frames_xcap(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    app: &AppHandle,
) -> anyhow::Result<()> {
    let monitors = xcap::Monitor::all().context("failed to enumerate monitors")?;
    let monitor = monitors
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no monitors found"))?;

    let mut encoder = Encoder::new().context("failed to create H.264 encoder")?;

    let mut frame_count = 0u32;
    let start = Instant::now();

    loop {
        let capture_start = Instant::now();
        let image = monitor.capture_image()?;
        let capture_ms = capture_start.elapsed().as_secs_f64() * 1000.0;

        let rgb_image = image::DynamicImage::ImageRgba8(image).to_rgb8();
        let (w, h) = (rgb_image.width() as usize, rgb_image.height() as usize);

        // RGB8 → YUV → H.264
        let encode_start = Instant::now();
        let rgb_source = RgbSliceU8::new(rgb_image.as_raw(), (w, h));
        let yuv = YUVBuffer::from_rgb8_source(rgb_source);
        let (h264_data, is_keyframe) = {
            let bitstream = encoder.encode(&yuv).context("H.264 encode failed")?;
            let kf = matches!(bitstream.frame_type(), openh264::encoder::FrameType::I | openh264::encoder::FrameType::IDR);
            (bitstream.to_vec(), kf)
        };
        let encode_ms = encode_start.elapsed().as_secs_f64() * 1000.0;
        let h264_size = h264_data.len();

        // Header: width(4) + height(4) + size(4) + keyframe(1) + codec(1) = 14 bytes
        let header = [
            (w as u32).to_be_bytes(),
            (h as u32).to_be_bytes(),
            (h264_size as u32).to_be_bytes(),
        ]
        .concat();
        let kf_byte = if is_keyframe { 1u8 } else { 0u8 };

        send.write_all(&header).await?;
        send.write_all(&[kf_byte, 0u8]).await?; // xcap path is always h264
        send.write_all(&h264_data).await?;

        frame_count += 1;
        let elapsed = start.elapsed();
        let fps = frame_count as f64 / elapsed.as_secs_f64().max(0.001);
        eprintln!(
            "[host] frame={} {}x{} h264={}B kf={} fps={:.1} enc={:.1}ms cap={:.1}ms",
            frame_count, w, h, h264_size, is_keyframe, fps, encode_ms, capture_ms
        );

        let _ = app.emit(
            "host-encode-stats",
            serde_json::json!({
                "frame": frame_count,
                "encode_ms": (encode_ms * 10.0).round() / 10.0,
                "capture_ms": (capture_ms * 10.0).round() / 10.0,
                "size_bytes": h264_size,
                "fps": (fps * 10.0).round() / 10.0,
                "keyframe": is_keyframe,
            }),
        );

        match tokio::time::timeout(Duration::from_millis(1), recv.read(&mut [0u8; 1])).await {
            Ok(Ok(Some(_))) => {
                eprintln!("[host] client disconnected");
                break;
            }
            _ => {}
        }

        tokio::task::yield_now().await;
    }

    Ok(())
}

// ─── Input Stream Handler (host side) ─────────────────────────────────────────

#[derive(Debug)]
struct InputStreamHandler {
    connections: Arc<AtomicU32>,
    app: AppHandle,
}

impl ProtocolHandler for InputStreamHandler {
    async fn accept(&self, conn: Connection) -> Result<(), iroh::protocol::AcceptError> {
        if let Err(e) = handle_input(conn).await {
            eprintln!("[host] input error: {}", e);
        }
        Ok(())
    }
}

async fn handle_input(conn: Connection) -> anyhow::Result<()> {
    let (mut send, mut recv) = conn.accept_bi().await?;
    eprintln!("[host] input client connected: {}", conn.remote_id());

    let mut start_buf = [0u8; 1];
    recv.read_exact(&mut start_buf).await?;
    if start_buf[0] != 1 {
        return Ok(());
    }

    let (tx, rx) = std::sync::mpsc::channel::<InputEvent>();
    std::thread::spawn(move || {
        let mut enigo = match Enigo::new(&Settings::default()) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[host] enigo init: {:?}", e);
                return;
            }
        };
        for event in rx {
            eprintln!("[host] input: {:?}", event);
            if let Err(e) = event.apply(&mut enigo) {
                eprintln!("[host] inject error: {}", e);
            }
        }
    });

    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];

    loop {
        let n = match recv.read(&mut chunk).await {
            Ok(Some(n)) => n,
            Ok(None) => {
                eprintln!("[host] input client disconnected");
                break;
            }
            Err(e) => {
                eprintln!("[host] input read error: {}", e);
                break;
            }
        };
        if n == 0 {
            break;
        }

        buf.extend_from_slice(&chunk[..n]);

        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line_str = String::from_utf8_lossy(&line[..line.len() - 1]);

            if line_str.trim().is_empty() {
                continue;
            }

            match serde_json::from_str::<InputEvent>(&line_str) {
                Ok(event) => {
                    let _ = tx.send(event);
                }
                Err(e) => {
                    eprintln!("[host] parse error: {} (line: {})", e, line_str);
                }
            }
        }
    }

    drop(tx);
    let _ = send.write_all(b"bye\n").await;
    Ok(())
}

// ─── Iroh Client ─────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct FramePayload {
    pub width: u32,
    pub height: u32,
    pub data: String,
    pub keyframe: bool,
    pub codec: String,
}

#[derive(Serialize)]
pub struct ConnectResult {
    pub connected: bool,
    pub host_node_id: Option<String>,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn iroh_client_connect(
    app: AppHandle,
    state: State<'_, AppState>,
    pin: String,
) -> Result<ConnectResult, String> {
    // Derive the HOST's node ID from the Titan (same key = same node ID)
    let host_secret = tokio::task::spawn_blocking(move || derive_iroh_secret_from_titan(&pin))
        .await
        .map_err(|e| format!("Titan derivation task failed: {}", e))?
        .map_err(|e| format!("Titan derivation failed: {:?}", e))?;

    let host_node_id = host_secret.public();

    // Client uses a random identity (can't connect to yourself)
    let client_secret = SecretKey::generate();
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(client_secret)
        .bind()
        .await
        .map_err(|e| format!("Failed to bind endpoint: {}", e))?;

    let _ = tokio::time::timeout(Duration::from_secs(10), endpoint.online()).await;

    // Connect via relay using only the derived node ID — no address JSON
    let addr = iroh::EndpointAddr::new(host_node_id)
        .with_relay_url(N0_RELAY.parse().map_err(|e| format!("Invalid relay URL: {}", e))?);

    // Connect frame stream
    let frame_conn = endpoint
        .connect(addr.clone(), FRAME_ALPN)
        .await
        .map_err(|e| format!("Failed to connect frame stream: {}", e))?;

    let (mut frame_send, mut frame_recv) = frame_conn
        .open_bi()
        .await
        .map_err(|e| format!("Failed to open frame stream: {}", e))?;

    frame_send
        .write_all(&[1u8])
        .await
        .map_err(|e| format!("Failed to send start: {}", e))?;

    // Connect input stream
    let input_conn = endpoint
        .connect(addr, INPUT_ALPN)
        .await
        .map_err(|e| format!("Failed to connect input stream: {}", e))?;

    let (mut input_send, mut _input_recv) = input_conn
        .open_bi()
        .await
        .map_err(|e| format!("Failed to open input stream: {}", e))?;

    input_send
        .write_all(&[1u8])
        .await
        .map_err(|e| format!("Failed to send input start: {}", e))?;

    // Store input sender in state
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
    {
        let mut input_send_guard = state.input_send.lock().await;
        *input_send_guard = Some(tx);
    }

    // Store endpoint for disconnect
    {
        let mut ce = state.client_endpoint.lock().await;
        *ce = Some(endpoint.clone());
    }

    // Spawn input forwarder
    let mut input_stream = input_send;
    tokio::spawn(async move {
        let mut rx = rx;
        while let Some(event) = rx.recv().await {
            let json = match serde_json::to_string(&event) {
                Ok(j) => j + "\n",
                Err(e) => {
                    eprintln!("[client] serialize error: {}", e);
                    continue;
                }
            };
            if input_stream.write_all(json.as_bytes()).await.is_err() {
                eprintln!("[client] input stream write failed, disconnecting");
                break;
            }
        }
        let _ = input_stream.finish();
    });

    // Spawn frame reader — dual path based on WebCodecs availability
    let use_webcodecs = state.webcodecs.load(Ordering::SeqCst);
    tokio::spawn(async move {
        let mut frame_count = 0u32;
        let start = Instant::now();

        // Initialize software decoder only if WebCodecs is NOT available
        let mut decoder = if use_webcodecs {
            None
        } else {
            match openh264::decoder::Decoder::new() {
                Ok(d) => Some(d),
                Err(e) => {
                    let _ = app.emit("frame-error", format!("Decoder init failed: {}", e));
                    return;
                }
            }
        };

        loop {
            let mut header = [0u8; 14];
            if frame_recv.read_exact(&mut header).await.is_err() {
                let _ = app.emit("frame-error", "Connection lost");
                break;
            }

            let w = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
            let h = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
            let h264_len =
                u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;
            let is_keyframe = header[12] == 1;
            let codec = byte_to_codec(header[13]).to_string();

            let mut h264_buf = vec![0u8; h264_len];
            if frame_recv.read_exact(&mut h264_buf).await.is_err() {
                let _ = app.emit("frame-error", "Connection lost");
                break;
            }

            if use_webcodecs {
                // Path 1: Pass raw H.264 to frontend — browser decodes via WebCodecs
                let b64 = base64::engine::general_purpose::STANDARD.encode(&h264_buf);
                let _ = app.emit(
                    "frame",
                    FramePayload { width: w, height: h, data: b64, keyframe: is_keyframe, codec: codec.clone() },
                );

                frame_count += 1;
                let elapsed = start.elapsed();
                let fps = frame_count as f64 / elapsed.as_secs_f64().max(0.001);
                let _ = app.emit(
                    "frame-stats",
                    serde_json::json!({ "fps": fps, "count": frame_count, "keyframe": is_keyframe }),
                );
            } else if let Some(ref mut dec) = decoder {
                // Path 2: Software decode → JPEG
                for nal in nal_units(&h264_buf) {
                    if let Ok(Some(yuv)) = dec.decode(nal) {
                        let (yw, yh) = yuv.dimensions();
                        let rgb_len = yuv.rgb8_len();
                        let mut rgb_raw = vec![0u8; rgb_len];
                        yuv.write_rgb8(&mut rgb_raw);

                        let img = match image::RgbImage::from_raw(yw as u32, yh as u32, rgb_raw) {
                            Some(img) => img,
                            None => continue,
                        };
                        let mut jpeg_buf = Vec::with_capacity(30_000);
                        if image::DynamicImage::ImageRgb8(img)
                            .write_to(&mut Cursor::new(&mut jpeg_buf), image::ImageFormat::Jpeg)
                            .is_err()
                        {
                            continue;
                        }

                        let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg_buf);
                        let _ = app.emit(
                            "frame",
                            FramePayload { width: yw as u32, height: yh as u32, data: b64, keyframe: is_keyframe, codec: codec.clone() },
                        );

                        frame_count += 1;
                        let elapsed = start.elapsed();
                        let fps = frame_count as f64 / elapsed.as_secs_f64().max(0.001);
                        let _ = app.emit(
                            "frame-stats",
                            serde_json::json!({ "fps": fps, "count": frame_count, "keyframe": is_keyframe }),
                        );
                    }
                }
            }

            tokio::task::yield_now().await;
        }

        drop(endpoint);
    });

    Ok(ConnectResult {
        connected: true,
        host_node_id: Some(host_node_id.to_string()),
        error: None,
    })
}

#[tauri::command]
pub async fn iroh_client_send_input(
    state: State<'_, AppState>,
    event: InputEvent,
) -> Result<bool, String> {
    let input_send = state.input_send.lock().await;
    match &*input_send {
        Some(tx) => {
            tx.send(event)
                .map_err(|e| format!("Input channel closed: {}", e))?;
            Ok(true)
        }
        None => Err("Not connected to host".into()),
    }
}

#[tauri::command]
pub async fn iroh_client_disconnect(state: State<'_, AppState>) -> Result<bool, String> {
    // Close endpoint — kills all streams
    {
        let mut ce = state.client_endpoint.lock().await;
        if let Some(endpoint) = ce.take() {
            endpoint.close().await;
        }
    }
    // Clear input channel
    {
        let mut input_send = state.input_send.lock().await;
        *input_send = None;
    }
    Ok(true)
}
