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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::Mutex as TokioMutex;

const FRAME_ALPN: &[u8] = b"keyhome/frame-stream/0";
const INPUT_ALPN: &[u8] = b"keyhome/input-stream/0";
const RPID: &str = "keyhome";
const SALT_MESSAGE: &str = "keyhome-iroh-identity-v1";
const N0_RELAY: &str = "https://usw1-1.relay.n0.iroh.link./";
const KEYRING_SERVICE: &str = "keyhome";
const KEYRING_ENTRY: &str = "host-identity";

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

pub struct AppState {
    pub host: Mutex<Option<HostState>>,
    pub input_send: TokioMutex<Option<tokio::sync::mpsc::UnboundedSender<InputEvent>>>,
}

pub struct HostState {
    pub node_id: String,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            host: Mutex::new(None),
            input_send: TokioMutex::new(None),
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
pub async fn iroh_host_start(state: State<'_, AppState>) -> Result<HostStatus, String> {
    // Read identity from keyring — no Titan needed
    let secret_bytes = load_identity_from_keyring()
        .map_err(|e| format!("Keyring read failed: {:?}", e))?
        .ok_or_else(|| "Not registered. Click 'Register Key' first.".to_string())?;

    let secret = SecretKey::from_bytes(&secret_bytes);
    let node_id = secret.public().to_string();

    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .bind()
        .await
        .map_err(|e| format!("Failed to bind endpoint: {}", e))?;

    let _ = tokio::time::timeout(Duration::from_secs(5), endpoint.online()).await;

    let frame_handler = Arc::new(FrameStreamHandler);
    let input_handler = Arc::new(InputStreamHandler);

    let router = Router::builder(endpoint.clone())
        .accept(FRAME_ALPN, frame_handler)
        .accept(INPUT_ALPN, input_handler)
        .spawn();

    std::mem::forget(endpoint);
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
struct FrameStreamHandler;

impl ProtocolHandler for FrameStreamHandler {
    async fn accept(&self, conn: Connection) -> Result<(), iroh::protocol::AcceptError> {
        if let Err(e) = stream_frames(conn).await {
            eprintln!("[host] stream error: {}", e);
        }
        Ok(())
    }
}

async fn stream_frames(conn: Connection) -> anyhow::Result<()> {
    let (mut send, mut recv) = conn.accept_bi().await?;

    let mut start_buf = [0u8; 1];
    recv.read_exact(&mut start_buf).await?;
    if start_buf[0] != 1 {
        return Ok(());
    }

    let monitors = xcap::Monitor::all().context("failed to enumerate monitors")?;
    let monitor = monitors
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no monitors found"))?;

    let mut encoder = Encoder::new().context("failed to create H.264 encoder")?;

    let mut frame_count = 0u32;
    let start = Instant::now();

    loop {
        let image = monitor.capture_image()?;
        let rgb_image = image::DynamicImage::ImageRgba8(image).to_rgb8();
        let (w, h) = (rgb_image.width() as usize, rgb_image.height() as usize);

        // RGB8 → YUV → H.264
        let rgb_source = RgbSliceU8::new(rgb_image.as_raw(), (w, h));
        let yuv = YUVBuffer::from_rgb8_source(rgb_source);
        let (h264_data, is_keyframe) = {
            let bitstream = encoder.encode(&yuv).context("H.264 encode failed")?;
            let kf = matches!(bitstream.frame_type(), openh264::encoder::FrameType::I | openh264::encoder::FrameType::IDR);
            (bitstream.to_vec(), kf)
        };
        let h264_size = h264_data.len();

        // Header: width(4) + height(4) + size(4) + is_keyframe(1)
        let kf_byte = [0u8, 0u8, 0u8, if is_keyframe { 1u8 } else { 0u8 }];
        let header = [
            (w as u32).to_be_bytes(),
            (h as u32).to_be_bytes(),
            (h264_size as u32).to_be_bytes(),
            kf_byte,
        ]
        .concat();

        send.write_all(&header).await?;
        send.write_all(&h264_data).await?;

        frame_count += 1;
        let elapsed = start.elapsed();
        let fps = frame_count as f64 / elapsed.as_secs_f64().max(0.001);
        eprintln!(
            "[host] frame={} {}x{} h264={}B kf={} fps={:.1}",
            frame_count, w, h, h264_size, is_keyframe, fps
        );

        match tokio::time::timeout(Duration::from_millis(1), recv.read(&mut [0u8; 1])).await {
            Ok(Ok(Some(_))) => {
                eprintln!("[host] client disconnected");
                break;
            }
            _ => {}
        }

        tokio::time::sleep(Duration::from_millis(33)).await;
    }

    Ok(())
}

// ─── Input Stream Handler (host side) ─────────────────────────────────────────

#[derive(Debug)]
struct InputStreamHandler;

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

    // Spawn frame reader (H.264 decode → JPEG for canvas)
    tokio::spawn(async move {
        let mut decoder = match openh264::decoder::Decoder::new() {
            Ok(d) => d,
            Err(e) => {
                let _ = app.emit("frame-error", format!("Decoder init failed: {}", e));
                return;
            }
        };

        let mut frame_count = 0u32;
        let start = Instant::now();

        loop {
            // Header: width(4) + height(4) + size(4) + is_keyframe(1) = 13 bytes
            let mut header = [0u8; 13];
            if frame_recv.read_exact(&mut header).await.is_err() {
                let _ = app.emit("frame-error", "Connection lost");
                break;
            }

            let _w = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
            let _h = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
            let h264_len =
                u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;
            let _is_keyframe = header[12] == 1;

            let mut h264_buf = vec![0u8; h264_len];
            if frame_recv.read_exact(&mut h264_buf).await.is_err() {
                let _ = app.emit("frame-error", "Connection lost");
                break;
            }

            // Decode H.264 → YUV → RGB8 → JPEG
            // Split into individual NAL units — decoder needs them separately
            for nal in nal_units(&h264_buf) {
                match decoder.decode(nal) {
                    Ok(Some(yuv)) => {
                        let (yw, yh) = yuv.dimensions();
                        let rgb_len = yuv.rgb8_len();
                        let mut rgb_raw = vec![0u8; rgb_len];
                        yuv.write_rgb8(&mut rgb_raw);

                        // Re-encode as JPEG for canvas
                        let img = image::RgbImage::from_raw(yw as u32, yh as u32, rgb_raw);
                        if let Some(img) = img {
                            let mut jpeg_buf = Vec::with_capacity(30_000);
                            if image::DynamicImage::ImageRgb8(img)
                                .write_to(&mut Cursor::new(&mut jpeg_buf), image::ImageFormat::Jpeg)
                                .is_ok()
                            {
                                let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg_buf);
                                let _ = app.emit(
                                    "frame",
                                    FramePayload {
                                        width: yw as u32,
                                        height: yh as u32,
                                        data: b64,
                                    },
                                );
                            }
                        }

                        frame_count += 1;
                        let elapsed = start.elapsed();
                        let fps = frame_count as f64 / elapsed.as_secs_f64().max(0.001);
                        let _ = app.emit(
                            "frame-stats",
                            serde_json::json!({ "fps": fps, "count": frame_count }),
                        );
                    }
                    Ok(None) => {
                        // Need more NALs — SPS/PPS or waiting for I-frame
                    }
                    Err(e) => {
                        eprintln!("[client] decode error: {}", e);
                    }
                }
            }
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
