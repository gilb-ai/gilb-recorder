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

use crate::meeting::{StopCountdownTx, StopResolution};
use crate::state::AppState;

const COUNTDOWN_LABEL: &str = "countdown";
const STOP_COUNTDOWN_LABEL: &str = "stop-countdown";

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
        // Grab focus so the very first click hits a button. A borderless,
        // always-on-top window over another app (Zoom) is otherwise inactive on
        // macOS, where the first click only activates the window — making
        // "Record now"/"Stop now" appear dead until a second click.
        .focused(true)
        .center()
        .build()?
        .set_focus()
        .ok();
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

/// Open the borderless, centered, always-on-top *stop*-countdown window for the
/// meeting currently being recorded. Mirrors [`open_countdown_window`] but its
/// progress button counts down to *stopping* the capture; "Keep recording"
/// backs out. Opened by the meeting bridge on `MeetingEvent::Ended`.
pub(crate) fn open_stop_countdown_window(
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
    let url = WebviewUrl::App(format!("stop-countdown.html?{query}").into());

    WebviewWindowBuilder::new(app, STOP_COUNTDOWN_LABEL, url)
        .title("Gilb")
        .inner_size(380.0, 220.0)
        .resizable(false)
        .decorations(false)
        .always_on_top(true)
        // Grab focus so the very first click hits a button. A borderless,
        // always-on-top window over another app (Zoom) is otherwise inactive on
        // macOS, where the first click only activates the window — making
        // "Record now"/"Stop now" appear dead until a second click.
        .focused(true)
        .center()
        .build()?
        .set_focus()
        .ok();
    Ok(())
}

/// One exit point for the stop-countdown popup: hand the user's choice
/// (`keep == true` keeps recording, otherwise stop now) to the meeting bridge
/// over the managed channel, then tear the window down.
#[tauri::command]
pub async fn resolve_stop_countdown(
    app: tauri::AppHandle,
    stop_tx: tauri::State<'_, StopCountdownTx>,
    meeting_id: i64,
    keep: bool,
) -> Result<(), String> {
    info!(meeting_id, keep, "stop-countdown resolved");
    // Close the window *first*, unconditionally: it's borderless + always-on-top
    // with no title bar, so if the send below fails (e.g. the bridge task is
    // gone) we must not leave an un-dismissable overlay floating over everything.
    if let Some(win) = app.get_webview_window(STOP_COUNTDOWN_LABEL) {
        let _ = win.close();
    }
    stop_tx
        .0
        .send(StopResolution { meeting_id, keep })
        .await
        .map_err(|_| "meeting bridge is gone".to_string())
}

/// Stop the active meeting recording from the main window's recording
/// indicator. Routes a "stop now" resolution to the bridge over the same
/// channel as the stop-countdown popup; there's no popup window to close.
#[tauri::command]
pub async fn stop_meeting_recording(
    stop_tx: tauri::State<'_, StopCountdownTx>,
    meeting_id: i64,
) -> Result<(), String> {
    info!(meeting_id, "stop meeting recording (indicator)");
    stop_tx
        .0
        .send(StopResolution {
            meeting_id,
            keep: false,
        })
        .await
        .map_err(|_| "meeting bridge is gone".to_string())
}
