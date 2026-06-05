//! gilb Tauri shell — wires the UI to `gilb-engine`.

mod commands;
mod events;
mod logging;
mod meeting;
mod state;

use tauri::Manager;
use tracing::{error, warn};

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
            // Bridge the persisted BYOK OpenAI key into the process env (unless
            // one is already exported) so `RecordingSettings::from_env` — and
            // thus the meeting transcription trigger — sees it. Best-effort: a
            // missing file is fine, an unreadable one is logged, never fatal.
            if let Err(err) = gilb_config::hydrate_openai_key_env() {
                warn!(
                    ?err,
                    "failed to hydrate OPENAI_API_KEY from persisted store"
                );
            }
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
            commands::privacy::open_privacy_pane,
            commands::countdown::show_countdown,
            commands::countdown::resolve_countdown,
            commands::settings::open_settings,
            commands::settings::get_openai_key,
            commands::settings::set_openai_key,
            commands::settings::test_openai_key,
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
