//! Bridge `gilb-events::EventBus` → Tauri `emit`. Each `BusMessage` from the
//! bus is forwarded to the webview under the matching event name:
//!
//! - `health`     — `BusMessage<HealthEvent>`
//! - `recording`  — `BusMessage<RecordingEvent>`
//! - `permission` — the current `Permissions` snapshot, emitted on change
//!
//! Tasks live until the broadcast sender is dropped (i.e. process exit).

use std::sync::Arc;

use gilb_engine::Engine;
use gilb_events::EventBus;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::broadcast::error::RecvError;
use tracing::warn;

const HEALTH_EVENT: &str = "health";
const RECORDING_EVENT: &str = "recording";
#[cfg(target_os = "macos")]
const PERMISSION_EVENT: &str = "permission";

/// Subscribe to the health + recording channels on the engine's event bus and
/// forward each message to the webview. Spawned tasks own the receivers.
pub fn spawn_proxies(app: AppHandle, engine: Arc<Engine>) {
    let bus = engine.event_bus().clone();
    spawn_proxy_health(app.clone(), &bus);
    spawn_proxy_recording(app.clone(), &bus);
    // Permission grants change in System Settings, outside our process, so
    // nothing pushes them onto the bus — the watcher polls instead. The
    // permissions splash is macOS-only, and so is the watcher.
    #[cfg(target_os = "macos")]
    spawn_permission_watcher(app, engine);
}

fn spawn_proxy_health(app: AppHandle, bus: &EventBus) {
    let mut rx = bus.subscribe_health();
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(msg) => emit(&app, HEALTH_EVENT, &msg),
                Err(RecvError::Lagged(n)) => {
                    warn!(skipped = n, "health proxy lagged");
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
}

fn spawn_proxy_recording(app: AppHandle, bus: &EventBus) {
    let mut rx = bus.subscribe_recording();
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(msg) => emit(&app, RECORDING_EVENT, &msg),
                Err(RecvError::Lagged(n)) => {
                    warn!(skipped = n, "recording proxy lagged");
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// Poll the permission snapshot once a second and emit `permission` when it
/// changes, so the splash clears right after a grant in System Settings
/// instead of whenever the frontend's 5-second poll happens to run. Exits
/// once everything is granted — the splash never reappears on its own, and a
/// revocation from Settings is still caught by that same 5-second poll.
#[cfg(target_os = "macos")]
fn spawn_permission_watcher(app: AppHandle, engine: Arc<Engine>) {
    tauri::async_runtime::spawn(async move {
        let mut last: Option<(bool, bool, bool, bool)> = None;
        loop {
            let perms = engine
                .status()
                .await
                .map(|s| s.permissions)
                .unwrap_or_default();
            let snapshot = (
                perms.accessibility,
                perms.input_monitoring,
                perms.screen_recording,
                perms.microphone,
            );
            // The first sample only seeds the comparison — the frontend
            // already refreshed on launch.
            if last.replace(snapshot).is_some_and(|prev| prev != snapshot) {
                emit(&app, PERMISSION_EVENT, &perms);
            }
            if snapshot == (true, true, true, true) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });
}

fn emit<T: Serialize + Clone>(app: &AppHandle, name: &str, payload: &T) {
    if let Err(err) = app.emit(name, payload.clone()) {
        warn!(?err, event = name, "failed to emit to webview");
    }
}
