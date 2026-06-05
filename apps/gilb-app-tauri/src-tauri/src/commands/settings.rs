//! Settings commands (the settings modal in the main window).
//!
//! - Meeting detection (subsystem B) master switch: `get_meeting_detection` /
//!   `set_meeting_detection`. The setter persists the flag and signals the
//!   detector supervisor over [`MeetingControlTx`] so it takes effect live.
//! - BYOK OpenAI API-key store: `get_openai_key` / `set_openai_key` /
//!   `test_openai_key`. Persistence lives in `gilb-config`
//!   (`$HOME/.gilb/openai.json`, 0600); `set_openai_key` mirrors the key into the
//!   process env so the meeting transcription trigger picks it up. (The key UI is
//!   currently hidden; transcription still reads `OPENAI_API_KEY` from the env.)

use gilb_transcribe::KeyValidity;

use crate::meeting::MeetingControlTx;

/// Whether meeting detection is enabled (persisted). The settings UI reads this
/// to render the toggle.
#[tauri::command]
pub async fn get_meeting_detection() -> bool {
    gilb_config::load_preferences().meeting_detection_enabled
}

/// Persist the meeting-detection master switch and apply it live: the detector
/// supervisor starts or stops the platform detector — no restart needed.
#[tauri::command]
pub async fn set_meeting_detection(
    ctl: tauri::State<'_, MeetingControlTx>,
    enabled: bool,
) -> Result<(), String> {
    let mut prefs = gilb_config::load_preferences();
    prefs.meeting_detection_enabled = enabled;
    gilb_config::save_preferences(&prefs).map_err(|e| e.to_string())?;
    let _ = ctl.0.send(enabled).await;
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
