//! Pre-record / stop countdown popup windows + their resolution commands.
//!
//! `show_countdown` spawns a borderless, always-on-top, screen-centered second
//! OS window (label `countdown`) that floats over other apps and fills a single
//! Record button over `countdown_seconds`. `resolve_countdown` is the one exit
//! point: it publishes `RecordingArmed`/`RecordingCancelled` on the EventBus,
//! logs the outcome, and tears the window down. The matching stop-countdown
//! pair hands the user's choice to the meeting bridge over [`StopCountdownTx`].
//!
//! The window title is read from the [`ShellConfig`] managed state, so
//! differently-branded shells reuse these windows without forking.

use gilb_config::RecordingSettings;
use gilb_events::{EventBus, RecordingEvent};
use gilb_pipeline::StopResolution;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tracing::info;

use crate::ShellConfig;

const COUNTDOWN_LABEL: &str = "countdown";
const STOP_COUNTDOWN_LABEL: &str = "stop-countdown";

/// Fallback window title when no [`ShellConfig`] is managed (shells are expected
/// to manage one — see [`crate::spawn_meeting_pipeline`]).
const DEFAULT_TITLE: &str = "Gilb";

/// Managed Tauri state holding the sender half of the pipeline's stop-countdown
/// resolution channel, so the `resolve_stop_countdown` command can hand the
/// user's choice back to the bridge task.
pub struct StopCountdownTx(pub tokio::sync::mpsc::Sender<StopResolution>);

/// Resolve the countdown window title from the optionally-managed
/// [`ShellConfig`], falling back to [`DEFAULT_TITLE`]. Pure so the threading is
/// unit-testable without a running Tauri app.
fn shell_title(cfg: Option<&ShellConfig>) -> String {
    cfg.map(|c| c.window_title.clone())
        .unwrap_or_else(|| DEFAULT_TITLE.to_string())
}

fn window_title(app: &AppHandle) -> String {
    shell_title(app.try_state::<ShellConfig>().as_deref())
}

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
        .title(window_title(app))
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

/// Publish the countdown outcome on the recording bus. Pure over the bus so the
/// `armed -> Armed` / `!armed -> Cancelled` mapping is unit-testable.
fn publish_resolution(bus: &EventBus, meeting_id: i64, armed: bool) {
    if armed {
        info!(meeting_id, "RecordingArmed");
        bus.publish_recording(RecordingEvent::Armed { meeting_id });
    } else {
        info!(meeting_id, "RecordingCancelled");
        bus.publish_recording(RecordingEvent::Cancelled { meeting_id });
    }
}

#[tauri::command]
pub async fn resolve_countdown(
    app: tauri::AppHandle,
    bus: tauri::State<'_, EventBus>,
    meeting_id: i64,
    armed: bool,
) -> Result<(), String> {
    publish_resolution(&bus, meeting_id, armed);

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
        .title(window_title(app))
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

#[cfg(test)]
mod tests {
    use super::*;
    use gilb_events::RecordingEvent;

    #[test]
    fn shell_title_uses_config_when_present() {
        let cfg = ShellConfig {
            window_title: "gilb".to_string(),
        };
        assert_eq!(shell_title(Some(&cfg)), "gilb");
    }

    #[test]
    fn shell_title_falls_back_without_config() {
        assert_eq!(shell_title(None), DEFAULT_TITLE);
    }

    #[tokio::test]
    async fn resolve_countdown_armed_publishes_armed() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe_recording();
        publish_resolution(&bus, 42, true);
        let msg = rx.recv().await.unwrap();
        assert!(matches!(
            msg.payload,
            RecordingEvent::Armed { meeting_id: 42 }
        ));
    }

    #[tokio::test]
    async fn resolve_countdown_not_armed_publishes_cancelled() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe_recording();
        publish_resolution(&bus, 7, false);
        let msg = rx.recv().await.unwrap();
        assert!(matches!(
            msg.payload,
            RecordingEvent::Cancelled { meeting_id: 7 }
        ));
    }
}
