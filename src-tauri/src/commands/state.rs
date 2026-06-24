use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as TokioMutex;

pub const FRAME_ALPN: &[u8] = b"sigil/frame-stream/0";
pub const INPUT_ALPN: &[u8] = b"sigil/input-stream/0";
pub const RPID: &str = "sigil";
pub const SALT_MESSAGE: &str = "sigil-iroh-identity-v1";
pub const KEYRING_SERVICE: &str = "sigil";
pub const KEYRING_ENTRY: &str = "host-identity";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EncoderConfig {
    pub codec: String,
    pub backend: String,
    pub bitrate: String,
    pub framerate: u32,
    pub gop: u32,
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

pub struct HostState {
    pub node_id: String,
}

pub struct AppState {
    pub host: Mutex<Option<HostState>>,
    pub host_connections: Arc<AtomicU32>,
    pub host_endpoint: TokioMutex<Option<iroh::Endpoint>>,
    pub input_send:
        TokioMutex<Option<tokio::sync::mpsc::UnboundedSender<super::input::InputEvent>>>,
    pub client_endpoint: TokioMutex<Option<iroh::Endpoint>>,
    pub webcodecs: AtomicBool,
    pub daemon: bool,
    pub encoder_config: Mutex<EncoderConfig>,
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
            webcodecs: AtomicBool::new(false),
            daemon,
            encoder_config: Mutex::new(EncoderConfig::default()),
        }
    }
}

// ─── Simple state Tauri commands ─────────────────────────────────────────────

#[tauri::command]
pub fn is_daemon_mode(state: tauri::State<'_, AppState>) -> bool {
    state.daemon
}

#[tauri::command]
pub fn set_webcodecs_available(state: tauri::State<'_, AppState>, available: bool) {
    state.webcodecs.store(available, Ordering::SeqCst);
}

#[tauri::command]
pub fn is_webcodecs_available(state: tauri::State<'_, AppState>) -> bool {
    state.webcodecs.load(Ordering::SeqCst)
}
