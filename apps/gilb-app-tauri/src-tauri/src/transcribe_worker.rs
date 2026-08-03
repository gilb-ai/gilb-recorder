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
use gilb_transcribe::SharedModel;
use gilb_transcribe::{transcribe_meeting, LocalTranscriber};
use std::sync::{Arc, OnceLock};

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn};

/// Drop the warm model after this much idle time. The window belongs to the
/// shared owner now: real-time suggestions borrow the same instance, and
/// whoever goes idle first must not pull it out from under the other.
const IDLE_UNLOAD: Duration = Duration::from_secs(5 * 60);

/// Work item for the transcription worker.
pub enum TranscriptionJob {
    /// Transcribe this meeting (if it has audio and isn't done yet).
    Meeting(i64),
    /// Re-scan the DB and enqueue every meeting still needing a transcript.
    Sweep,
    /// Drop the warm model so the next job reloads it (e.g. language changed).
    /// Drops the shared cache entry too — otherwise the next job would get the
    /// old model right back from the realtime side's cache.
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

/// The process-wide model. Real-time suggestions ask the same handle, so a
/// meeting that ends while the overlay is still warm no longer costs a second
/// ~570 MB copy.
pub fn shared_model() -> Arc<SharedModel<LocalTranscriber>> {
    static SHARED: OnceLock<Arc<SharedModel<LocalTranscriber>>> = OnceLock::new();
    SHARED.get_or_init(|| SharedModel::new(IDLE_UNLOAD)).clone()
}

/// Borrow the model, loading it if this is the first ask. `None` when no model
/// is downloaded or loading fails — jobs then wait for a download.
async fn load_model() -> Option<Arc<LocalTranscriber>> {
    let path = transcribe_model_path().ok()?;
    if !path.exists() {
        debug!("no local transcription model; jobs will wait for a download");
        return None;
    }
    let language = load_preferences().transcription_language;
    // The language keys the shared cache: after a language change the next
    // job gets a fresh model, not the realtime side's instance (or vice versa).
    let key = language.clone();
    match shared_model()
        .get(&key, move || LocalTranscriber::new(&path, language))
        .await
    {
        Ok(model) => Some(model),
        Err(err) => {
            warn!(error = %err, "failed to load transcription model");
            None
        }
    }
}

async fn run_worker(
    db: Db,
    mut rx: UnboundedReceiver<TranscriptionJob>,
    self_tx: UnboundedSender<TranscriptionJob>,
) {
    let mut model: Option<Arc<LocalTranscriber>> = None;
    loop {
        // Wait for the next job; while a model is warm, unload it after idle.
        let job = if model.is_some() {
            tokio::select! {
                job = rx.recv() => job,
                _ = tokio::time::sleep(IDLE_UNLOAD) => {
                    debug!("transcription worker idle; releasing the model");
                    model = None;
                    // Ask the shared owner to drop its reference too; it
                    // refuses while suggestions are still using it.
                    shared_model().unload_if_idle();
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
                // The worker's borrow is gone; evict the shared cache entry
                // too so the next job reloads with the new configuration now,
                // not after both consumers have idled out.
                shared_model().invalidate();
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

                if let Err(err) = transcribe_meeting(
                    &db,
                    meeting_id,
                    Path::new(&audio_path),
                    transcriber.as_ref(),
                )
                .await
                {
                    warn!(meeting_id, error = %err, "failed to persist transcription");
                }
            }
        }
    }
}
