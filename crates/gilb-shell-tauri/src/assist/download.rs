//! The whisper-model gate: the feature is off until the ~570 MB model exists,
//! and turning the switch on is what downloads it — no separate button, and
//! the stack wires itself up when the file lands.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use tauri::{AppHandle, Manager};
use tracing::{info, warn};

use super::{emit_status, is_enabled, state, wire, AssistState};

#[derive(Default)]
pub(super) struct ModelDownload {
    pub(super) active: AtomicBool,
    pub(super) percent: AtomicU8,
    /// Raised when the user turns the feature off mid-download.
    pub(super) cancel: AtomicBool,
}

/// Download the model in the background (D9). On success the stack wires
/// itself up — no restart needed.
pub(super) fn start_model_download(app: &AppHandle) {
    use tauri_plugin_notification::NotificationExt;

    let Some(state) = state(app) else { return };
    if state.download.active.swap(true, Ordering::SeqCst) {
        return; // already running
    }
    state.download.percent.store(0, Ordering::SeqCst);
    state.download.cancel.store(false, Ordering::SeqCst);
    let strings = state.host.strings();
    let url = state.host.model_url();
    emit_status(app);

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let result = async {
            gilb_config::ensure_models_dir()?;
            let path = gilb_config::transcribe_model_path()?;
            let progress_app = app.clone();
            let state = app.state::<AssistState>();
            crate::model::download(&url, &path, &state.download.cancel, move |done, total| {
                if total == 0 {
                    return;
                }
                let pct = (done * 100 / total).min(100) as u8;
                let Some(state) = progress_app.try_state::<AssistState>() else {
                    return;
                };
                if state.download.percent.swap(pct, Ordering::SeqCst) != pct {
                    emit_status(&progress_app);
                }
            })
            .await
        }
        .await;

        if let Some(state) = app.try_state::<AssistState>() {
            state.download.active.store(false, Ordering::SeqCst);
        }
        match result {
            Ok(crate::model::Downloaded::Cancelled) => info!("assist model download cancelled"),
            Ok(crate::model::Downloaded::Completed(_)) => {
                info!("assist model downloaded");
                let _ = app
                    .notification()
                    .builder()
                    .title(&strings.app_name)
                    .body(&strings.model_downloaded)
                    .show();
                if is_enabled() {
                    wire(&app);
                }
            }
            Err(err) => {
                warn!(error = %err, "assist model download failed");
                let _ = app
                    .notification()
                    .builder()
                    .title(&strings.app_name)
                    .body(&strings.model_failed)
                    .show();
            }
        }
        emit_status(&app);
    });
}
