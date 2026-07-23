// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod media;
mod platform_capabilities;

use commands::AppState;

fn main() {
    let state = match AppState::from_args(std::env::args_os()) {
        Ok(state) => state,
        Err(error) => {
            eprintln!("portal: {error}");
            std::process::exit(2);
        }
    };

    let app = tauri::Builder::default()
        // Must be registered first so secondary deep-link launches are routed
        // into the already-running Portal process.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            for argument in argv.into_iter().skip(1) {
                let path = std::path::PathBuf::from(&argument);
                if path.extension().and_then(|value| value.to_str()) != Some("goq-invite") {
                    continue;
                }
                if let Ok(url) = url::Url::from_file_path(path)
                    && let Err(error) = commands::enrollment::stage_opened_url(app, &url)
                {
                    log::warn!("could not import invitation from secondary launch: {error}");
                }
            }
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_dialog::init())
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
            use tauri_plugin_deep_link::DeepLinkExt;

            if let Ok(Some(urls)) = app.deep_link().get_current() {
                for url in urls {
                    if let Err(error) = commands::enrollment::stage_opened_url(app.handle(), &url) {
                        log::warn!("could not import startup invitation: {error}");
                    }
                }
            }
            let app_handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    if let Err(error) = commands::enrollment::stage_opened_url(&app_handle, &url) {
                        log::warn!("could not import opened invitation: {error}");
                    }
                }
            });

            // The Tauri application is the installed client. Hosting is owned
            // exclusively by the separate, headless `sigil` daemon.
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
            commands::enrollment::portal_enrollment_status,
            commands::enrollment::portal_import_invitation_file,
            commands::enrollment::portal_confirm_invitation,
            commands::enrollment::portal_cancel_invitation,
            commands::enrollment::portal_reset_enrollment,
            commands::state::development_connection_mode,
            commands::state::set_client_cursor_grab,
            commands::state::set_client_window_size,
            commands::state::set_webcodecs_available,
            commands::state::is_webcodecs_available,
            commands::network::iroh_client_connect,
            commands::network::iroh_client_disconnect,
            commands::network::iroh_client_ack_frame,
            commands::network::iroh_client_request_keyframe,
            commands::network::iroh_client_send_media_feedback,
            commands::network::iroh_client_ack_audio,
            commands::network::iroh_client_stop_audio,
            commands::network::iroh_client_send_input,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Portal application");
    app.run(|app, event| {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
        if let tauri::RunEvent::Opened { urls } = event {
            for url in urls {
                if let Err(error) = commands::enrollment::stage_opened_url(app, &url) {
                    log::warn!("could not import opened invitation file: {error}");
                }
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "android")))]
        let _ = (app, event);
    });
    commands::state::restore_client_cursor();
}
