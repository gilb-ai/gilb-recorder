//! Activity-tracking lifecycle: start/stop/status + the persisted pause flag.
//!
//! `start_capture`/`stop_capture` drive the always-on a11y engine (subsystem A
//! in `docs/ui-design.md`); the UI presents them as Resume/Pause. The pause flag
//! is persisted so a deliberate pause survives restarts (the UI reads it on
//! launch to decide whether to auto-resume).

use gilb_config::RecordingSettings;
use gilb_engine::EngineStatus;

use crate::state::AppState;

#[tauri::command]
pub async fn start_capture(state: tauri::State<'_, AppState>) -> Result<i64, String> {
    let settings = RecordingSettings::from_env();
    let session = state
        .engine
        .start_capture(settings)
        .await
        .map_err(|e| e.to_string())?;
    // Capture is on → let the analyzer daemon run alongside it (Tier-2 only).
    state.analyzer.set_active(true);
    Ok(session)
}

#[tauri::command]
pub async fn stop_capture(state: tauri::State<'_, AppState>) -> Result<(), String> {
    // Stop analysis first so it isn't reading a session we're about to close.
    state.analyzer.set_active(false);
    state
        .engine
        .stop_capture("user-stop")
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn status(state: tauri::State<'_, AppState>) -> Result<EngineStatus, String> {
    state.engine.status().await.map_err(|e| e.to_string())
}

/// Whether the user has paused activity tracking (persisted). The UI reads this
/// on launch to decide whether to auto-resume capture.
#[tauri::command]
pub async fn get_tracking_paused() -> bool {
    gilb_config::load_preferences().tracking_paused
}

/// Persist the paused flag. Called alongside `stop_capture` (pause) and
/// `start_capture` (resume) so the choice survives restarts.
#[tauri::command]
pub async fn set_tracking_paused(paused: bool) -> Result<(), String> {
    let mut prefs = gilb_config::load_preferences();
    prefs.tracking_paused = paused;
    gilb_config::save_preferences(&prefs).map_err(|e| e.to_string())
}
