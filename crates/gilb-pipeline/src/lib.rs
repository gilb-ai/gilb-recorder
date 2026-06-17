//! Meeting capture pipeline — bridges the [`MeetingDetector`] to the recorder
//! and a pluggable UI.
//!
//! This crate hosts the app-agnostic half of the meeting flow so that any
//! shell built on the gilb crates (the gilb Tauri app, differently-branded
//! recorders, a future CLI) can reuse it. The shell supplies a [`MeetingUi`]
//! implementation — how countdowns are shown and how the "recording now"
//! indicator is rendered is up to it (popup windows, tray icon + system
//! notifications, nothing at all).
//!
//! [`meeting_pipeline`] is the runtime host wiring: it returns the control
//! handles plus a future the shell spawns on its async runtime. When polled,
//! the future spawns the recorder (which self-wires to
//! `RecordingEvent::Armed`/`Cancelled` on the bus) and the platform meeting
//! detector, then runs a `select!` loop over three inputs — detector events,
//! the recording bus, and stop-countdown resolutions:
//!
//! - `MeetingEvent::Started` → insert a `meetings` row (so the id exists before
//!   the UI), then ask the UI to open the start countdown for that id.
//! - `MeetingEvent::Ended` → ask the UI to open the *stop* countdown for the
//!   active recording; the user can stop now or keep recording. The recorder is
//!   stopped once the countdown resolves (or as a fallback when the UI fails).
//! - `RecordingEvent::Armed`/`Cancelled` (bus) → forward the recording
//!   indicator state to the UI.
//! - `AppsChanged` / `HealthDegraded` → logged only.
//!
//! The detector is the live macOS unified-log detector on macOS and the WASAPI
//! audio-session detector on Windows; on any other platform a [`MockDetector`]
//! stands in (it never fires on its own) so shells still build. The
//! event→action mapping ([`plan_action`]) and app selection ([`pick_app`]) are
//! pure so they unit-test without a detector, recorder, or UI.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use gilb_db::{meetings::insert_meeting, Db};
use gilb_events::{EventBus, RecordingEvent};
use gilb_meeting::{MeetingApp, MeetingDetector, MeetingEvent};
use gilb_record::{spawn_recorder, RecordingOutcome};
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

#[cfg(target_os = "macos")]
use gilb_meeting::MacosDetector;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use gilb_meeting::MockDetector;
#[cfg(target_os = "windows")]
use gilb_meeting::WindowsDetector;

/// Capacity of the stop-countdown resolution channel (one in-flight popup at a
/// time, with slack for rapid clicks).
const STOP_RESOLUTION_CAPACITY: usize = 8;

/// Capacity of the detection enable/disable control channel.
const DETECTION_CTL_CAPACITY: usize = 8;

/// Buffer between the detector supervisor and the bridge loop.
const DETECTOR_FORWARD_CAPACITY: usize = 64;

/// Hard ceiling on a single meeting recording, measured from the moment it
/// arms. "Keep recording" can postpone the stop but never past this — capture
/// auto-stops 3h in regardless of detector events, so a stuck or forgotten
/// recording can't keep capturing screen + mic (and growing on disk) forever.
const MAX_RECORDING: std::time::Duration = std::time::Duration::from_secs(3 * 60 * 60);

/// The "recording a meeting now" indicator state handed to [`MeetingUi`].
/// `app`/`started_at_ms` are set while a meeting recording is running and
/// `None` once it stops; a UI can render an elapsed timer from
/// `started_at_ms`. `Serialize` so a webview shell can emit it as-is.
#[derive(Clone, Serialize)]
pub struct RecordingStatus {
    pub recording: bool,
    /// The recording meeting's id while active — a UI's Stop control passes it
    /// back through its stop command. `None` once stopped.
    pub meeting_id: Option<i64>,
    pub app: Option<String>,
    pub started_at_ms: Option<i64>,
}

/// A stop-countdown outcome, routed from the shell back to the bridge loop via
/// [`PipelineHandles::stop_tx`]. `keep == true` means "keep recording" — the
/// meeting end was premature, leave capture running until the next end.
#[derive(Debug)]
pub struct StopResolution {
    pub meeting_id: i64,
    pub keep: bool,
}

