//! Recording lifecycle: start/stop/status. Each command resolves an
//! `AppState` reference and forwards to `gilb-engine`.

use gilb_config::RecordingSettings;
use gilb_engine::EngineStatus;

use crate::state::AppState;

#[tauri::command]
pub async fn start_capture(state: tauri::State<'_, AppState>) -> Result<i64, String> {
    let settings = RecordingSettings::from_env();
    state
        .engine
        .start_capture(settings)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn stop_capture(state: tauri::State<'_, AppState>) -> Result<(), String> {
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
