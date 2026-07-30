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
use tracing::{info, warn};

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

/// A database that couldn't be migrated (left behind by an incompatible
/// version) and was moved aside so the app could start with a fresh one.
/// Reported to the user via [`show_db_rescue_notice`].
pub struct DbRescue {
    pub archived_to: std::path::PathBuf,
}

/// Open the SQLite database and build [`AppState`]. Pulled out of `setup()` so
/// failures can be reported to the user before the app exits.
///
/// If the open fails because the on-disk database has an incompatible
/// migration history (edited checksum, downgrade past a newer schema), the
/// database files are archived next to themselves and the open is retried
/// once with a fresh database; the returned [`DbRescue`] tells `setup()` to
/// notify the user. Any other failure — and a failure of the retry itself —
/// propagates as before.
pub fn build_app_state() -> Result<(AppState, Option<DbRescue>)> {
    ensure_data_dir().context("ensure_data_dir")?;
    let path = db_path().context("db_path")?;
    info!(?path, "opening engine");
    let (engine, rescue) = match tauri::async_runtime::block_on(Engine::open(path.clone())) {
        Ok(engine) => (engine, None),
        Err(err) if gilb_db::is_migrate_error(&err) => {
            warn!(
                ?err,
                "incompatible db migration history; archiving and starting fresh"
            );
            let archived_to =
                gilb_db::archive_incompatible_db(&path).context("archiving the incompatible db")?;
            let engine = tauri::async_runtime::block_on(Engine::open(path))
                .context("opening a fresh db after archiving the incompatible one")?;
            (engine, Some(DbRescue { archived_to }))
        }
        Err(err) => return Err(err),
    };
    Ok((
        AppState {
            engine: Arc::new(engine),
            pending_auth: Mutex::new(None),
            analyzer: AnalyzerSupervisor::spawn(),
            recording: Mutex::new(None),
            arming_since: Mutex::new(None),
        },
        rescue,
    ))
}

/// Non-blocking info dialog: the previous database was incompatible with this
/// version and was archived at `rescue.archived_to`; recording starts fresh.
pub fn show_db_rescue_notice(app: &AppHandle, rescue: &DbRescue) {
    let body = format!(
        "Your recording database was created by an incompatible version of \
         gilb and could not be upgraded.\n\nIt was moved to:\n{}\n\ngilb will \
         record into a fresh database. The archived file can be inspected or \
         restored manually.",
        rescue.archived_to.display()
    );
    app.dialog()
        .message(body)
        .title("gilb")
        .kind(MessageDialogKind::Info)
        .show(|_| {});
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
