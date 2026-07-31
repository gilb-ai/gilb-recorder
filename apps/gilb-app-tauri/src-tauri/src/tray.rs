//! gilb's system tray — its first tray (until now gilb had only the main
//! window). The presentation (menu, idle↔recording icon flip, dedup'd refresh,
//! event routing) lives in `gilb_shell_tauri::tray`; this module supplies gilb's
//! wording and icons plus the [`TrayController`] that binds menu actions to
//! gilb's [`AppState`].
//!
//! Menu: Open gilb / Start Recording|Stop Recording / Quit.

use gilb_shell_tauri::tray::{self, TrayConfig, TrayController};
use tauri::{AppHandle, Manager};

use crate::recording;
use crate::state::AppState;

const TRAY_ID: &str = "gilb-tray";

#[cfg(target_os = "macos")]
const ICON_IDLE: &[u8] = include_bytes!("../icons/tray/tray-iconTemplate@2x.png");
#[cfg(target_os = "macos")]
const ICON_RECORDING: &[u8] = include_bytes!("../icons/tray/tray-iconTemplate-recording@2x.png");
#[cfg(not(target_os = "macos"))]
const ICON_IDLE: &[u8] = include_bytes!("../icons/tray/tray-icon.png");
#[cfg(not(target_os = "macos"))]
const ICON_RECORDING: &[u8] = include_bytes!("../icons/tray/tray-icon-recording.png");

/// Binds the reusable tray to gilb's [`AppState`]: recording state selects the
/// icon/label, and the menu actions open the window, toggle a manual recording,
/// or stop-then-quit.
struct GilbTrayController;

impl TrayController for GilbTrayController {
    fn is_recording(&self, app: &AppHandle) -> bool {
        app.try_state::<AppState>()
            .is_some_and(|state| state.recording.lock().is_some())
    }

    fn status_line(&self, _app: &AppHandle) -> Option<String> {
        // gilb has no tray status line yet; the menu omits the line.
        None
    }

    fn account_line(&self, _app: &AppHandle) -> Option<String> {
        // The signed-in user's email, shown at the top of the menu. gilb-web
        // sends it as the `employee` label in the auth callback (see
        // commands::auth), persisted in <Documents>/Gilb/credentials.json. `None` while
        // signed out, so the shared renderer hides the line.
        gilb_config::load_credentials()
            .ok()
            .flatten()
            .and_then(|creds| creds.employee)
    }

    fn on_open(&self, app: &AppHandle) {
        show_main_window(app);
    }

    fn on_toggle(&self, app: &AppHandle) {
        recording::toggle(app);
    }

    fn on_quit(&self, app: &AppHandle) {
        // Stop an active recording first — a bare exit would kill the capture
        // mid-write and lose the meeting.
        recording::stop_then_quit(app);
    }
}

/// Build gilb's tray. Call once from `setup`, after `AppState` is managed.
pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    tray::setup(
        app,
        TrayConfig {
            tray_id: TRAY_ID.into(),
            tooltip: "gilb".into(),
            open_label: "Open gilb".into(),
            start_label: "Start Recording".into(),
            stop_label: "Stop Recording".into(),
            quit_label: "Quit".into(),
            icon_idle: ICON_IDLE,
            icon_recording: ICON_RECORDING,
        },
        GilbTrayController,
    )
}

/// Re-render the tray (icon + menu) from current [`AppState`] — called by the
/// meeting hook whenever the recording indicator changes.
pub fn refresh(app: &AppHandle) {
    tray::refresh(app);
}

fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.set_focus();
    }
}
