//! gilb's [`ShellHooks`] over the reusable Tauri meeting glue in
//! `crates/gilb-shell-tauri`. The shared work (countdown windows, the
//! `meeting-recording` webview event) lives in the crate; gilb only diverges on
//! `recording_stopped`, where a finished meeting is enqueued for on-device
//! transcription.

use std::path::PathBuf;

use gilb_db::Db;
use gilb_events::EventBus;
use gilb_shell_tauri::{spawn_meeting_pipeline as shell_spawn, ShellHooks};
use tauri::{AppHandle, Manager};
use tracing::warn;

use crate::transcribe_worker::{TranscribeTx, TranscriptionJob};

/// gilb-specific divergence: a finished meeting recording goes to on-device
/// transcription. The countdown windows and `meeting-recording` indicator are
/// the crate's generic work.
struct GilbHooks;

impl ShellHooks for GilbHooks {
    /// Enqueue a finished meeting for on-device transcription. The background
    /// worker (`transcribe_worker`) owns the model + queue and does the rest;
    /// gating on a downloaded model, audio presence, and persistence all happen
    /// there. A no-op if the worker isn't registered. Non-blocking — never
    /// stalls the bridge.
    fn on_recording_stopped(&self, app: &AppHandle, meeting_id: i64) {
        match app.try_state::<TranscribeTx>() {
            Some(tx) => {
                let _ = tx.0.send(TranscriptionJob::Meeting(meeting_id));
            }
            None => warn!(meeting_id, "transcription worker not available; skipping"),
        }
    }
}

/// Start the meeting pipeline with gilb's hooks. Thin wrapper over the crate so
/// the `setup` call site stays unchanged.
pub fn spawn_meeting_pipeline(app: AppHandle, bus: EventBus, db: Db, data_dir: PathBuf) {
    shell_spawn(app, GilbHooks, bus, db, data_dir);
}
