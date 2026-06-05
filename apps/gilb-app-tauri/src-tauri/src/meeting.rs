//! Meeting flow bridge — connects the [`MeetingDetector`] to the recorder and
//! the countdown UI.
//!
//! [`spawn_meeting_pipeline`] is the app's runtime host wiring: at startup it
//! spawns the recorder (which self-wires to `RecordingEvent::Armed`/`Cancelled`
//! on the bus) and the platform meeting detector, then runs a `select!` loop
//! over three inputs — detector events, the recording bus, and stop-countdown
//! resolutions:
//!
//! - `MeetingEvent::Started` → insert a `meetings` row (so the id exists before
//!   the UI), then open the start-countdown window for that id.
//! - `MeetingEvent::Ended` → open the *stop*-countdown window for the active
//!   recording; the user can stop now or keep recording. It no longer stops the
//!   recorder directly — that happens once the popup resolves (or auto-stops).
//! - `RecordingEvent::Armed`/`Cancelled` (bus) → flip the in-app
//!   `meeting-recording` indicator the main window renders.
//! - `AppsChanged` / `HealthDegraded` → logged only.
//!
//! The detector is the live macOS unified-log detector on macOS; elsewhere a
//! [`MockDetector`] stands in (it never fires on its own), so the app builds and
//! runs on every platform while the real flow is macOS-only. The event→action
//! mapping ([`plan_action`]) and app selection ([`pick_app`]) are pure so they
//! unit-test without a detector, recorder, or windows.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::transcribe_worker::{TranscribeTx, TranscriptionJob};
use gilb_db::{meetings::insert_meeting, Db};
use gilb_events::{EventBus, RecordingEvent};
use gilb_meeting::{MeetingApp, MeetingDetector, MeetingEvent};
use gilb_record::{spawn_recorder, RecordingOutcome};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

#[cfg(target_os = "macos")]
use gilb_meeting::MacosDetector;
#[cfg(not(target_os = "macos"))]
use gilb_meeting::MockDetector;

use crate::commands::countdown::{open_countdown_window, open_stop_countdown_window};

/// Webview event carrying the in-app "recording a meeting now" indicator state.
const MEETING_RECORDING_EVENT: &str = "meeting-recording";

/// Capacity of the stop-countdown resolution channel (one in-flight popup at a
/// time, with slack for rapid clicks).
const STOP_RESOLUTION_CAPACITY: usize = 8;

/// Hard ceiling on a single meeting recording, measured from the moment it
/// arms. "Keep recording" can postpone the stop but never past this — capture
/// auto-stops 3h in regardless of detector events, so a stuck or forgotten
/// recording can't keep capturing screen + mic (and growing on disk) forever.
const MAX_RECORDING: std::time::Duration = std::time::Duration::from_secs(3 * 60 * 60);

/// Indicator payload emitted to the main window. `app`/`started_at_ms` are set
/// while a meeting recording is running and `None` once it stops; the webview
/// renders the elapsed timer from `started_at_ms`.
#[derive(Clone, Serialize)]
struct MeetingRecordingStatus {
    recording: bool,
    /// The recording meeting's id while active — the main window's Stop button
    /// passes it back to `stop_meeting_recording`. `None` once stopped.
    meeting_id: Option<i64>,
    app: Option<String>,
    started_at_ms: Option<i64>,
}

/// A stop-countdown popup outcome, routed from the `resolve_stop_countdown`
/// command back to the bridge loop. `keep == true` means "keep recording" —
/// the meeting end was premature, leave capture running until the next end.
#[derive(Debug)]
pub struct StopResolution {
    pub meeting_id: i64,
    pub keep: bool,
}

/// Managed Tauri state holding the sender half of the bridge's stop-countdown
/// resolution channel, so the `resolve_stop_countdown` command can hand the
/// user's choice back to the bridge task.
pub struct StopCountdownTx(pub mpsc::Sender<StopResolution>);

/// Managed Tauri state for toggling meeting detection (subsystem B) at runtime.
/// `set_meeting_detection` sends `true`/`false` here; the detector supervisor
/// starts or stops the platform detector accordingly — no restart needed.
pub struct MeetingControlTx(pub mpsc::Sender<bool>);

/// Buffer between the detector supervisor and the bridge loop.
const DETECTOR_FORWARD_CAPACITY: usize = 64;

