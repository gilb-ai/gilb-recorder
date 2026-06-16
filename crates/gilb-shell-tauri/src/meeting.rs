//! Generic Tauri [`MeetingUi`] for gilb-based shells.
//!
//! [`ShellMeetingUi`] does the work that's identical across shells — open the
//! countdown popup windows and emit the `meeting-recording` webview event — then
//! delegates the parts that diverge (what a finished recording feeds into, any
//! extra status mirroring) to the app's [`ShellHooks`].

use gilb_pipeline::{MeetingUi, RecordingStatus};
use tauri::{AppHandle, Emitter};
use tracing::warn;

use crate::countdown::{open_countdown_window, open_stop_countdown_window};

/// Webview event carrying the in-app "recording a meeting now" indicator state.
const MEETING_RECORDING_EVENT: &str = "meeting-recording";

/// Per-app divergence in the meeting flow. The generic [`ShellMeetingUi`] does
/// the shared work (countdown windows, the `meeting-recording` webview event)
/// then calls these so each shell can add its own behaviour — e.g. enqueue
/// transcription vs. upload on stop, or mirror status into a tray.
///
/// Both hooks default to no-ops; a shell overrides only what it needs.
pub trait ShellHooks: Send + Sync + 'static {
    /// The recording indicator changed. Already emitted to the webview before
    /// this runs; override to mirror it elsewhere (e.g. a tray icon).
    fn on_recording_status(&self, _app: &AppHandle, _status: &RecordingStatus) {}
    /// A meeting recording was stopped and its files are final — e.g. enqueue
    /// transcription or upload.
    fn on_recording_stopped(&self, _app: &AppHandle, _meeting_id: i64) {}
}

/// [`MeetingUi`] backed by the Tauri shell: countdowns are popup windows, the
/// indicator is a webview event, and the divergent bits go through `hooks`.
pub struct ShellMeetingUi<H: ShellHooks> {
    pub(crate) app: AppHandle,
    pub(crate) hooks: H,
}

impl<H: ShellHooks> MeetingUi for ShellMeetingUi<H> {
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
        self.hooks.on_recording_status(&self.app, status);
    }

    fn recording_stopped(&self, meeting_id: i64) {
        self.hooks.on_recording_stopped(&self.app, meeting_id);
    }
}
