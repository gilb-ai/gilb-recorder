//! `AppState` + init helpers shared between `setup()` and Tauri commands.
//!
//! `build_app_state` is intentionally synchronous from Tauri's point of view —
//! `setup()` is not async and we want to surface failures to the user before
//! the event loop starts.

use std::sync::Arc;

use anyhow::{Context, Result};
use gilb_config::{db_path, ensure_data_dir, logs_dir};
use gilb_engine::Engine;
use parking_lot::Mutex;
use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tracing::info;

use crate::analyzer_supervisor::AnalyzerSupervisor;

/// In-flight recorder→gilb-web login. Set by `start_login`, consumed by the
/// `gilb://auth/callback` deep-link handler, which checks `state` matches the
/// callback before trusting it (the callback must belong to a login this
/// instance started).
pub struct PendingAuth {
    pub state: String,
    pub gilb_web_url: String,
}

pub struct AppState {
    pub engine: Arc<Engine>,
    pub pending_auth: Mutex<Option<PendingAuth>>,
    /// Drives the bundled `gilb-analyzer run` daemon: active while capture runs.
    pub analyzer: AnalyzerSupervisor,
}

/// Open the SQLite database and build [`AppState`]. Pulled out of `setup()` so
/// failures can be reported to the user before the app exits.
pub fn build_app_state() -> Result<AppState> {
    ensure_data_dir().context("ensure_data_dir")?;
    let path = db_path().context("db_path")?;
    info!(?path, "opening engine");
    let engine = tauri::async_runtime::block_on(Engine::open(path))?;
    Ok(AppState {
        engine: Arc::new(engine),
        pending_auth: Mutex::new(None),
        analyzer: AnalyzerSupervisor::spawn(),
    })
}

/// Show a modal native dialog explaining why the app can't start, and point
/// the user at the log file. Best-effort: if dialog rendering itself fails,
/// the error is already in the log.
pub fn show_init_error(app: &AppHandle, err: &anyhow::Error) {
    let log_hint = logs_dir()
        .ok()
        .map(|p| format!("\n\nDetails in the log: {}", p.display()))
        .unwrap_or_default();
    let body = format!("gilb failed to start:\n\n{err:#}{log_hint}");
    let _ = app
        .dialog()
        .message(body)
        .title("gilb")
        .kind(MessageDialogKind::Error)
        .blocking_show();
}
