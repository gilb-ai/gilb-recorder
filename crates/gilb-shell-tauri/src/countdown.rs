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
use url::Url;

use crate::ShellConfig;

const COUNTDOWN_LABEL: &str = "countdown";
const STOP_COUNTDOWN_LABEL: &str = "stop-countdown";
const COUNTDOWN_HTML: &str = "countdown.html";
const STOP_COUNTDOWN_HTML: &str = "stop-countdown.html";

/// Fallback window title when no [`ShellConfig`] is managed (shells are expected
/// to manage one — see [`crate::spawn_meeting_pipeline`]).
const DEFAULT_TITLE: &str = "WorkScreen";

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

/// Build the `app=&meeting_id=&seconds=` query string the countdown pages read.
fn countdown_query(app_name: &str, meeting_id: i64) -> String {
    let seconds = RecordingSettings::from_env().countdown_seconds;
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("app", app_name)
        .append_pair("meeting_id", &meeting_id.to_string())
        .append_pair("seconds", &seconds.to_string())
        .finish()
}

/// Re-point a live window's URL at `/{html}?{query}` while preserving its scheme
/// and host, so the result is correct whether assets are served from the bundled
/// `tauri://` protocol or a dev server. Pure so the rewrite is unit-testable.
fn reuse_url(current: &Url, html: &str, query: &str) -> Url {
    let mut url = current.clone();
    url.set_path(&format!("/{html}"));
    url.set_query(Some(query));
    url
}

/// Show a borderless, centered, always-on-top countdown popup, *reusing* the
/// existing OS window when one is already alive rather than destroying and
/// rebuilding it.
///
/// macOS teardown safety: `WebviewWindow::close()` frees the WKWebView's
/// `WebPageProxy`, but WebKit may still have main-thread runloop work queued for
/// it — obscured-content-inset updates triggered by the create/center/focus we
/// just performed. That deferred work then fires against freed memory, a
/// use-after-free crash in `WebPageProxy::dispatchSetObscuredContentInsets`
/// (observed on macOS 26). Keeping one hidden window per popup alive and
/// re-navigating it removes the teardown race entirely.
///
/// Reuse relies on the countdown page resetting itself on load: the `resolve`
/// guard + `clearTimeout` in `countdown.ts`/`stop-countdown.ts` mean a hidden,
/// already-resolved page never auto-fires, and a fresh `navigate` restarts the
/// fill from zero with the new meeting's params.
fn show_countdown_popup(
    app: &AppHandle,
    label: &str,
    html: &str,
    app_name: &str,
    meeting_id: i64,
) -> tauri::Result<()> {
    let query = countdown_query(app_name, meeting_id);

    if let Some(win) = app.get_webview_window(label) {
        // Reuse: reload the existing webview with a fresh countdown, recenter for
        // the current display layout, and resurface it.
        win.navigate(reuse_url(&win.url()?, html, &query))?;
        let _ = win.center();
        let _ = win.show();
        let _ = win.set_focus();
        return Ok(());
    }

    let url = WebviewUrl::App(format!("{html}?{query}").into());
    WebviewWindowBuilder::new(app, label, url)
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

/// Open the borderless, centered, always-on-top countdown window for
/// `meeting_id`. Shared by the `show_countdown` command (invoked from the UI)
/// and the Rust-side meeting bridge, which can't `invoke` a command.
pub(crate) fn open_countdown_window(
    app: &AppHandle,
    app_name: &str,
    meeting_id: i64,
) -> tauri::Result<()> {
    show_countdown_popup(app, COUNTDOWN_LABEL, COUNTDOWN_HTML, app_name, meeting_id)
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

    // Hide, don't close: the window is reused on the next meeting (see
    // `show_countdown_popup`), and closing the WKWebView here would risk the
    // macOS teardown use-after-free. The page has already cleared its own timer.
    if let Some(win) = app.get_webview_window(COUNTDOWN_LABEL) {
        let _ = win.hide();
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
    show_countdown_popup(
        app,
        STOP_COUNTDOWN_LABEL,
        STOP_COUNTDOWN_HTML,
        app_name,
        meeting_id,
    )
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
    // Hide the window *first*, unconditionally: it's borderless + always-on-top
    // with no title bar, so if the send below fails (e.g. the bridge task is
    // gone) we must not leave an un-dismissable overlay floating over everything.
    // Hide (not close) keeps the WKWebView alive for reuse and avoids the macOS
    // teardown use-after-free; the page has already cleared its own timer.
    if let Some(win) = app.get_webview_window(STOP_COUNTDOWN_LABEL) {
        let _ = win.hide();
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

    #[test]
    fn reuse_url_preserves_origin_and_swaps_path_query() {
        // Bundled asset protocol: scheme + host must survive the rewrite so the
        // reused window navigates to the same origin, just a fresh countdown.
        let current =
            Url::parse("tauri://localhost/countdown.html?app=Old&meeting_id=1&seconds=5").unwrap();
        let out = reuse_url(&current, COUNTDOWN_HTML, "app=New&meeting_id=2&seconds=9");
        assert_eq!(out.scheme(), "tauri");
        assert_eq!(out.host_str(), Some("localhost"));
        assert_eq!(out.path(), "/countdown.html");
        assert_eq!(out.query(), Some("app=New&meeting_id=2&seconds=9"));
    }

    #[test]
    fn reuse_url_works_against_a_dev_server_origin() {
        let current = Url::parse("http://localhost:1420/countdown.html").unwrap();
        let out = reuse_url(
            &current,
            STOP_COUNTDOWN_HTML,
            "app=Zoom&meeting_id=3&seconds=5",
        );
        assert_eq!(out.scheme(), "http");
        assert_eq!(out.host_str(), Some("localhost"));
        assert_eq!(out.port(), Some(1420));
        assert_eq!(out.path(), "/stop-countdown.html");
        assert_eq!(out.query(), Some("app=Zoom&meeting_id=3&seconds=5"));
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
