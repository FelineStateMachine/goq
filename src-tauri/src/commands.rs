//! Tauri backend commands for Keyhome.
//!
//! Commands:
//! - fido_device_info: enumerate FIDO2 devices and get info
//! - fido_pin_retries: check PIN retry count
//! - iroh_host_start: start Iroh endpoint, stream screen frames + accept input
//! - iroh_host_status: check if host endpoint is running
//! - iroh_client_connect: connect to host, receive frames, emit events
//! - iroh_client_send_input: send input event to host for injection

use anyhow::Context as _;
use base64::Engine;
use ctap_hid_fido2::{Cfg, FidoKeyHidFactory};
use enigo::{Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use iroh::endpoint::{presets, Connection};
use iroh::protocol::{ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, SecretKey};
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::Mutex as TokioMutex;

const FRAME_ALPN: &[u8] = b"keyhome/frame-stream/0";
const INPUT_ALPN: &[u8] = b"keyhome/input-stream/0";

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
    pub addr_json: String,
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

// ─── Iroh Host ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct HostStatus {
    pub running: bool,
    pub addr_json: Option<String>,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn iroh_host_start(state: State<'_, AppState>) -> Result<HostStatus, String> {
    let secret = SecretKey::generate();
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .bind()
        .await
        .map_err(|e| format!("Failed to bind endpoint: {}", e))?;

    let _ = tokio::time::timeout(Duration::from_secs(5), endpoint.online()).await;

    let addr = endpoint.addr();
    let addr_json =
        serde_json::to_string(&addr).map_err(|e| format!("Failed to serialize addr: {}", e))?;

    // Frame stream handler (existing)
    let frame_handler = Arc::new(FrameStreamHandler);
    // Input handler (new)
    let input_handler = Arc::new(InputStreamHandler);

    let router = Router::builder(endpoint.clone())
        .accept(FRAME_ALPN, frame_handler)
        .accept(INPUT_ALPN, input_handler)
        .spawn();

    // Keep endpoint and router alive by leaking — MVP tradeoff
    std::mem::forget(endpoint);
    std::mem::forget(router);

    let mut host = state.host.lock().map_err(|e| format!("Lock error: {}", e))?;
    *host = Some(HostState {
        addr_json: addr_json.clone(),
    });

    Ok(HostStatus {
        running: true,
        addr_json: Some(addr_json),
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
                addr_json: Some(hs.addr_json.clone()),
                error: None,
            },
            None => HostStatus {
                running: false,
                addr_json: None,
                error: None,
            },
        },
        None => HostStatus {
            running: false,
            addr_json: None,
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

    // Wait for client start signal
    let mut start_buf = [0u8; 1];
    recv.read_exact(&mut start_buf).await?;
    if start_buf[0] != 1 {
        return Ok(());
    }

    // Capture screen
    let monitors = xcap::Monitor::all().context("failed to enumerate monitors")?;
    let monitor = monitors
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no monitors found"))?;

    let mut frame_count = 0u32;
    let start = Instant::now();

    loop {
        let image = monitor.capture_image()?;
        let rgb_image = image::DynamicImage::ImageRgba8(image).to_rgb8();
        let (w, h) = (rgb_image.width(), rgb_image.height());

        let mut jpeg_buf = Vec::with_capacity(50_000);
        rgb_image.write_to(&mut Cursor::new(&mut jpeg_buf), image::ImageFormat::Jpeg)?;
        let jpeg_size = jpeg_buf.len();

        let header = [
            (w as u32).to_be_bytes(),
            (h as u32).to_be_bytes(),
            (jpeg_size as u32).to_be_bytes(),
        ]
        .concat();

        send.write_all(&header).await?;
        send.write_all(&jpeg_buf).await?;

        frame_count += 1;
        let elapsed = start.elapsed();
        let fps = frame_count as f64 / elapsed.as_secs_f64().max(0.001);
        eprintln!(
            "[host] frame={} {}x{} jpeg={}B fps={:.1}",
            frame_count, w, h, jpeg_size, fps
        );

        // Check for client disconnect
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

    // Wait for start signal
    let mut start_buf = [0u8; 1];
    recv.read_exact(&mut start_buf).await?;
    if start_buf[0] != 1 {
        return Ok(());
    }

    // Enigo is not Send on macOS (holds NonNull<CGEventSource>).
    // Run it in a dedicated blocking thread, fed via a std channel.
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

        // Process complete newline-delimited JSON events
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

    // Drop tx to signal the blocking thread to exit
    drop(tx);
    let _ = send.write_all(b"bye\n").await;
    Ok(())
}

// ─── Iroh Client ─────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct FramePayload {
    pub width: u32,
    pub height: u32,
    pub data: String, // base64 JPEG
}

#[derive(Serialize)]
pub struct ConnectResult {
    pub connected: bool,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn iroh_client_connect(
    app: AppHandle,
    state: State<'_, AppState>,
    addr_json: String,
) -> Result<ConnectResult, String> {
    let addr: EndpointAddr = serde_json::from_str(&addr_json)
        .map_err(|e| format!("Invalid address: {}", e))?;

    let secret = SecretKey::generate();
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .bind()
        .await
        .map_err(|e| format!("Failed to bind endpoint: {}", e))?;

    let _ = tokio::time::timeout(Duration::from_secs(10), endpoint.online()).await;

    // Connect frame stream
    let frame_conn = endpoint
        .connect(addr.clone(), FRAME_ALPN)
        .await
        .map_err(|e| format!("Failed to connect frame stream: {}", e))?;

    let (mut frame_send, mut frame_recv) = frame_conn
        .open_bi()
        .await
        .map_err(|e| format!("Failed to open frame stream: {}", e))?;

    // Send start signal
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

    // Send start signal for input
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

    // Spawn input forwarder: reads from channel, writes to Iroh stream
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

    // Spawn background task to read frames and emit events
    tokio::spawn(async move {
        let mut frame_count = 0u32;
        let start = Instant::now();

        loop {
            let mut header = [0u8; 12];
            if frame_recv.read_exact(&mut header).await.is_err() {
                let _ = app.emit("frame-error", "Connection lost");
                break;
            }

            let w = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
            let h = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
            let jpeg_len =
                u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;

            let mut jpeg_buf = vec![0u8; jpeg_len];
            if frame_recv.read_exact(&mut jpeg_buf).await.is_err() {
                let _ = app.emit("frame-error", "Connection lost");
                break;
            }

            frame_count += 1;
            let elapsed = start.elapsed();
            let fps = frame_count as f64 / elapsed.as_secs_f64().max(0.001);

            let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg_buf);
            let _ = app.emit(
                "frame",
                FramePayload {
                    width: w,
                    height: h,
                    data: b64,
                },
            );
            let _ = app.emit(
                "frame-stats",
                serde_json::json!({ "fps": fps, "count": frame_count }),
            );
        }

        drop(endpoint);
    });

    Ok(ConnectResult {
        connected: true,
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
