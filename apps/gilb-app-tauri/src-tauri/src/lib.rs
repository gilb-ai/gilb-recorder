//! gilb Tauri shell — wires the UI to `gilb-engine`.

mod commands;
mod events;
mod logging;
mod meeting;
mod recording;
mod state;
mod transcribe_worker;
mod tray;

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
                    // the state in. The detector is live on macOS (unified log)
                    // and Windows (WASAPI); a no-op stand-in elsewhere (see `meeting`).
                    let bus = s.engine.event_bus().clone();
                    let db = s.engine.db().clone();
                    app.manage(s);
                    // The countdown popup windows read their title from this.
                    app.manage(gilb_shell_tauri::ShellConfig {
                        window_title: "gilb".into(),
                    });
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

                    // System tray: gilb's home (open the window, toggle a manual
                    // recording, quit). Built after `AppState` is managed — the
                    // controller reads `recording` from it.
                    if let Err(err) = tray::setup(app.handle()) {
                        error!(?err, "tray setup failed");
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
            gilb_shell_tauri::open_privacy_pane,
            gilb_shell_tauri::show_countdown,
            gilb_shell_tauri::resolve_countdown,
            gilb_shell_tauri::resolve_stop_countdown,
            gilb_shell_tauri::stop_meeting_recording,
            gilb_shell_tauri::get_meeting_detection,
            gilb_shell_tauri::set_meeting_detection,
            commands::transcription::get_transcription_status,
            commands::transcription::set_transcription_language,
            commands::transcription::download_model,
            commands::transcription::cancel_model_download,
            commands::transcription::delete_model,
            commands::auth::start_login,
            commands::auth::auth_status,
            commands::auth::sign_out,
        ])
        .build(tauri::generate_context!());

    match result {
        Ok(app) => app.run(|handle, event| match event {
            // On quit, ask the analyzer supervisor to stop the daemon (best
            // effort — the daemon's own parent-death guard is the hard backstop).
            tauri::RunEvent::ExitRequested { .. } => {
                if let Some(state) = handle.try_state::<state::AppState>() {
                    state.analyzer.set_active(false);
                }
            }
            // Hard-exit once the event loop has fully stopped, skipping the libc
            // atexit / C++ static-destructor phase. whisper.cpp keeps a global
            // ggml Metal device whose static destructor frees its residency sets
            // (`ggml_metal_rsets_free`); at process exit that aborts because
            // model buffers are still live, and it races a background residency
            // heartbeat thread — a SIGABRT on essentially every quit. The process
            // is already terminating and SQLite commits are durable (WAL +
            // synchronous=NORMAL fsync), so bypassing atexit teardown is safe.
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Exit => {
                // SAFETY: `_exit` is async-signal-safe and allocation-free; we are
                // past all Tauri teardown and intentionally skip atexit handlers.
                unsafe { libc::_exit(0) }
            }
            _ => {}
        }),
        Err(err) => {
            error!(?err, "tauri build/runtime error");
            // No app handle to dialog from at this point — log + non-zero exit.
            std::process::exit(1);
        }
    }
}
