//! Tauri backend commands for Keyhome.
//!
//! Commands:
//! - fido_device_info: enumerate FIDO2 devices and get info
//! - fido_pin_retries: check PIN retry count
//! - iroh_host_start: start Iroh endpoint, stream screen frames to clients
//! - iroh_host_status: check if host endpoint is running
//! - iroh_client_connect: connect to a host, receive frames, emit "frame" events

use anyhow::Context as _;
use base64::Engine;
use ctap_hid_fido2::{Cfg, FidoKeyHidFactory};
use iroh::endpoint::{presets, Connection};
use iroh::protocol::{ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, SecretKey};
use serde::Serialize;
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ALPN: &[u8] = b"keyhome/frame-stream/0";

// ─── AppState ────────────────────────────────────────────────────────────────

pub struct AppState {
    pub host: Mutex<Option<HostState>>,
}

pub struct HostState {
    pub addr_json: String,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            host: Mutex::new(None),
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

    let handler = Arc::new(FrameStreamHandler);
    let _router = Router::builder(endpoint.clone())
        .accept(ALPN, handler)
        .spawn();

    // Keep endpoint alive by leaking — MVP tradeoff
    std::mem::forget(endpoint);
    std::mem::forget(_router);

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
    _state: State<'_, AppState>,
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

    let conn = endpoint
        .connect(addr, ALPN)
        .await
        .map_err(|e| format!("Failed to connect: {}", e))?;

    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| format!("Failed to open stream: {}", e))?;

    // Send start signal
    send.write_all(&[1u8])
        .await
        .map_err(|e| format!("Failed to send start: {}", e))?;

    // Spawn background task to read frames and emit events
    tokio::spawn(async move {
        let mut frame_count = 0u32;
        let start = Instant::now();

        loop {
            let mut header = [0u8; 12];
            if recv.read_exact(&mut header).await.is_err() {
                let _ = app.emit("frame-error", "Connection lost");
                break;
            }

            let w = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
            let h = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
            let jpeg_len =
                u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;

            let mut jpeg_buf = vec![0u8; jpeg_len];
            if recv.read_exact(&mut jpeg_buf).await.is_err() {
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
            let _ = app.emit("frame-stats", serde_json::json!({ "fps": fps, "count": frame_count }));
        }

        // Keep endpoint alive until task ends
        drop(endpoint);
    });

    Ok(ConnectResult {
        connected: true,
        error: None,
    })
}
