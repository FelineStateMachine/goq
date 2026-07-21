// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;

use commands::AppState;

fn main() {
    let state = match AppState::from_args(std::env::args_os()) {
        Ok(state) => state,
        Err(error) => {
            eprintln!("sigil-spark: {error}");
            std::process::exit(2);
        }
    };

    let run_result = tauri::Builder::default()
        // Packet-level transport logs are catastrophically expensive in an
        // interactive media client. Keep actionable warnings and errors
        // without formatting every QUIC packet on the UI-critical machine.
        .plugin(
            tauri_plugin_log::Builder::default()
                .level(log::LevelFilter::Warn)
                .build(),
        )
        .manage(state)
        .setup(|app| {
            use tauri::Manager;

            // The Tauri application is the installed client. Hosting is owned
            // exclusively by the separate, headless `sigil-host` daemon.
            if let Some(window) = app.get_webview_window("main") {
                let focus_window = window.clone();
                window.on_window_event(move |event| {
                    if matches!(event, tauri::WindowEvent::Focused(true))
                        && let Err(error) =
                            commands::state::reassert_client_cursor_grab(&focus_window)
                    {
                        log::warn!("could not restore cursor grab after focus changed: {error}");
                    }
                });
                let _ = window.show();
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::auth::fido_device_info,
            commands::auth::fido_pin_retries,
            commands::auth::key_derive_identity,
            commands::state::development_connection_mode,
            commands::state::set_client_cursor_grab,
            commands::state::set_webcodecs_available,
            commands::state::is_webcodecs_available,
            commands::network::iroh_client_connect,
            commands::network::iroh_client_disconnect,
            commands::network::iroh_client_ack_frame,
            commands::network::iroh_client_ack_audio,
            commands::network::iroh_client_stop_audio,
            commands::network::iroh_client_send_input,
        ])
        .run(tauri::generate_context!());
    commands::state::restore_client_cursor();
    run_result.expect("error while running tauri application");
}
