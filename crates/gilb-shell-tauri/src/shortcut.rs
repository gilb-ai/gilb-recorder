//! Global (system-wide) hotkeys, registered the careful way.
//!
//! Two things make this worth a module rather than a call site:
//!
//! **`global_shortcut()` panics** when the product did not register the
//! plugin, and it panics inside a Tauri callback that cannot unwind — so the
//! process aborts. A shell must not take a whole product down over a keyboard
//! shortcut; it asks first and degrades to "no hotkey" instead.
//!
//! **These keys are taken from every app on the machine.** A global shortcut is
//! not a menu accelerator: once registered it fires whether or not the user was
//! looking at us, and the app that used to own it stops seeing it. That is a
//! real cost, so a shell binds few of them and picks combinations that are not
//! already load-bearing somewhere else.

use tauri::AppHandle;
use tracing::{info, warn};

/// Bind `accelerator` to `on_pressed`, or explain why not.
///
/// Returns whether the binding took. Callers treat `false` as "the feature is
/// still reachable from the UI, just not from the keyboard" — never as fatal.
pub fn register(
    app: &AppHandle,
    accelerator: &'static str,
    on_pressed: impl Fn(&AppHandle) + Send + Sync + 'static,
) -> bool {
    // Mobile has no global shortcuts, and the plugin does not build there.
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = (app, accelerator, on_pressed);
        false
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        use tauri::Manager;
        use tauri_plugin_global_shortcut::{GlobalShortcut, GlobalShortcutExt, ShortcutState};

        if app.try_state::<GlobalShortcut<tauri::Wry>>().is_none() {
            warn!(
                accelerator,
                "no global-shortcut plugin registered; hotkey disabled. \
                 Add tauri_plugin_global_shortcut to the app's builder to enable it"
            );
            return false;
        }

        let result =
            app.global_shortcut()
                .on_shortcut(accelerator, move |app, _shortcut, event| {
                    // Both edges arrive; acting on the release too would run
                    // everything twice.
                    if event.state() == ShortcutState::Pressed {
                        on_pressed(app);
                    }
                });
        match result {
            Ok(()) => {
                info!(accelerator, "hotkey registered");
                true
            }
            // Most often: another app already owns the combination. Worth a
            // line in the log, never worth failing startup.
            Err(err) => {
                warn!(?err, accelerator, "failed to register hotkey");
                false
            }
        }
    }
}
