//! Meeting flow bridge — connects the [`MeetingDetector`] to the recorder and
//! the countdown UI.
//!
//! [`spawn_meeting_pipeline`] is the app's runtime host wiring: at startup it
//! spawns the recorder (which self-wires to `RecordingEvent::Armed`/`Cancelled`
//! on the bus) and the platform meeting detector, then bridges detector events
//! the recorder doesn't see itself:
//!
//! - `MeetingEvent::Started` → insert a `meetings` row (so the id exists before
//!   the UI), then open the countdown window for that id.
//! - `MeetingEvent::Ended` → `recorder.stop(Completed)` for the active meeting.
//! - `AppsChanged` / `HealthDegraded` → logged only.
//!
//! The detector is the live macOS unified-log detector on macOS; elsewhere a
//! [`MockDetector`] stands in (it never fires on its own), so the app builds and
//! runs on every platform while the real flow is macOS-only. The event→action
//! mapping ([`plan_action`]) and app selection ([`pick_app`]) are pure so they
//! unit-test without a detector, recorder, or windows.

use std::path::{Path, PathBuf};

use gilb_config::RecordingSettings;
use gilb_db::{
    meetings::{get_meeting, insert_meeting},
    Db,
};
use gilb_events::EventBus;
use gilb_meeting::{MeetingApp, MeetingDetector, MeetingEvent};
use gilb_record::{spawn_recorder, RecordingOutcome};
use gilb_transcribe::{transcribe_meeting, OpenAiTranscriber};
use tauri::AppHandle;
use tracing::{debug, error, warn};

#[cfg(target_os = "macos")]
use gilb_meeting::MacosDetector;
#[cfg(not(target_os = "macos"))]
use gilb_meeting::MockDetector;

use crate::commands::countdown::open_countdown_window;

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

/// Start the meeting pipeline: spawn the recorder + detector and bridge detector
/// events onto the countdown UI and the recorder. Returns immediately; the
/// bridge runs on a detached task for the life of the app.
pub fn spawn_meeting_pipeline(app: AppHandle, bus: EventBus, db: Db, data_dir: PathBuf) {
    let recorder = spawn_recorder(bus, db.clone(), data_dir);

    tauri::async_runtime::spawn(async move {
        #[cfg(target_os = "macos")]
        let detector = MacosDetector::new();
        #[cfg(not(target_os = "macos"))]
        let detector = MockDetector::new();

        let mut rx = match detector.start().await {
            Ok(rx) => rx,
            Err(err) => {
                error!(error = %err, "meeting detector failed to start");
                return;
            }
        };

        // `MeetingEvent::Ended` carries no id, so remember the id from the last
        // `Started` to drive the matching `recorder.stop`.
        let mut active: Option<i64> = None;

        while let Some(event) = rx.recv().await {
            match plan_action(&event) {
                MeetingAction::Start {
                    at_ms,
                    bundle_id,
                    display_name,
                } => match insert_meeting(&db, at_ms, &bundle_id).await {
                    Ok(id) => {
                        active = Some(id);
                        if let Err(err) = open_countdown_window(&app, &display_name, id) {
                            warn!(meeting_id = id, error = %err, "failed to open countdown window");
                        }
                    }
                    Err(err) => warn!(error = %err, "failed to insert meeting row"),
                },
                MeetingAction::Stop => {
                    if let Some(id) = active.take() {
                        if let Err(err) = recorder.stop(RecordingOutcome::Completed).await {
                            warn!(meeting_id = id, error = %err, "failed to stop recorder");
                        }
                        maybe_spawn_transcription(&db, id).await;
                    }
                }
                MeetingAction::Ignore => debug!(?event, "meeting event ignored"),
            }
        }

        // Keep the detector alive for the life of the loop; dropping it stops
        // the platform worker.
        drop(detector);
    });
}

/// After a meeting finalizes, kick off batch transcription on a detached task —
/// best-effort and only when a BYOK OpenAI key is configured
/// (`OPENAI_API_KEY`). A no-op when no key is set or the meeting has no audio
/// path. Never blocks the bridge or panics; the outcome (transcript or error)
/// is persisted to `meeting_transcripts` by [`transcribe_meeting`].
async fn maybe_spawn_transcription(db: &Db, meeting_id: i64) {
    let Some(api_key) = RecordingSettings::from_env().openai_api_key else {
        return;
    };
    let audio_path = match get_meeting(db, meeting_id).await {
        Ok(Some(m)) => m.audio_path,
        Ok(None) => None,
        Err(err) => {
            warn!(meeting_id, error = %err, "failed to load meeting for transcription");
            return;
        }
    };
    let Some(audio_path) = audio_path else {
        debug!(
            meeting_id,
            "meeting has no audio path; skipping transcription"
        );
        return;
    };

    let db = db.clone();
    tauri::async_runtime::spawn(async move {
        let transcriber = OpenAiTranscriber::new();
        if let Err(err) = transcribe_meeting(
            &db,
            &api_key,
            meeting_id,
            Path::new(&audio_path),
            &transcriber,
        )
        .await
        {
            warn!(meeting_id, error = %err, "failed to persist transcription outcome");
        }
    });
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
