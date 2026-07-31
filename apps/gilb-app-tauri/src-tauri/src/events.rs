//! Bridge `gilb-events::EventBus` → Tauri `emit`. Each `BusMessage` from the
//! bus is forwarded to the webview under the matching event name:
//!
//! - `health`     — `BusMessage<HealthEvent>`
//! - `recording`  — `BusMessage<RecordingEvent>`
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

/// Subscribe to the health + recording channels on the engine's event bus and
/// forward each message to the webview. Spawned tasks own the receivers.
pub fn spawn_proxies(app: AppHandle, engine: Arc<Engine>) {
    let bus = engine.event_bus().clone();
    spawn_proxy_health(app.clone(), &bus);
    spawn_proxy_recording(app, &bus);
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

fn emit<T: Serialize + Clone>(app: &AppHandle, name: &str, payload: &T) {
    if let Err(err) = app.emit(name, payload.clone()) {
        warn!(?err, event = name, "failed to emit to webview");
    }
}
