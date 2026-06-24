// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;

use commands::AppState;

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_log::Builder::default().build())
        .manage(AppState::default())
        .setup(|app| {
            use tauri::Manager;

            // Load persisted encoder config before the window reads it
            if let Ok(data_dir) = app.path().app_data_dir() {
                let path = data_dir.join("encoder_config.json");
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    if let Ok(config) =
                        serde_json::from_str::<commands::EncoderConfig>(&contents)
                    {
                        let state = app.state::<commands::AppState>();
                        *state.encoder_config.lock().unwrap() = config;
                    }
                }
            }

            // Daemon mode runs headless; only show the window for interactive use
            if !app.state::<commands::AppState>().daemon {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::auth::fido_device_info,
            commands::auth::fido_pin_retries,
            commands::auth::key_derive_identity,
            commands::state::is_daemon_mode,
            commands::state::set_webcodecs_available,
            commands::state::is_webcodecs_available,
            commands::streaming::get_encoder_config,
            commands::streaming::set_encoder_config,
            commands::streaming::detect_available_encoders,
            commands::auth::host_registration_status,
            commands::auth::key_register_host,
            commands::auth::host_unregister,
            commands::network::iroh_host_start,
            commands::network::iroh_host_stop,
            commands::network::iroh_host_status,
            commands::network::iroh_client_connect,
            commands::network::iroh_client_disconnect,
            commands::network::iroh_client_send_input,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
