//! Settings window + its commands.
//!
//! `open_settings` spawns (or focuses) a standard decorated, screen-centered
//! second OS window (label `settings`) that loads `settings.html`. The window
//! hosts the meeting-detection toggle (presentational) plus the functional BYOK
//! OpenAI API-key field: `get_openai_key` / `set_openai_key` / `test_openai_key`
//! load, persist, and validate the key. Persistence lives in `gilb-config`
//! (`$HOME/.gilb/openai.json`, 0600); `set_openai_key` also mirrors the key into
//! the process env so the meeting transcription trigger
//! (`RecordingSettings::from_env`) picks it up with no change to that module.

use gilb_transcribe::KeyValidity;
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
        .inner_size(420.0, 360.0)
        .resizable(false)
        .center()
        .build()
        .map_err(|e| format!("failed to open settings window: {e}"))?;
    Ok(())
}

/// Load the persisted BYOK OpenAI key, if any. `None` ⇒ no key stored. A read
/// error is treated as "no key" so the field simply renders empty.
#[tauri::command]
pub async fn get_openai_key() -> Option<String> {
    gilb_config::load_openai_key().unwrap_or(None)
}

/// Persist (or clear) the BYOK OpenAI key and mirror it into the process env so
/// the meeting transcription trigger (`RecordingSettings::from_env`) sees it
/// immediately, without restart. An empty/whitespace key clears the store and
/// unsets `OPENAI_API_KEY` — the privacy gate: no key ⇒ no audio leaves the
/// device.
#[tauri::command]
pub async fn set_openai_key(key: String) -> Result<(), String> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        gilb_config::clear_openai_key().map_err(|e| e.to_string())?;
        std::env::remove_var("OPENAI_API_KEY");
    } else {
        gilb_config::save_openai_key(trimmed).map_err(|e| e.to_string())?;
        std::env::set_var("OPENAI_API_KEY", trimmed);
    }
    Ok(())
}

/// Validate a key against OpenAI without persisting it. Returns `"valid"` /
/// `"invalid"` / `"unknown"`; `Err` on a transport failure (no response).
#[tauri::command]
pub async fn test_openai_key(key: String) -> Result<String, String> {
    match gilb_transcribe::validate_api_key(key.trim()).await {
        Ok(KeyValidity::Valid) => Ok("valid".to_string()),
        Ok(KeyValidity::Invalid) => Ok("invalid".to_string()),
        Ok(KeyValidity::Unknown) => Ok("unknown".to_string()),
        Err(e) => Err(e.to_string()),
    }
}
