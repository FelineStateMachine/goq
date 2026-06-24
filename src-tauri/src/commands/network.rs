use base64::Engine;
use iroh::{endpoint::presets, Endpoint, SecretKey};
use iroh::protocol::Router;
use openh264::{formats::YUVSource, nal_units};
use serde::Serialize;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};
use super::auth::{derive_iroh_secret_from_key, load_identity_from_keyring};
use super::input::{InputEvent, InputStreamHandler};
use super::state::{AppState, HostState, INPUT_ALPN, FRAME_ALPN};
use super::streaming::{byte_to_codec, FrameStreamHandler};

// ─── Host commands ────────────────────────────────────────────────────────────

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
    let input_handler = Arc::new(InputStreamHandler);

    let router = Router::builder(endpoint.clone())
        .accept(FRAME_ALPN, frame_handler)
        .accept(INPUT_ALPN, input_handler)
        .spawn();

    {
        let mut he = state.host_endpoint.lock().await;
        *he = Some(endpoint);
    }
    std::mem::forget(router);

    let mut host = state.host.lock().map_err(|e| format!("Lock error: {}", e))?;
    *host = Some(HostState { node_id: node_id.clone() });

    Ok(HostStatus { running: true, node_id: Some(node_id), error: None })
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
            Some(hs) => HostStatus { running: true, node_id: Some(hs.node_id.clone()), error: None },
            None => HostStatus { running: false, node_id: None, error: None },
        },
        None => HostStatus {
            running: false,
            node_id: None,
            error: Some("State lock poisoned".into()),
        },
    }
}

// ─── Client commands ──────────────────────────────────────────────────────────

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
    // FIDO2 derivation — 30s timeout so a missing/stuck key surfaces quickly
    let host_secret = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || derive_iroh_secret_from_key(&pin)),
    )
    .await
    .map_err(|_| "Security key timed out (30s). Make sure your key is connected.".to_string())?
    .map_err(|e| format!("Task failed: {}", e))?
    .map_err(|e| format!("FIDO2 error: {:?}", e))?;

    let host_node_id = host_secret.public();

    // Key has been tapped — relay connection is next; update the UI overlay
    let _ = app.emit("fido-done", ());

    let client_secret = SecretKey::generate();
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(client_secret)
        .bind()
        .await
        .map_err(|e| format!("Failed to bind endpoint: {}", e))?;

    let _ = tokio::time::timeout(Duration::from_secs(10), endpoint.online()).await;

    // Use just the node ID — the presets::N0 relay map handles geographic
    // routing and fallback across all N0 relays automatically.
    let addr = iroh::EndpointAddr::new(host_node_id);

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

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
    {
        let mut input_send_guard = state.input_send.lock().await;
        *input_send_guard = Some(tx);
    }

    {
        let mut ce = state.client_endpoint.lock().await;
        *ce = Some(endpoint.clone());
    }

    // Input forwarder: throttle MouseMove to ~60 fps to prevent queue growth
    let mut input_stream = input_send;
    tokio::spawn(async move {
        let mut rx = rx;
        let mut last_mouse_time = Instant::now();
        const MOUSE_INTERVAL: Duration = Duration::from_millis(16);

        while let Some(event) = rx.recv().await {
            if matches!(event, InputEvent::MouseMove { .. }) {
                let now = Instant::now();
                if now.duration_since(last_mouse_time) < MOUSE_INTERVAL {
                    continue;
                }
                last_mouse_time = now;
            }
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

    // Frame reader — dual path: WebCodecs (raw bytes) or software JPEG decode
    let use_webcodecs = state.webcodecs.load(Ordering::SeqCst);
    tokio::spawn(async move {
        let mut frame_count = 0u32;
        let start = Instant::now();

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
            let frame_len =
                u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;
            let is_keyframe = header[12] == 1;
            let codec = byte_to_codec(header[13]).to_string();

            let mut frame_buf = vec![0u8; frame_len];
            if frame_recv.read_exact(&mut frame_buf).await.is_err() {
                let _ = app.emit("frame-error", "Connection lost");
                break;
            }

            if use_webcodecs {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&frame_buf);
                let _ = app.emit(
                    "frame",
                    FramePayload {
                        width: w,
                        height: h,
                        data: b64,
                        keyframe: is_keyframe,
                        codec: codec.clone(),
                    },
                );
                frame_count += 1;
                let fps = frame_count as f64 / start.elapsed().as_secs_f64().max(0.001);
                let _ = app.emit(
                    "frame-stats",
                    serde_json::json!({ "fps": fps, "count": frame_count, "keyframe": is_keyframe }),
                );
            } else if let Some(ref mut dec) = decoder {
                for nal in nal_units(&frame_buf) {
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
                            .write_to(
                                &mut Cursor::new(&mut jpeg_buf),
                                image::ImageFormat::Jpeg,
                            )
                            .is_err()
                        {
                            continue;
                        }

                        let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg_buf);
                        let _ = app.emit(
                            "frame",
                            FramePayload {
                                width: yw as u32,
                                height: yh as u32,
                                data: b64,
                                keyframe: is_keyframe,
                                codec: codec.clone(),
                            },
                        );
                        frame_count += 1;
                        let fps = frame_count as f64 / start.elapsed().as_secs_f64().max(0.001);
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
            tx.send(event).map_err(|e| format!("Input channel closed: {}", e))?;
            Ok(true)
        }
        None => Err("Not connected to host".into()),
    }
}

#[tauri::command]
pub async fn iroh_client_disconnect(state: State<'_, AppState>) -> Result<bool, String> {
    {
        let mut ce = state.client_endpoint.lock().await;
        if let Some(endpoint) = ce.take() {
            endpoint.close().await;
        }
    }
    {
        let mut input_send = state.input_send.lock().await;
        *input_send = None;
    }
    Ok(true)
}