/// Owns the platform meeting detector and starts/stops it on demand, forwarding
/// its events to the bridge over `det_tx`. Decoupling the detector's lifecycle
/// from the bridge loop is what makes the "Enable meeting detection" toggle live:
/// the bridge always consumes `det_tx`; this task decides whether the detector
/// is running. `ctl_rx` carries enable(true)/disable(false); the channel closing
/// (app exit) stops the detector and ends the task.
async fn run_detector_supervisor(
    detector: impl MeetingDetector + 'static,
    det_tx: mpsc::Sender<MeetingEvent>,
    mut ctl_rx: mpsc::Receiver<bool>,
    initially_enabled: bool,
) {
    let mut wanted = initially_enabled;
    loop {
        // Idle until detection is wanted (or the control channel closes at exit).
        while !wanted {
            match ctl_rx.recv().await {
                Some(w) => wanted = w,
                None => return,
            }
        }

        let mut events = match detector.start().await {
            Ok(rx) => rx,
            Err(err) => {
                error!(error = %err, "meeting detector failed to start");
                wanted = false;
                continue;
            }
        };
        info!("meeting detection started");

        // Forward events until detection is disabled or the stream ends.
        loop {
            tokio::select! {
                ev = events.recv() => match ev {
                    Some(e) => {
                        if det_tx.send(e).await.is_err() {
                            return; // bridge gone → nothing to forward to
                        }
                    }
                    None => {
                        warn!("meeting detector stream ended");
                        wanted = false;
                        break;
                    }
                },
                ctl = ctl_rx.recv() => match ctl {
                    Some(true) => {} // already running
                    Some(false) => {
                        let _ = detector.stop().await;
                        info!("meeting detection stopped");
                        wanted = false;
                        break;
                    }
                    None => {
                        let _ = detector.stop().await;
                        return;
                    }
                },
            }
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn emit_recording_status(app: &AppHandle, status: &MeetingRecordingStatus) {
    if let Err(err) = app.emit(MEETING_RECORDING_EVENT, status) {
        warn!(?err, "failed to emit meeting-recording status");
    }
}

/// What the bridge should do in response to a detector event. Kept separate
/// from the IO so the mapping is a pure, unit-testable function.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MeetingAction {
    /// Insert a `meetings` row at `at_ms` for `bundle_id`, then open the
    /// countdown window titled with `display_name`.
    Start {
        at_ms: i64,
        bundle_id: String,
        display_name: String,
    },
    /// Stop the active recording as completed.
    Stop,
    /// Nothing to drive — log and move on.
    Ignore,
}

/// Choose which meeting app to record when several share the mic. The detector
/// already allowlists and de-dups; we just take the first.
fn pick_app(apps: &[MeetingApp]) -> Option<&MeetingApp> {
    apps.first()
}

/// Pure mapping from a detector event to the bridge's [`MeetingAction`].
fn plan_action(event: &MeetingEvent) -> MeetingAction {
    match event {
        MeetingEvent::Started { at, apps } => match pick_app(apps) {
            Some(app) => MeetingAction::Start {
                at_ms: at.timestamp_millis(),
                bundle_id: app.bundle_id.clone(),
                display_name: app.display_name.clone(),
            },
            None => MeetingAction::Ignore,
        },
        MeetingEvent::Ended { .. } => MeetingAction::Stop,
        MeetingEvent::AppsChanged { .. } | MeetingEvent::HealthDegraded { .. } => {
            MeetingAction::Ignore
        }
    }
}

/// Mutable meeting state the bridge loop threads through its handlers. Grouping
/// it keeps the handlers' signatures small and the `select!` arms readable.
#[derive(Default)]
struct BridgeState {
    /// `meeting_id -> display name`, learned on `Started` and consumed by the
    /// indicator when capture actually arms.
    app_names: HashMap<i64, String>,
    /// The meeting currently being recorded (armed and not yet stopped), with
    /// its display name. Drives the indicator and the stop-countdown target;
    /// `MeetingEvent::Ended` carries no id, so this is how we know what to stop.
    recording: Option<(i64, String)>,
    /// Absolute deadline at which the active recording is force-stopped (set
    /// when it arms; see `MAX_RECORDING`). `None` when nothing is recording.
    cap_deadline: Option<Instant>,
}

/// Start the meeting pipeline: spawn the recorder + detector and bridge detector
/// events onto the countdown UI and the recorder. Returns immediately; the
/// bridge runs on a detached task for the life of the app.
pub fn spawn_meeting_pipeline(app: AppHandle, bus: EventBus, db: Db, data_dir: PathBuf) {
    // Subscribe to the recording channel *before* `bus` is moved into the
    // recorder: the bridge watches `Armed`/`Cancelled` to drive the in-app
    // indicator, independently of the recorder's own subscription.
    let mut rec_rx = bus.subscribe_recording();
    let (stop_tx, mut stop_rx) = mpsc::channel::<StopResolution>(STOP_RESOLUTION_CAPACITY);
    app.manage(StopCountdownTx(stop_tx));
    // Control channel for the live meeting-detection toggle (subsystem B).
    let (ctl_tx, ctl_rx) = mpsc::channel::<bool>(8);
    app.manage(MeetingControlTx(ctl_tx));

    tauri::async_runtime::spawn(async move {
        // Spawn the recorder *inside* Tauri's async runtime: `spawn_recorder`
        // calls `tokio::spawn`, which panics ("no reactor running") if invoked
        // straight from the synchronous Tauri `setup` hook, where no runtime is
        // entered.
        let recorder = spawn_recorder(bus, db.clone(), data_dir);

        #[cfg(target_os = "macos")]
        let detector = MacosDetector::new();
        #[cfg(not(target_os = "macos"))]
        let detector = MockDetector::new();

        // The detector lives in its own supervisor so detection can be toggled at
        // runtime; the bridge just consumes the forwarded events. Whether it
        // starts now depends on the persisted master switch.
        let (det_tx, mut rx) = mpsc::channel::<MeetingEvent>(DETECTOR_FORWARD_CAPACITY);
        let enabled = gilb_config::load_preferences().meeting_detection_enabled;
        tokio::spawn(run_detector_supervisor(detector, det_tx, ctl_rx, enabled));

        // All the meeting state the loop threads through its handlers.
        let mut state = BridgeState::default();
        // Once the bus closes (process exit) stop polling it to avoid a busy loop.
        let mut bus_open = true;

        loop {
            // Copied out so the timer future doesn't borrow `state` (the select!
            // arms need `&mut state`). Recreated each iteration so the current
            // deadline is honoured; pends forever while nothing is recording.
            let deadline = state.cap_deadline;
            let cap = async move {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending::<()>().await,
                }
            };

            tokio::select! {
                // Detector events: meeting start/end transitions.
                maybe_event = rx.recv() => {
                    let Some(event) = maybe_event else { break };
                    match plan_action(&event) {
                        MeetingAction::Start { at_ms, bundle_id, display_name } => {
                            match insert_meeting(&db, at_ms, &bundle_id).await {
                                Ok(id) => {
                                    state.app_names.insert(id, display_name.clone());
                                    if let Err(err) = open_countdown_window(&app, &display_name, id) {
                                        warn!(meeting_id = id, error = %err, "failed to open countdown window");
                                    }
                                }
                                Err(err) => warn!(error = %err, "failed to insert meeting row"),
                            }
                        }
                        MeetingAction::Stop => {
                            // Don't stop yet — let the user confirm or keep recording
                            // through the stop-countdown popup. If it won't open, fall
                            // back to stopping immediately so a capture never gets stuck.
                            if let Some((id, name)) = state.recording.clone() {
                                if let Err(err) = open_stop_countdown_window(&app, &name, id) {
                                    warn!(meeting_id = id, error = %err, "failed to open stop-countdown; stopping now");
                                    stop_recording(&app, &recorder,id, &mut state).await;
                                }
                            }
                        }
                        MeetingAction::Ignore => debug!(?event, "meeting event ignored"),
                    }
                }

                // Recording bus: arm/cancel drive the in-app indicator.
                rec = rec_rx.recv(), if bus_open => match rec {
                    Ok(msg) => match msg.payload {
                        RecordingEvent::Armed { meeting_id } => {
                            let name = state
                                .app_names
                                .get(&meeting_id)
                                .cloned()
                                .unwrap_or_else(|| "this meeting".to_string());
                            state.recording = Some((meeting_id, name.clone()));
                            state.cap_deadline = Some(Instant::now() + MAX_RECORDING);
                            emit_recording_status(&app, &MeetingRecordingStatus {
                                recording: true,
                                meeting_id: Some(meeting_id),
                                app: Some(name),
                                started_at_ms: Some(now_ms()),
                            });
                        }
                        RecordingEvent::Cancelled { meeting_id } => {
                            // Drop the name map entry whether or not this meeting
                            // ever armed (cancel-before-arm would otherwise leak it).
                            state.app_names.remove(&meeting_id);
                            if state.recording.as_ref().is_some_and(|(id, _)| *id == meeting_id) {
                                state.recording = None;
                                state.cap_deadline = None;
                                emit_recording_status(&app, &MeetingRecordingStatus {
                                    recording: false,
                                    meeting_id: None,
                                    app: None,
                                    started_at_ms: None,
                                });
                            }
                        }
                    },
                    Err(RecvError::Lagged(skipped)) => {
                        warn!(skipped, "meeting bridge lagged behind the recording bus");
                    }
                    Err(RecvError::Closed) => bus_open = false,
                },

                // Stop-countdown popup resolution.
                Some(res) = stop_rx.recv() => {
                    if res.keep {
                        // Postpone the stop, but keep the absolute cap (`cap_deadline`
                        // is untouched) so "keep recording" can't run past MAX_RECORDING.
                        debug!(meeting_id = res.meeting_id, "keep recording (premature meeting end)");
                    } else {
                        stop_recording(&app, &recorder,res.meeting_id, &mut state).await;
                    }
                }

                // Safety cap: force-stop a recording that has run for MAX_RECORDING.
                _ = cap, if deadline.is_some() => {
                    if let Some((id, _)) = state.recording.clone() {
                        warn!(meeting_id = id, hours = MAX_RECORDING.as_secs() / 3600, "meeting recording hit the duration cap; stopping");
                        stop_recording(&app, &recorder,id, &mut state).await;
                    } else {
                        state.cap_deadline = None;
                    }
                }
            }
        }
        // The detector is owned by `run_detector_supervisor`; it stops when the
        // control channel closes at app exit.
    });
}

/// Stop the active meeting capture, kick off transcription, and clear the
/// in-app indicator. Shared by the stop-countdown "Stop now"/auto path and the
/// fallback when the popup can't be shown.
async fn stop_recording(
    app: &AppHandle,
    recorder: &gilb_record::Recorder<gilb_record::PlatformCapturer>,
    meeting_id: i64,
    state: &mut BridgeState,
) {
    state.cap_deadline = None;
    if let Err(err) = recorder.stop(RecordingOutcome::Completed).await {
        warn!(meeting_id, error = %err, "failed to stop recorder");
    }
    enqueue_transcription(app, meeting_id);
    state.app_names.remove(&meeting_id);
    if state
        .recording
        .as_ref()
        .is_some_and(|(id, _)| *id == meeting_id)
    {
        state.recording = None;
    }
    emit_recording_status(
        app,
        &MeetingRecordingStatus {
            recording: false,
            meeting_id: None,
            app: None,
            started_at_ms: None,
        },
    );
}

/// Enqueue a finished meeting for on-device transcription. The background worker
/// (`transcribe_worker`) owns the model + queue and does the rest; gating on a
/// downloaded model, audio presence, and persistence all happen there. A no-op
/// if the worker isn't registered. Non-blocking — never stalls the bridge.
fn enqueue_transcription(app: &AppHandle, meeting_id: i64) {
    match app.try_state::<TranscribeTx>() {
        Some(tx) => {
            let _ = tx.0.send(TranscriptionJob::Meeting(meeting_id));
        }
        None => warn!(meeting_id, "transcription worker not available; skipping"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use std::time::Duration;

    fn app(bundle_id: &str, display_name: &str) -> MeetingApp {
        MeetingApp {
            bundle_id: bundle_id.to_string(),
            display_name: display_name.to_string(),
        }
    }

    #[test]
    fn pick_app_takes_first() {
        let apps = vec![
            app("us.zoom.xos", "Zoom"),
            app("com.tinyspeck.slackmacgap", "Slack"),
        ];
        assert_eq!(pick_app(&apps).unwrap().bundle_id, "us.zoom.xos");
    }

    #[test]
    fn pick_app_none_when_empty() {
        assert!(pick_app(&[]).is_none());
    }

    #[test]
    fn started_maps_to_start_with_first_app() {
        let at = Utc.timestamp_opt(7, 0).unwrap();
        let event = MeetingEvent::Started {
            at,
            apps: vec![app("us.zoom.xos", "Zoom")],
        };
        assert_eq!(
            plan_action(&event),
            MeetingAction::Start {
                at_ms: 7_000,
                bundle_id: "us.zoom.xos".to_string(),
                display_name: "Zoom".to_string(),
            }
        );
    }

    #[test]
    fn started_with_no_apps_is_ignored() {
        let event = MeetingEvent::Started {
            at: Utc.timestamp_opt(0, 0).unwrap(),
            apps: vec![],
        };
        assert_eq!(plan_action(&event), MeetingAction::Ignore);
    }

    #[test]
    fn ended_maps_to_stop() {
        let event = MeetingEvent::Ended {
            at: Utc.timestamp_opt(1, 0).unwrap(),
            duration: Duration::from_secs(60),
        };
        assert_eq!(plan_action(&event), MeetingAction::Stop);
    }

    #[test]
    fn apps_changed_and_health_are_ignored() {
        let changed = MeetingEvent::AppsChanged {
            at: Utc.timestamp_opt(2, 0).unwrap(),
            apps: vec![app("us.zoom.xos", "Zoom")],
        };
        let health = MeetingEvent::HealthDegraded {
            reason: "log stream ended".to_string(),
        };
        assert_eq!(plan_action(&changed), MeetingAction::Ignore);
        assert_eq!(plan_action(&health), MeetingAction::Ignore);
    }
}
