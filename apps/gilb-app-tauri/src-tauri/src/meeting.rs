//! Tauri adapter over [`gilb_pipeline`] — the app-agnostic meeting bridge
//! lives in `crates/gilb-pipeline`; this module supplies its [`MeetingUi`]
//! (countdown popup windows, the `meeting-recording` webview event, the
//! transcription queue) and parks the control handles in Tauri managed state
//! for the countdown/settings commands.

use std::path::PathBuf;

use gilb_db::Db;
use gilb_events::EventBus;
use gilb_pipeline::{meeting_pipeline, MeetingUi, RecordingStatus};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;
use tracing::warn;

use crate::commands::countdown::{open_countdown_window, open_stop_countdown_window};
use crate::transcribe_worker::{TranscribeTx, TranscriptionJob};

pub use gilb_pipeline::StopResolution;

/// Webview event carrying the in-app "recording a meeting now" indicator state.
const MEETING_RECORDING_EVENT: &str = "meeting-recording";

/// Managed Tauri state holding the sender half of the pipeline's stop-countdown
/// resolution channel, so the `resolve_stop_countdown` command can hand the
/// user's choice back to the bridge task.
pub struct StopCountdownTx(pub mpsc::Sender<StopResolution>);

/// Managed Tauri state for toggling meeting detection (subsystem B) at runtime.
/// `set_meeting_detection` sends `true`/`false` here; the detector supervisor
/// starts or stops the platform detector accordingly — no restart needed.
pub struct MeetingControlTx(pub mpsc::Sender<bool>);

/// [`MeetingUi`] backed by the Tauri shell: countdowns are popup windows, the
/// indicator is a webview event, and finished recordings go to the
/// transcription worker.
struct TauriMeetingUi {
    app: AppHandle,
}

impl MeetingUi for TauriMeetingUi {
    fn open_start_countdown(&self, display_name: &str, meeting_id: i64) -> anyhow::Result<()> {
        Ok(open_countdown_window(&self.app, display_name, meeting_id)?)
    }

    fn open_stop_countdown(&self, display_name: &str, meeting_id: i64) -> anyhow::Result<()> {
        Ok(open_stop_countdown_window(
            &self.app,
            display_name,
            meeting_id,
        )?)
    }

    fn recording_status(&self, status: &RecordingStatus) {
        if let Err(err) = self.app.emit(MEETING_RECORDING_EVENT, status) {
            warn!(?err, "failed to emit meeting-recording status");
        }
    }

    /// Enqueue a finished meeting for on-device transcription. The background
    /// worker (`transcribe_worker`) owns the model + queue and does the rest;
    /// gating on a downloaded model, audio presence, and persistence all happen
    /// there. A no-op if the worker isn't registered. Non-blocking — never
    /// stalls the bridge.
    fn recording_stopped(&self, meeting_id: i64) {
        match self.app.try_state::<TranscribeTx>() {
            Some(tx) => {
                let _ = tx.0.send(TranscriptionJob::Meeting(meeting_id));
            }
            None => warn!(meeting_id, "transcription worker not available; skipping"),
        }
    }
}

/// Start the meeting pipeline: build it with the Tauri-backed UI, manage the
/// control handles, and spawn the bridge on Tauri's async runtime. Returns
/// immediately; the bridge runs on a detached task for the life of the app.
pub fn spawn_meeting_pipeline(app: AppHandle, bus: EventBus, db: Db, data_dir: PathBuf) {
    let enabled = gilb_config::load_preferences().meeting_detection_enabled;
    let (handles, bridge) = meeting_pipeline(
        TauriMeetingUi { app: app.clone() },
        bus,
        db,
        data_dir,
        enabled,
    );
    app.manage(StopCountdownTx(handles.stop_tx));
    app.manage(MeetingControlTx(handles.detection_ctl_tx));
    // The recorder and detector are spawned when the bridge future first runs,
    // inside Tauri's tokio runtime — `spawn_meeting_pipeline` itself stays safe
    // to call from the synchronous `setup` hook.
    tauri::async_runtime::spawn(bridge);
}
