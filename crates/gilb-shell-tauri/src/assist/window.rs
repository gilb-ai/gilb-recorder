//! The overlay window: borderless, transparent, always-on-top, excluded from
//! our own screen capture, and never focused — stealing focus from the
//! meeting app mid-call is worse than a second click.
//!
//! One behaviour rule lives here: an explicit hide is remembered, and the
//! panel stops popping itself up until the user reopens it (or a new meeting
//! starts). Fighting a deliberate close is worse than a missed suggestion.

use std::sync::atomic::Ordering;

use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tracing::warn;

use super::{state, ASSIST_WINDOW};

/// Show/hide the overlay. Deliberately not the record hotkey's neighbour:
/// `Cmd+M` is already manual recording in shells that bind it.
const ASSIST_SHORTCUT: &str = "CmdOrCtrl+Backslash";

/// Tell the overlay whether audio is actually reaching it.
///
/// The panel listens to a tap the recorder feeds, so with nothing recording it
/// hears nothing — and said nothing about it, which makes an idle panel
/// indistinguishable from a broken one. Someone who opens it and starts
/// talking has every reason to expect otherwise.
pub(super) fn emit_listening(app: &AppHandle, on: bool) {
    let _ = app.emit_to(
        ASSIST_WINDOW,
        "assist://listening",
        serde_json::json!({ "on": on }),
    );
}

/// Create the overlay if it doesn't exist yet — hidden, transparent,
/// always-on-top, excluded from our own screen capture, and never focused
/// (stealing focus from the meeting app mid-call is worse than a second click).
pub(super) fn ensure_window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    if let Some(window) = app.get_webview_window(ASSIST_WINDOW) {
        return Some(window);
    }
    let title = state(app)
        .map(|s| s.host.strings().window_title)
        .unwrap_or_default();
    let builder =
        WebviewWindowBuilder::new(app, ASSIST_WINDOW, WebviewUrl::App("assist.html".into()))
            .title(title)
            .inner_size(380.0, 520.0)
            .resizable(true)
            .decorations(false)
            .transparent(true)
            .always_on_top(true)
            .skip_taskbar(true)
            .visible_on_all_workspaces(true)
            .content_protected(true)
            .focused(false)
            .visible(false);

    // No NSVisualEffectView here: the vibrancy material paints the window
    // opaque, which defeats transparent(true) — tried, and the panel came out
    // solid. Translucency stays a plain CSS alpha in assist.html.

    match builder.build() {
        Ok(window) => Some(window),
        Err(err) => {
            warn!(error = %err, "failed to create assist window");
            None
        }
    }
}

pub(super) fn show_window(app: &AppHandle) {
    if let Some(window) = ensure_window(app) {
        let _ = window.show();
    }
}

pub(super) fn auto_show_suppressed(app: &AppHandle) -> bool {
    state(app).is_some_and(|s| s.auto_show_suppressed.load(Ordering::SeqCst))
}

/// Remember (or forget) that the panel was closed on purpose.
pub(super) fn set_auto_show_suppressed(app: &AppHandle, suppressed: bool) {
    if let Some(state) = state(app) {
        state
            .auto_show_suppressed
            .store(suppressed, Ordering::SeqCst);
    }
}

pub(super) fn toggle_window(app: &AppHandle) {
    match app.get_webview_window(ASSIST_WINDOW) {
        Some(window) if window.is_visible().unwrap_or(false) => {
            let _ = window.hide();
            set_auto_show_suppressed(app, true);
        }
        // Reopening by hand is consent to see suggestions again.
        _ => {
            set_auto_show_suppressed(app, false);
            show_window(app);
        }
    }
}

pub(super) fn register_shortcut(app: &AppHandle) {
    crate::shortcut::register(app, ASSIST_SHORTCUT, toggle_window);
}
