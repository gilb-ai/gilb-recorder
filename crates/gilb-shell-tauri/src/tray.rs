//! Reusable system-tray presentation for gilb-based shells.
//!
//! The crate owns the *presentation*: building the menu, flipping the icon
//! between idle and recording, the dedup'd [`refresh`], and routing menu
//! clicks. The shell owns the *state* behind a [`TrayController`] — whether a
//! recording is active, the optional status line, and what open/toggle/quit
//! actually do — so a differently-branded shell reuses this module instead of
//! copying it.
//!
//! Menu layout:
//!   [open] / [shell's extra items] / [optional status line] /
//!   [start|stop recording] / --- / [quit]
//!
//! A shell can declare its own clickable items via [`TrayConfig::extra_items`]
//! (e.g. "Sign in" / "Check permissions" / "Settings"); clicks route to
//! [`TrayController::on_custom`]. Empty by default, so a shell that wants only
//! the built-in items leaves it untouched.
//!
//! On macOS the icons are template PNGs (`icon_as_template`) so they adapt to
//! the menu-bar theme.

use parking_lot::Mutex;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, Wry};
use tracing::warn;

/// Built-in menu-item ids. A shell's [`TrayController::extra_items`] must not
/// reuse these — a collision would route the built-in's click to `on_custom`.
const MENU_OPEN: &str = "open";
const MENU_TOGGLE: &str = "toggle-recording";
const MENU_QUIT: &str = "quit";

/// A shell-declared clickable menu item, inserted after Open. Its `id` is passed
/// back to [`TrayController::on_custom`] when clicked, so the shell routes the
/// action. `id` must not be one of the built-in ids (`open`, `toggle-recording`,
/// `quit`).
pub struct TrayMenuItem {
    pub id: String,
    pub label: String,
}

/// Static per-shell tray configuration: the menu wording, tooltip, tray id, and
/// the two icon variants (raw PNG bytes, decoded on demand). Differently-branded
/// shells supply their own.
pub struct TrayConfig {
    pub tray_id: String,
    pub tooltip: String,
    pub open_label: String,
    /// Toggle label while idle (e.g. "Начать запись").
    pub start_label: String,
    /// Toggle label while recording (e.g. "Остановить запись").
    pub stop_label: String,
    pub quit_label: String,
    pub icon_idle: &'static [u8],
    pub icon_recording: &'static [u8],
}

/// The dynamic half of the tray, owned by the shell. The crate calls these to
/// render the current state and to route clicks; the shell reads/writes its own
/// `AppState` behind them.
pub trait TrayController: Send + Sync + 'static {
    /// Whether a recording is active right now — selects the icon variant and
    /// the toggle label.
    fn is_recording(&self, app: &AppHandle) -> bool;
    /// Optional informational line shown between Open and the toggle (e.g. an
    /// upload/transcription status). `None` hides it.
    fn status_line(&self, app: &AppHandle) -> Option<String>;
    fn on_open(&self, app: &AppHandle);
    fn on_toggle(&self, app: &AppHandle);
    fn on_quit(&self, app: &AppHandle);
    /// Extra clickable items shown after Open (e.g. sign-in / permissions /
    /// settings). Re-evaluated on every render, so the shell can vary them by
    /// state — e.g. hide "Sign in" once signed in. Default: none. Clicks route
    /// to [`Self::on_custom`]; ids must not collide with the built-ins.
    fn extra_items(&self, _app: &AppHandle) -> Vec<TrayMenuItem> {
        Vec::new()
    }
    /// A shell-declared [`Self::extra_items`] entry was clicked, passed its
    /// `id`. Default no-op for shells with no extra items.
    fn on_custom(&self, _app: &AppHandle, _id: &str) {}
}

/// Config + controller, managed as Tauri state so the menu-event closure and
/// [`refresh`] can reach them.
struct TrayState {
    config: TrayConfig,
    controller: Box<dyn TrayController>,
}

fn icon(config: &TrayConfig, recording: bool) -> tauri::Result<Image<'static>> {
    Image::from_bytes(if recording {
        config.icon_recording
    } else {
        config.icon_idle
    })
}

/// The toggle menu item's label for the current recording state.
fn toggle_label(config: &TrayConfig, recording: bool) -> &str {
    if recording {
        &config.stop_label
    } else {
        &config.start_label
    }
}

fn build_menu(
    app: &AppHandle,
    config: &TrayConfig,
    recording: bool,
    status: Option<&str>,
    extra: &[TrayMenuItem],
) -> tauri::Result<Menu<Wry>> {
    let open = MenuItem::with_id(app, MENU_OPEN, &config.open_label, true, None::<&str>)?;
    let toggle = MenuItem::with_id(
        app,
        MENU_TOGGLE,
        toggle_label(config, recording),
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, MENU_QUIT, &config.quit_label, true, None::<&str>)?;

    let menu = Menu::new(app)?;
    menu.append(&open)?;
    // Shell-declared items (sign-in / permissions / settings …) — clicks route
    // to `on_custom` via their id.
    for item in extra {
        menu.append(&MenuItem::with_id(
            app,
            item.id.as_str(),
            &item.label,
            true,
            None::<&str>,
        )?)?;
    }
    if let Some(status) = status {
        // Informational, non-clickable line mirroring the shell's status.
        menu.append(&MenuItem::new(app, status, false, None::<&str>)?)?;
    }
    menu.append(&toggle)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&quit)?;
    Ok(menu)
}

