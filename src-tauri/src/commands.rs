//! Tauri backend commands for Keyhome.
//!
//! Commands:
//! - fido_device_info: enumerate FIDO2 devices and get info
//! - fido_pin_retries: check PIN retry count
//! - iroh_host_start: start Iroh endpoint, return addr JSON
//! - iroh_host_status: check if host endpoint is running
//! - iroh_client_connect: connect to a host addr (placeholder for now)

use ctap_hid_fido2::{Cfg, FidoKeyHidFactory};
use serde::Serialize;
use std::sync::Mutex;
use tauri::State;

#[derive(Default)]
pub struct AppState {
    pub host_addr: Mutex<Option<String>>,
}

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

#[derive(Serialize)]
pub struct HostStatus {
    pub running: bool,
    pub addr_json: Option<String>,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn iroh_host_start(state: State<'_, AppState>) -> Result<HostStatus, String> {
    let secret = iroh::SecretKey::generate();
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(secret)
        .bind()
        .await
        .map_err(|e| format!("Failed to bind endpoint: {}", e))?;

    let addr = endpoint.addr();
    let addr_json = serde_json::to_string(&addr).map_err(|e| format!("Failed to serialize addr: {}", e))?;

    let mut host_addr = state.host_addr.lock().map_err(|e| format!("Lock error: {}", e))?;
    *host_addr = Some(addr_json.clone());

    // Keep endpoint alive by leaking it — for MVP this is fine
    // In production, store in AppState
    std::mem::forget(endpoint);

    Ok(HostStatus {
        running: true,
        addr_json: Some(addr_json),
        error: None,
    })
}

#[tauri::command]
pub fn iroh_host_status(state: State<'_, AppState>) -> HostStatus {
    let host_addr = state.host_addr.lock().ok();
    match host_addr {
        Some(addr) => HostStatus {
            running: addr.is_some(),
            addr_json: addr.clone(),
            error: None,
        },
        None => HostStatus {
            running: false,
            addr_json: None,
            error: Some("State lock poisoned".into()),
        },
    }
}

#[derive(Serialize)]
pub struct ConnectResult {
    pub connected: bool,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn iroh_client_connect(_addr_json: String) -> Result<ConnectResult, String> {
    // Placeholder — full client connection will be implemented in next iteration
    Ok(ConnectResult {
        connected: false,
        error: Some("Client connection not yet implemented in Tauri UI".into()),
    })
}
