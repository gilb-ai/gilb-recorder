//! Settings commands (the settings modal in the main window).
//!
//! Meeting detection (subsystem B) master switch: `get_meeting_detection` /
//! `set_meeting_detection`. The setter persists the flag and signals the
//! detector supervisor over [`MeetingControlTx`] so it takes effect live.

/// Managed Tauri state for toggling meeting detection (subsystem B) at runtime.
/// `set_meeting_detection` sends `true`/`false` here; the detector supervisor
/// starts or stops the platform detector accordingly — no restart needed.
pub struct MeetingControlTx(pub tokio::sync::mpsc::Sender<bool>);

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
