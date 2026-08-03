//! Manual recording toggle — driven by the tray menu ("Начать запись" /
//! "Остановить запись") and graceful quit. This is gilb's first hand-started
//! recording: until now the flow was auto-only (detect → countdown → record).
//!
//! Start: insert a `meetings` row tagged `manual` and publish
//! `RecordingEvent::Armed` — the recorder picks it off the bus exactly as if a
//! countdown had completed. Stop: route a "stop now" resolution to the pipeline
//! over the same channel the stop-countdown popup uses.

use gilb_db::meetings::insert_meeting;
use gilb_events::RecordingEvent;
use gilb_shell_tauri::{StopCountdownTx, StopResolution};
use tauri::{AppHandle, Manager};
use tracing::{info, warn};

use crate::state::AppState;

/// Bundle id recorded for recordings started by hand (no detected app).
const MANUAL_BUNDLE_ID: &str = "manual";

/// Start/stop a recording from anywhere, without finding the tray first —
/// which is the point: the moment worth recording is usually one where the
/// user is looking at the other app, not at us.
///
/// `Cmd/Ctrl+Shift+R` rather than the tidier `Cmd+R` or `Cmd+M`: a global
/// shortcut is taken from *every* app on the machine, so it must not be a
/// combination something else needs. `Cmd+R` reloads in every browser and
/// editor; `Cmd+M` minimises windows on macOS. Adding Shift costs nothing and
/// steals nothing anyone will miss.
pub const RECORD_SHORTCUT: &str = "CmdOrCtrl+Shift+R";

/// Bind [`RECORD_SHORTCUT`] to [`toggle`]. Best-effort: if another app already
/// owns the combination the tray still works, and the shell logs why.
pub fn register_shortcut(app: &AppHandle) {
    gilb_shell_tauri::shortcut::register(app, RECORD_SHORTCUT, toggle);
}

/// An in-flight arm older than this is considered lost (e.g. the pipeline
/// ignored it because another capture was already active) and stops blocking
/// the toggle.
const ARMING_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn toggle(app: &AppHandle) {
    let state = app.state::<AppState>();
    let active = state.recording.lock().clone();
    match active {
        Some((meeting_id, _)) => stop(app, meeting_id),
        None => {
            // Guard the bus round-trip: between publishing Armed and the
            // pipeline's status callback, state.recording is still None — a
            // quick second toggle must not insert a second meeting.
            let mut arming = state.arming_since.lock();
            if arming.is_some_and(|t| t.elapsed() < ARMING_GRACE) {
                tracing::debug!("manual arm already in flight; ignoring toggle");
                return;
            }
            *arming = Some(std::time::Instant::now());
            drop(arming);
            start(app);
        }
    }
}

fn start(app: &AppHandle) {
    let state = app.state::<AppState>();
    let db = state.engine.db().clone();
    let bus = state.engine.event_bus().clone();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match insert_meeting(&db, now_ms(), MANUAL_BUNDLE_ID).await {
            Ok(meeting_id) => {
                info!(meeting_id, "manual recording armed");
                bus.publish_recording(RecordingEvent::Armed { meeting_id });
            }
            Err(err) => {
                warn!(?err, "failed to insert manual meeting row");
                // Nothing was armed — release the toggle guard right away.
                *app.state::<AppState>().arming_since.lock() = None;
            }
        }
    });
}

/// Quit gracefully: stop an active recording first so its files are finalized
/// and handed to transcription, then exit. Used by the tray's "Завершить".
pub fn stop_then_quit(app: &AppHandle) {
    let state = app.state::<AppState>();
    let active = state.recording.lock().clone();
    let Some((meeting_id, _)) = active else {
        app.exit(0);
        return;
    };
    info!(meeting_id, "stopping active recording before quit");
    stop(app, meeting_id);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // Wait for the pipeline to finalize the capture (recorder.stop is
        // awaited inside the bridge), with a hard ceiling so quit can't hang.
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if app.state::<AppState>().recording.lock().is_none() {
                break;
            }
        }
        app.exit(0);
    });
}

fn stop(app: &AppHandle, meeting_id: i64) {
    let Some(stop_tx) = app.try_state::<StopCountdownTx>() else {
        return;
    };
    let tx = stop_tx.0.clone();
    tauri::async_runtime::spawn(async move {
        info!(meeting_id, "manual recording stop requested");
        if tx
            .send(StopResolution {
                meeting_id,
                keep: false,
            })
            .await
            .is_err()
        {
            warn!("meeting pipeline is gone; stop dropped");
        }
    });
}
