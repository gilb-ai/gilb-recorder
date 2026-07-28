//! Reusable Tauri glue for the gilb meeting flow.
//!
//! The app-agnostic half of the meeting flow lives in [`gilb_pipeline`]; this
//! crate supplies the *Tauri* half — the countdown popup windows, the
//! `meeting-recording` webview event, and the settings/privacy commands — so any
//! shell built on the gilb crates (the gilb app, a differently-branded recorder)
//! reuses it instead of copying these modules.
//!
//! A shell:
//! 1. manages a [`ShellConfig`] (the window title for the countdown popups),
//! 2. calls [`spawn_meeting_pipeline`] from `setup`, passing its [`ShellHooks`],
//! 3. registers the re-exported `#[tauri::command]`s in its own
//!    `generate_handler!`.
//!
//! The two `MeetingUi` methods that diverge between shells — what a finished
//! recording feeds into and any extra status mirroring — are the [`ShellHooks`];
//! everything else is shared.

mod countdown;
mod meeting;
mod privacy;
mod settings;
pub mod tray;

use std::path::PathBuf;

use gilb_db::Db;
use gilb_events::EventBus;
use gilb_pipeline::meeting_pipeline;
use tauri::{AppHandle, Manager};

pub use countdown::{
    resolve_countdown, resolve_stop_countdown, show_countdown, stop_meeting_recording,
    StopCountdownTx,
};
pub use gilb_pipeline::{RecordingStatus, StopResolution};

/// Managed Tauri state: the pipeline's live audio tap (see
/// `gilb_record::AudioTap`). Shells with a realtime consumer (assist) fetch it
/// via `app.state::<AudioTapHandle>()` and subscribe; everyone else ignores it.
pub struct AudioTapHandle(pub std::sync::Arc<gilb_record::AudioTap>);
pub use meeting::{ShellHooks, ShellMeetingUi};
pub use privacy::open_privacy_pane;
pub use settings::{get_meeting_detection, set_meeting_detection, MeetingControlTx};

/// Per-shell configuration, managed as Tauri state. Currently just the title
/// applied to the countdown popup windows, so differently-branded builds reuse
/// the windows without forking.
pub struct ShellConfig {
    pub window_title: String,
}

/// Start the meeting pipeline with the Tauri-backed UI: manage the control
/// handles + the recording bus (so the countdown commands can publish), and
/// spawn the bridge on Tauri's async runtime. Returns immediately; the bridge
/// runs on a detached task for the life of the app.
///
/// Safe to call from the synchronous `setup` hook — the recorder and detector
/// are spawned only when the bridge future first runs inside Tauri's runtime.
pub fn spawn_meeting_pipeline<H: ShellHooks>(
    app: AppHandle,
    hooks: H,
    bus: EventBus,
    db: Db,
    data_dir: PathBuf,
) {
    let enabled = gilb_config::load_preferences().meeting_detection_enabled;
    // Manage a bus clone so `resolve_countdown` can publish arm/cancel; it shares
    // the broadcast channel the recorder subscribes to.
    app.manage(bus.clone());
    let (handles, bridge) = meeting_pipeline(
        ShellMeetingUi {
            app: app.clone(),
            hooks,
        },
        bus,
        db,
        data_dir,
        enabled,
    );
    app.manage(StopCountdownTx(handles.stop_tx));
    app.manage(MeetingControlTx(handles.detection_ctl_tx));
    app.manage(AudioTapHandle(handles.audio_tap));
    tauri::async_runtime::spawn(bridge);
}
