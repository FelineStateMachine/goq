// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;

use commands::AppState;

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_log::Builder::default().build())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::fido_device_info,
            commands::fido_pin_retries,
            commands::iroh_host_start,
            commands::iroh_host_status,
            commands::iroh_client_connect,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
