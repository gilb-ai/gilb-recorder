//! gilb Tauri shell — wires the UI to `gilb-engine`.

mod commands;
mod events;
mod logging;
mod meeting;
mod state;
mod transcribe_worker;

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
        .setup(|app| {
            match state::build_app_state() {
                Ok(s) => {
                    events::spawn_proxies(app.handle().clone(), s.engine.clone());

                    // Start the meeting flow: detector + recorder + countdown
                    // bridge. Pull the bus/db off the engine before `manage` moves
                    // the state in. The detector is the live macOS path; elsewhere
                    // it's a no-op stand-in (see `meeting`).
                    let bus = s.engine.event_bus().clone();
                    let db = s.engine.db().clone();
                    app.manage(s);
                    // Cancel flag shared between download_model / cancel_model_download.
                    app.manage(commands::transcription::DownloadCancel::default());
                    // Background transcription worker + its queue. Spawned before
                    // the meeting pipeline so the queue exists when meetings end;
                    // also used by the language setting and the model downloader.
                    app.manage(transcribe_worker::spawn_transcription_worker(db.clone()));
                    match gilb_config::data_dir() {
                        Ok(data_dir) => {
                            meeting::spawn_meeting_pipeline(app.handle().clone(), bus, db, data_dir)
                        }
                        Err(err) => error!(?err, "data_dir failed; meeting pipeline not started"),
                    }

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
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::capture::start_capture,
            commands::capture::stop_capture,
            commands::capture::status,
            commands::capture::get_tracking_paused,
            commands::capture::set_tracking_paused,
            commands::privacy::open_privacy_pane,
            commands::countdown::show_countdown,
            commands::countdown::resolve_countdown,
            commands::countdown::resolve_stop_countdown,
            commands::countdown::stop_meeting_recording,
            commands::settings::get_meeting_detection,
            commands::settings::set_meeting_detection,
            commands::transcription::get_transcription_status,
            commands::transcription::set_transcription_language,
            commands::transcription::download_model,
            commands::transcription::cancel_model_download,
            commands::transcription::delete_model,
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
