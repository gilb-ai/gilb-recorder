//! Settings window + its open command.
//!
//! `open_settings` spawns (or focuses) a standard decorated, screen-centered
//! second OS window (label `settings`) that loads `settings.html`. The window
//! hosts presentational capture settings; today that is a single "Enable
//! meeting detection" toggle. The toggle is UX-only — it neither persists nor
//! starts/stops detection. Mirrors the countdown window pattern.

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

const SETTINGS_LABEL: &str = "settings";

#[tauri::command]
pub async fn open_settings(app: AppHandle) -> Result<(), String> {
    // Focus the existing window instead of stacking a second one.
    if let Some(win) = app.get_webview_window(SETTINGS_LABEL) {
        let _ = win.set_focus();
        return Ok(());
    }

    let url = WebviewUrl::App("settings.html".into());
    WebviewWindowBuilder::new(&app, SETTINGS_LABEL, url)
        .title("Gilb Settings")
        .inner_size(420.0, 240.0)
        .resizable(false)
        .center()
        .build()
        .map_err(|e| format!("failed to open settings window: {e}"))?;
    Ok(())
}