/// How the pipeline talks to the shell. Implementations must be cheap and
/// non-blocking — every method is called from the bridge loop.
///
/// The start/stop countdown hooks only *show* the countdown; arming or
/// stopping the recorder is driven by the shell afterwards (publishing
/// `RecordingEvent::Armed` on the bus when the start countdown completes,
/// sending a [`StopResolution`] when the stop countdown resolves).
pub trait MeetingUi: Send + Sync + 'static {
    /// A meeting just started: show the pre-record countdown for `meeting_id`.
    fn open_start_countdown(&self, display_name: &str, meeting_id: i64) -> anyhow::Result<()>;
    /// The meeting seems to have ended: ask whether to stop or keep recording.
    fn open_stop_countdown(&self, display_name: &str, meeting_id: i64) -> anyhow::Result<()>;
    /// The recording indicator changed (armed, cancelled, or stopped).
    fn recording_status(&self, status: &RecordingStatus);
    /// A meeting recording was stopped and its files are final — e.g. enqueue
    /// transcription or upload.
    fn recording_stopped(&self, meeting_id: i64);
}

/// Control handles returned by [`meeting_pipeline`], to be kept by the shell:
/// stop-countdown resolutions flow into `stop_tx`, the live meeting-detection
/// toggle into `detection_ctl_tx` (`true` = enable). The pipeline winds down
/// when both senders are dropped and the detector stream ends.
pub struct PipelineHandles {
    pub stop_tx: mpsc::Sender<StopResolution>,
    pub detection_ctl_tx: mpsc::Sender<bool>,
}

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

/// What the bridge should do in response to a detector event. Kept separate
/// from the IO so the mapping is a pure, unit-testable function.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MeetingAction {
    /// Insert a `meetings` row at `at_ms` for `bundle_id`, then open the
    /// countdown titled with `display_name`.
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

/// Build the meeting pipeline: returns the shell's control handles and the
/// bridge future. The shell keeps the handles and spawns the future on its
/// async runtime (it must be polled inside a tokio runtime — the recorder and
/// detector supervisor are spawned when it first runs, not here, so this
/// function is safe to call from synchronous setup code).
pub fn meeting_pipeline(
    ui: impl MeetingUi,
    bus: EventBus,
    db: Db,
    data_dir: PathBuf,
    detection_initially_enabled: bool,
) -> (PipelineHandles, impl Future<Output = ()> + Send) {
    let (stop_tx, stop_rx) = mpsc::channel::<StopResolution>(STOP_RESOLUTION_CAPACITY);
    let (ctl_tx, ctl_rx) = mpsc::channel::<bool>(DETECTION_CTL_CAPACITY);
    let handles = PipelineHandles {
        stop_tx,
        detection_ctl_tx: ctl_tx,
    };
    let fut = run_bridge(
        ui,
        bus,
        db,
        data_dir,
        detection_initially_enabled,
        stop_rx,
        ctl_rx,
    );
    (handles, fut)
}

