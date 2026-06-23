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
    /// The active meeting recording (id, display name) — mirrors the pipeline's
    /// indicator so the tray menu and manual stop know what to stop. `None` when
    /// nothing is recording.
    pub recording: Mutex<Option<(i64, String)>>,
    /// Set while a manual arm is in flight (between publishing
    /// `RecordingEvent::Armed` and the pipeline's status callback), so a quick
    /// double toggle can't insert two meetings. Cleared by any status update;
    /// entries older than a few seconds are treated as stale.
    pub arming_since: Mutex<Option<std::time::Instant>>,
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
        recording: Mutex::new(None),
        arming_since: Mutex::new(None),
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
