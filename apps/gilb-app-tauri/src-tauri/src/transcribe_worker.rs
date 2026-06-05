//! Background transcription worker.
//!
//! A single long-lived task owns the queue and the loaded Whisper model, so
//! meetings are transcribed **one at a time** (bounded RAM, no concurrent model
//! loads). The model is loaded lazily on the first job, kept warm, and unloaded
//! after [`IDLE_UNLOAD`] of inactivity to release ~570 MB.
//!
//! Reliability: meetings are enqueued (never transcribed inline), and a `Sweep`
//! re-scans the DB for completed meetings without a transcript — fired at
//! startup and after a model download — so nothing is lost if the app was closed
//! before/during transcription.

use std::path::Path;
use std::time::Duration;

use gilb_config::{load_preferences, transcribe_model_path};
use gilb_db::meetings::{get_meeting, pending_transcriptions};
use gilb_db::Db;
use gilb_transcribe::{transcribe_meeting, LocalTranscriber};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn};

/// Drop the warm model after this much idle time.
const IDLE_UNLOAD: Duration = Duration::from_secs(5 * 60);

/// Work item for the transcription worker.
pub enum TranscriptionJob {
    /// Transcribe this meeting (if it has audio and isn't done yet).
    Meeting(i64),
    /// Re-scan the DB and enqueue every meeting still needing a transcript.
    Sweep,
    /// Drop the warm model so the next job reloads it (e.g. language changed).
    ReloadModel,
}

/// Tauri-managed handle for enqueuing transcription work.
pub struct TranscribeTx(pub UnboundedSender<TranscriptionJob>);

/// Spawn the worker task and return its sender (manage it as Tauri state). Fires
/// an initial `Sweep` so meetings missed while the app was closed get picked up.
pub fn spawn_transcription_worker(db: Db) -> TranscribeTx {
    let (tx, rx) = mpsc::unbounded_channel();
    let self_tx = tx.clone();
    tauri::async_runtime::spawn(run_worker(db, rx, self_tx));
    let _ = tx.send(TranscriptionJob::Sweep);
    TranscribeTx(tx)
}

/// Load the model off the async runtime (it's a blocking, ~570 MB read).
/// Returns `None` if no model is downloaded or loading fails.
async fn load_model() -> Option<LocalTranscriber> {
    let path = transcribe_model_path().ok()?;
    if !path.exists() {
        debug!("no local transcription model; jobs will wait for a download");
        return None;
    }
    let language = load_preferences().transcription_language;
    match tauri::async_runtime::spawn_blocking(move || LocalTranscriber::new(&path, language)).await
    {
        Ok(Ok(model)) => Some(model),
        Ok(Err(err)) => {
            warn!(error = %err, "failed to load transcription model");
            None
        }
        Err(err) => {
            warn!(error = %err, "model-load task panicked");
            None
        }
    }
}

async fn run_worker(
    db: Db,
    mut rx: UnboundedReceiver<TranscriptionJob>,
    self_tx: UnboundedSender<TranscriptionJob>,
) {
    let mut model: Option<LocalTranscriber> = None;
    loop {
        // Wait for the next job; while a model is warm, unload it after idle.
        let job = if model.is_some() {
            tokio::select! {
                job = rx.recv() => job,
                _ = tokio::time::sleep(IDLE_UNLOAD) => {
                    debug!("unloading idle transcription model");
                    model = None;
                    continue;
                }
            }
        } else {
            rx.recv().await
        };
        let Some(job) = job else { break }; // all senders dropped → shut down

        match job {
            TranscriptionJob::ReloadModel => {
                model = None;
            }
            TranscriptionJob::Sweep => match pending_transcriptions(&db).await {
                Ok(ids) => {
                    if !ids.is_empty() {
                        info!(count = ids.len(), "enqueuing pending transcriptions");
                    }
                    for id in ids {
                        let _ = self_tx.send(TranscriptionJob::Meeting(id));
                    }
                }
                Err(err) => warn!(error = %err, "failed to scan pending transcriptions"),
            },
            TranscriptionJob::Meeting(meeting_id) => {
                let audio_path = match get_meeting(&db, meeting_id).await {
                    Ok(Some(m)) => m.audio_path,
                    Ok(None) => None,
                    Err(err) => {
                        warn!(meeting_id, error = %err, "failed to load meeting");
                        continue;
                    }
                };
                let Some(audio_path) = audio_path else {
                    debug!(meeting_id, "meeting has no audio; skipping");
                    continue;
                };

                if model.is_none() {
                    model = load_model().await;
                }
                let Some(transcriber) = model.as_ref() else {
                    // No model yet — leave the meeting pending; a later Sweep
                    // (e.g. after the model downloads) will retry it.
                    continue;
                };

                if let Err(err) =
                    transcribe_meeting(&db, meeting_id, Path::new(&audio_path), transcriber).await
                {
                    warn!(meeting_id, error = %err, "failed to persist transcription");
                }
            }
        }
    }
}