/// Build the tray and store `config` + `controller` as Tauri state. Call once
/// from the shell's `setup`.
pub fn setup<C: TrayController>(
    app: &AppHandle,
    config: TrayConfig,
    controller: C,
) -> tauri::Result<()> {
    let recording = controller.is_recording(app);
    let status = controller.status_line(app);
    let extra = controller.extra_items(app);
    let menu = build_menu(app, &config, recording, status.as_deref(), &extra)?;
    TrayIconBuilder::with_id(config.tray_id.as_str())
        .icon(icon(&config, recording)?)
        .icon_as_template(cfg!(target_os = "macos"))
        .menu(&menu)
        .show_menu_on_left_click(true)
        .tooltip(config.tooltip.as_str())
        .on_menu_event(|app, event| {
            let Some(state) = app.try_state::<TrayState>() else {
                return;
            };
            match event.id.as_ref() {
                MENU_OPEN => state.controller.on_open(app),
                MENU_TOGGLE => state.controller.on_toggle(app),
                // Routing through the controller lets the shell stop an active
                // recording before exiting rather than killing it mid-write.
                MENU_QUIT => state.controller.on_quit(app),
                // Any other id belongs to a shell-declared extra item.
                other => state.controller.on_custom(app, other),
            }
        })
        .build(app)?;
    app.manage(TrayState {
        config,
        controller: Box::new(controller),
    });
    Ok(())
}

/// Last rendered (recording, status, extra items) — skip native menu/icon churn
/// when nothing visible changed (refresh can fire per status tick). The extra
/// items are part of the key so a state-driven change (e.g. "Sign in" appearing)
/// still re-renders even when recording/status are unchanged.
static LAST_RENDERED: Mutex<Option<(bool, Option<String>, Vec<(String, String)>)>> =
    Mutex::new(None);

/// Re-render the tray (icon + menu) from the [`TrayController`]. A no-op until
/// [`setup`] has run.
pub fn refresh(app: &AppHandle) {
    let Some(state) = app.try_state::<TrayState>() else {
        return;
    };
    let Some(tray) = app.tray_by_id(state.config.tray_id.as_str()) else {
        return;
    };
    let recording = state.controller.is_recording(app);
    let status = state.controller.status_line(app);
    let extra = state.controller.extra_items(app);
    let extra_key: Vec<(String, String)> = extra
        .iter()
        .map(|i| (i.id.clone(), i.label.clone()))
        .collect();

    let mut last = LAST_RENDERED.lock();
    let icon_changed = last.as_ref().map(|(r, _, _)| *r) != Some(recording);
    if last.as_ref() == Some(&(recording, status.clone(), extra_key.clone())) {
        return;
    }
    *last = Some((recording, status.clone(), extra_key));
    drop(last);

    if icon_changed {
        match icon(&state.config, recording) {
            Ok(img) => {
                if let Err(err) = tray.set_icon(Some(img)) {
                    warn!(?err, "failed to set tray icon");
                }
                let _ = tray.set_icon_as_template(cfg!(target_os = "macos"));
            }
            Err(err) => warn!(?err, "failed to load tray icon"),
        }
    }
    match build_menu(app, &state.config, recording, status.as_deref(), &extra) {
        Ok(menu) => {
            if let Err(err) = tray.set_menu(Some(menu)) {
                warn!(?err, "failed to update tray menu");
            }
        }
        Err(err) => warn!(?err, "failed to build tray menu"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> TrayConfig {
        TrayConfig {
            tray_id: "test-tray".into(),
            tooltip: "Test".into(),
            open_label: "Open".into(),
            start_label: "Start recording".into(),
            stop_label: "Stop recording".into(),
            quit_label: "Quit".into(),
            icon_idle: &[],
            icon_recording: &[],
        }
    }

    #[test]
    fn toggle_label_tracks_recording_state() {
        let cfg = config();
        assert_eq!(toggle_label(&cfg, false), "Start recording");
        assert_eq!(toggle_label(&cfg, true), "Stop recording");
    }

    #[test]
    fn extra_item_ids_do_not_collide_with_builtins() {
        let items = [
            TrayMenuItem {
                id: "sign-in".into(),
                label: "Sign in".into(),
            },
            TrayMenuItem {
                id: "settings".into(),
                label: "Settings".into(),
            },
        ];
        for item in &items {
            assert!(
                ![MENU_OPEN, MENU_TOGGLE, MENU_QUIT].contains(&item.id.as_str()),
                "extra item id {:?} collides with a built-in",
                item.id
            );
        }
    }
}
