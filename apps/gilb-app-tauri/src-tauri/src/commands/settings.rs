//! Settings commands (the settings modal in the main window).
//!
//! - Meeting detection (subsystem B) master switch: `get_meeting_detection` /
//!   `set_meeting_detection`. The setter persists the flag and signals the
//!   detector supervisor over [`MeetingControlTx`] so it takes effect live.
//!
//! Meeting transcription is on-device (local Whisper) and needs no API key; it
//! self-enables once the model is downloaded — see `gilb_transcribe`.

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
