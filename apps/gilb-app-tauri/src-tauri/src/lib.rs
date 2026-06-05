//! gilb Tauri shell — wires the UI to `gilb-engine`.

mod commands;
mod events;
mod logging;
mod state;

use tauri::Manager;
use tracing::error;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Held until run() returns; dropping flushes the non-blocking writer.
    let _log_guard = logging::init_tracing();

    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default();

    // Single-instance must be registered first so a second launch carrying a
    // `gilb://` deep link (Windows/Linux) forwards it to the running instance
    // instead of starting a new one. Desktop-only (matches the Cargo cfg gate).
    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|_app, _argv, _cwd| {}));
    }

    builder = builder
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_process::init());

    // Updater is desktop-only (matches the Cargo cfg gate).
    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_updater::Builder::new().build());
    }

    let result = builder
        .setup(|app| match state::build_app_state() {
            Ok(s) => {
                events::spawn_proxies(app.handle().clone(), s.engine.clone());
                app.manage(s);

                // Deep-link auth callbacks (gilb://auth/callback?token=…).
                use tauri_plugin_deep_link::DeepLinkExt;
                // On Windows/Linux the scheme is only auto-registered for the
                // installed build; register at runtime so dev builds work too.
                #[cfg(any(windows, target_os = "linux"))]
                {
                    let _ = app.deep_link().register_all();
                }
                let handle = app.handle().clone();
                app.deep_link().on_open_url(move |event| {
                    for url in event.urls() {
                        commands::auth::handle_callback(&handle, &url);
                    }
                });
                Ok(())
            }
            Err(err) => {
                error!(?err, "engine init failed");
                state::show_init_error(app.handle(), &err);
                Err(err.into())
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::capture::start_capture,
            commands::capture::stop_capture,
            commands::capture::status,
            commands::privacy::open_privacy_pane,
            commands::countdown::show_countdown,
            commands::countdown::resolve_countdown,
            commands::auth::start_login,
            commands::auth::auth_status,
            commands::auth::sign_out,
        ])
        .run(tauri::generate_context!());

    if let Err(err) = result {
        error!(?err, "tauri runtime error");
        // No app handle to dialog from at this point — log + non-zero exit.
        std::process::exit(1);
    }
}
