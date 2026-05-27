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

    let result = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| match state::build_app_state() {
            Ok(s) => {
                events::spawn_proxies(app.handle().clone(), s.engine.clone());
                app.manage(s);
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
        ])
        .run(tauri::generate_context!());

    if let Err(err) = result {
        error!(?err, "tauri runtime error");
        // No app handle to dialog from at this point — log + non-zero exit.
        std::process::exit(1);
    }
}
