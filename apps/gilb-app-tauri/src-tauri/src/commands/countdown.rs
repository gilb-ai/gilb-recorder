//! Pre-record countdown popup window + its resolution command.
//!
//! `show_countdown` spawns a borderless, always-on-top, screen-centered second
//! OS window (label `countdown`) that floats over other apps and fills a single
//! Record button over `countdown_seconds`. `resolve_countdown` is the one exit
//! point: it publishes `RecordingArmed`/`RecordingCancelled` on the EventBus,
//! logs the outcome, and tears the window down. Nothing consumes those events
//! yet — the trigger wiring (MeetingEvent::Started -> show_countdown) is a
//! separate card.

use gilb_config::RecordingSettings;
use gilb_events::RecordingEvent;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tracing::info;

use crate::state::AppState;

const COUNTDOWN_LABEL: &str = "countdown";

/// Open the borderless, centered, always-on-top countdown window for
/// `meeting_id`. Shared by the `show_countdown` command (invoked from the UI)
/// and the Rust-side meeting bridge, which can't `invoke` a command.
pub(crate) fn open_countdown_window(
    app: &AppHandle,
    app_name: &str,
    meeting_id: i64,
) -> tauri::Result<()> {
    let seconds = RecordingSettings::from_env().countdown_seconds;
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("app", app_name)
        .append_pair("meeting_id", &meeting_id.to_string())
        .append_pair("seconds", &seconds.to_string())
        .finish();
    let url = WebviewUrl::App(format!("countdown.html?{query}").into());

    WebviewWindowBuilder::new(app, COUNTDOWN_LABEL, url)
        .title("Gilb")
        .inner_size(380.0, 220.0)
        .resizable(false)
        .decorations(false)
        .always_on_top(true)
        .center()
        .build()?;
    Ok(())
}

#[tauri::command]
pub async fn show_countdown(
    app: tauri::AppHandle,
    app_name: String,
    meeting_id: i64,
) -> Result<(), String> {
    open_countdown_window(&app, &app_name, meeting_id)
        .map_err(|e| format!("failed to open countdown window: {e}"))
}

#[tauri::command]
pub async fn resolve_countdown(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    meeting_id: i64,
    armed: bool,
) -> Result<(), String> {
    let bus = state.engine.event_bus();
    if armed {
        info!(meeting_id, "RecordingArmed");
        bus.publish_recording(RecordingEvent::Armed { meeting_id });
    } else {
        info!(meeting_id, "RecordingCancelled");
        bus.publish_recording(RecordingEvent::Cancelled { meeting_id });
    }

    if let Some(win) = app.get_webview_window(COUNTDOWN_LABEL) {
        let _ = win.close();
    }
    Ok(())
}