/// The bridge loop body — see the crate docs for the event map.
async fn run_bridge(
    ui: impl MeetingUi,
    bus: EventBus,
    db: Db,
    data_dir: PathBuf,
    detection_initially_enabled: bool,
    mut stop_rx: mpsc::Receiver<StopResolution>,
    ctl_rx: mpsc::Receiver<bool>,
) {
    // Subscribe to the recording channel *before* `bus` is moved into the
    // recorder: the bridge watches `Armed`/`Cancelled` to drive the indicator,
    // independently of the recorder's own subscription.
    let mut rec_rx = bus.subscribe_recording();
    let recorder = spawn_recorder(bus, db.clone(), data_dir);

    #[cfg(target_os = "macos")]
    let detector = MacosDetector::new();
    #[cfg(target_os = "windows")]
    let detector = WindowsDetector::new();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let detector = MockDetector::new();

    // The detector lives in its own supervisor so detection can be toggled at
    // runtime; the bridge just consumes the forwarded events.
    let (det_tx, mut rx) = mpsc::channel::<MeetingEvent>(DETECTOR_FORWARD_CAPACITY);
    tokio::spawn(run_detector_supervisor(
        detector,
        det_tx,
        ctl_rx,
        detection_initially_enabled,
    ));

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
                                if let Err(err) = ui.open_start_countdown(&display_name, id) {
                                    warn!(meeting_id = id, error = %err, "failed to open the start countdown");
                                }
                            }
                            Err(err) => warn!(error = %err, "failed to insert meeting row"),
                        }
                    }
                    MeetingAction::Stop => {
                        // Don't stop yet — let the user confirm or keep recording
                        // through the stop countdown. If it won't open, fall back
                        // to stopping immediately so a capture never gets stuck.
                        if let Some((id, name)) = state.recording.clone() {
                            if let Err(err) = ui.open_stop_countdown(&name, id) {
                                warn!(meeting_id = id, error = %err, "failed to open the stop countdown; stopping now");
                                stop_recording(&ui, &recorder, id, &mut state).await;
                            }
                        }
                    }
                    MeetingAction::Ignore => debug!(?event, "meeting event ignored"),
                }
            }

            // Recording bus: arm/cancel drive the indicator.
            rec = rec_rx.recv(), if bus_open => match rec {
                Ok(msg) => match msg.payload {
                    RecordingEvent::Armed { meeting_id } => {
                        // The recorder ignores an arm while another capture is
                        // active — mirror that here, or the bridge would track
                        // meeting B while the recorder keeps capturing A, and a
                        // later stop would finalize A but report B to the shell.
                        if let Some((active, _)) = &state.recording {
                            if *active != meeting_id {
                                warn!(
                                    meeting_id,
                                    active, "arm ignored: another meeting is recording"
                                );
                                continue;
                            }
                        }
                        // `None` when the app is unknown (e.g. a shell arming a
                        // manual recording) — the shell/frontend picks its own
                        // localized fallback label.
                        let name = state.app_names.get(&meeting_id).cloned();
                        state.recording = Some((
                            meeting_id,
                            name.clone().unwrap_or_else(|| "this meeting".to_string()),
                        ));
                        state.cap_deadline = Some(Instant::now() + MAX_RECORDING);
                        ui.recording_status(&RecordingStatus {
                            recording: true,
                            meeting_id: Some(meeting_id),
                            app: name,
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
                            ui.recording_status(&RecordingStatus {
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

            // Stop-countdown resolution.
            Some(res) = stop_rx.recv() => {
                if res.keep {
                    // Postpone the stop, but keep the absolute cap (`cap_deadline`
                    // is untouched) so "keep recording" can't run past MAX_RECORDING.
                    debug!(meeting_id = res.meeting_id, "keep recording (premature meeting end)");
                } else {
                    stop_recording(&ui, &recorder, res.meeting_id, &mut state).await;
                }
            }

            // Safety cap: force-stop a recording that has run for MAX_RECORDING.
            _ = cap, if deadline.is_some() => {
                if let Some((id, _)) = state.recording.clone() {
                    warn!(meeting_id = id, hours = MAX_RECORDING.as_secs() / 3600, "meeting recording hit the duration cap; stopping");
                    stop_recording(&ui, &recorder, id, &mut state).await;
                } else {
                    state.cap_deadline = None;
                }
            }
        }
    }
    // The detector is owned by `run_detector_supervisor`; it stops when the
    // control channel closes at app exit.
}

/// Stop the active meeting capture, notify the shell, and clear the indicator.
/// Shared by the stop-countdown "Stop now"/auto path and the fallback when the
/// countdown can't be shown.
async fn stop_recording(
    ui: &impl MeetingUi,
    recorder: &gilb_record::Recorder<gilb_record::PlatformCapturer>,
    meeting_id: i64,
    state: &mut BridgeState,
) {
    // A stale resolution (an old popup or a queued indicator stop) must not
    // kill a different meeting's capture: the recorder stops whatever is
    // active, so gate on the bridge's view of what that is.
    if state
        .recording
        .as_ref()
        .is_none_or(|(id, _)| *id != meeting_id)
    {
        warn!(
            meeting_id,
            "stop requested for a meeting that isn't recording; ignoring"
        );
        return;
    }
    state.cap_deadline = None;

    // Clear the indicator before stopping: the meeting is over the moment the
    // user leaves it, and `recorder.stop()` now also muxes the audio into the
    // video synchronously (a re-encode that can take a while). The mux is
    // post-processing, not recording, so leaving the indicator lit through it
    // would misrepresent a finished meeting as still recording. The capture
    // itself (screen + mic streams) is torn down at the top of `stop()` anyway.
    state.app_names.remove(&meeting_id);
    if state
        .recording
        .as_ref()
        .is_some_and(|(id, _)| *id == meeting_id)
    {
        state.recording = None;
    }
    ui.recording_status(&RecordingStatus {
        recording: false,
        meeting_id: None,
        app: None,
        started_at_ms: None,
    });

    // Finalize the recording (stops capture, then muxes audio into the video).
    // `recording_stopped` hands the file to the upload queue, so it must run
    // after the mux completes — the muxed video with sound is then what uploads.
    if let Err(err) = recorder.stop(RecordingOutcome::Completed).await {
        warn!(meeting_id, error = %err, "failed to stop recorder");
    }
    ui.recording_stopped(meeting_id);
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
