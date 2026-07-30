//! On-device transcription model management for the settings UI.
//!
//! The local Whisper model lives in `~/.gilb/models/` and its presence is the
//! gate that enables meeting transcription (see `gilb_transcribe` and
//! `meeting::maybe_spawn_transcription`). These commands let the user download
//! it (with progress), delete it, and pick the transcription language.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager, State};

use crate::transcribe_worker::{TranscribeTx, TranscriptionJob};

/// HuggingFace URL for the ggml large-v3-turbo (q5_0) model we ship.

/// Emit a progress update at most every ~4 MB to avoid flooding the webview.

/// Shared cancel flag for an in-flight download, managed by Tauri so
/// [`cancel_model_download`] can signal [`download_model`].
#[derive(Default)]
pub struct DownloadCancel(pub Arc<AtomicBool>);

/// Snapshot of the local transcription setup, read by the settings UI on open.
#[derive(Clone, serde::Serialize)]
pub struct TranscriptionStatus {
    /// Whether the model is present — the gate for transcription.
    model_downloaded: bool,
    /// Size on disk in bytes (0 when absent).
    model_bytes: u64,
    /// Persisted language: "auto" | "ru" | "en".
    language: String,
}

/// Read the model presence/size and the persisted language.
#[tauri::command]
pub async fn get_transcription_status() -> TranscriptionStatus {
    let language = gilb_config::load_preferences().transcription_language;
    let (model_downloaded, model_bytes) = gilb_config::transcribe_model_path()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| (true, m.len()))
        .unwrap_or((false, 0));
    TranscriptionStatus {
        model_downloaded,
        model_bytes,
        language,
    }
}

/// Persist the transcription language. Rejects anything but `auto`/`ru`/`en`.
/// Signals the worker to drop its warm model so the next meeting reloads with
/// the new language.
#[tauri::command]
pub async fn set_transcription_language(
    tx: State<'_, TranscribeTx>,
    language: String,
) -> Result<(), String> {
    match language.as_str() {
        "auto" | "ru" | "en" => {}
        other => return Err(format!("unsupported language: {other}")),
    }
    let mut prefs = gilb_config::load_preferences();
    prefs.transcription_language = language;
    gilb_config::save_preferences(&prefs).map_err(|e| e.to_string())?;
    let _ = tx.0.send(TranscriptionJob::ReloadModel);
    Ok(())
}

/// Delete the downloaded model, turning transcription off. A no-op if absent.
#[tauri::command]
pub async fn delete_model() -> Result<(), String> {
    let path = gilb_config::transcribe_model_path().map_err(|e| e.to_string())?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Signal an in-flight [`download_model`] to stop at the next chunk boundary.
#[tauri::command]
pub fn cancel_model_download(cancel: State<'_, DownloadCancel>) {
    cancel.0.store(true, Ordering::SeqCst);
}

/// One `model-download` event. `status` is `progress` | `done` | `cancelled` |
/// `error`; `error` is set only on `error`.
#[derive(Clone, serde::Serialize)]
struct Progress {
    status: &'static str,
    downloaded: u64,
    total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Download the model into `~/.gilb/models/`, emitting `model-download` progress
/// events. The bytes go to a `.part` file that is renamed into place only on
/// success, so a partial/aborted download is never seen as a valid model.
/// Best-effort cancellable via [`cancel_model_download`].
#[tauri::command]
pub async fn download_model(
    app: AppHandle,
    cancel: State<'_, DownloadCancel>,
) -> Result<(), String> {
    cancel.0.store(false, Ordering::SeqCst);
    let dir = gilb_config::ensure_models_dir().map_err(|e| e.to_string())?;
    let final_path = dir.join(gilb_config::TRANSCRIBE_MODEL_FILE);

    let progress_app = app.clone();
    let outcome = gilb_shell_tauri::model::download(
        gilb_config::TRANSCRIBE_MODEL_URL,
        &final_path,
        &cancel.0,
        move |downloaded, total| {
            let _ = progress_app.emit(
                "model-download",
                Progress {
                    status: "progress",
                    downloaded,
                    total,
                    error: None,
                },
            );
        },
    )
    .await;

    match outcome {
        Ok(gilb_shell_tauri::model::Downloaded::Completed(downloaded)) => {
            let _ = app.emit(
                "model-download",
                Progress {
                    status: "done",
                    downloaded,
                    total: downloaded,
                    error: None,
                },
            );
            // A model is now available — sweep meetings still awaiting a transcript.
            if let Some(tx) = app.try_state::<TranscribeTx>() {
                let _ = tx.0.send(TranscriptionJob::Sweep);
            }
            Ok(())
        }
        Ok(gilb_shell_tauri::model::Downloaded::Cancelled) => {
            let _ = app.emit(
                "model-download",
                Progress {
                    status: "cancelled",
                    downloaded: 0,
                    total: 0,
                    error: None,
                },
            );
            Ok(())
        }
        Err(e) => {
            let _ = app.emit("model-download", err_event(e.to_string()));
            Err(e.to_string())
        }
    }
}

fn err_event(error: String) -> Progress {
    Progress {
        status: "error",
        downloaded: 0,
        total: 0,
        error: Some(error),
    }
}
