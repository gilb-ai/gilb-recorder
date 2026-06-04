//! Unit tests for the pure parts of the Windows meeting detector: the
//! process-name -> bundle-id map and the [`SessionTracker`] count state
//! machine. These run on every platform (no COM); the live WASAPI path
//! is smoke-tested by hand on a Windows host.

use chrono::{TimeZone, Utc};
use gilb_meeting::allowlist;
use gilb_meeting::{MeetingApp, MeetingEvent, SessionEvent, SessionTracker};

fn app(bundle_id: &str, display_name: &str) -> MeetingApp {
    MeetingApp {
        bundle_id: bundle_id.to_string(),
        display_name: display_name.to_string(),
    }
}

#[test]
fn process_map_resolves_known_exes() {
    assert_eq!(
        allowlist::bundle_id_from_process_name("zoom.exe"),
        Some("us.zoom.xos")
    );
    assert_eq!(
        allowlist::bundle_id_from_process_name("ms-teams.exe"),
        Some("com.microsoft.teams2")
    );
    // Exe name with a space (telephony app).
    assert_eq!(
        allowlist::bundle_id_from_process_name("Aircall Workspace.exe"),
        Some("io.aircall.phone")
    );
}

#[test]
fn process_map_is_case_insensitive_and_strips_path() {
    // Case-insensitive match.
    assert_eq!(
        allowlist::bundle_id_from_process_name("ZOOM.EXE"),
        Some("us.zoom.xos")
    );
    // Full Win32 path: only the basename is matched.
    assert_eq!(
        allowlist::bundle_id_from_process_name(r"C:\Program Files\Zoom\bin\Zoom.exe"),
        Some("us.zoom.xos")
    );
    // Forward slashes too, for good measure.
    assert_eq!(
        allowlist::bundle_id_from_process_name("C:/Users/x/AppData/Slack/slack.exe"),
        Some("com.tinyspeck.slackmacgap")
    );
}

#[test]
fn process_map_returns_none_for_unknown() {
    assert_eq!(allowlist::bundle_id_from_process_name("notepad.exe"), None);
    assert_eq!(allowlist::bundle_id_from_process_name("chrome.exe"), None);
    assert_eq!(allowlist::bundle_id_from_process_name(""), None);
}

#[test]
fn tracker_emits_started_appschanged_then_ended() {
    let mut tracker = SessionTracker::new();
    let t0 = Utc.timestamp_opt(1_000, 0).unwrap();
    let t1 = Utc.timestamp_opt(1_010, 0).unwrap();
    let t2 = Utc.timestamp_opt(1_020, 0).unwrap();
    let t3 = Utc.timestamp_opt(1_100, 0).unwrap();

    // A session opening does not start a meeting on its own.
    assert_eq!(tracker.observe(SessionEvent::New, "zoom.exe", t0), None);

    // First app goes active -> Started.
    match tracker.observe(SessionEvent::Active, "zoom.exe", t0) {
        Some(MeetingEvent::Started { at, apps }) => {
            assert_eq!(at, t0);
            assert_eq!(apps, vec![app("us.zoom.xos", "Zoom")]);
        }
        other => panic!("expected Started, got {other:?}"),
    }

    // A second app goes active -> AppsChanged with both, sorted by id.
    match tracker.observe(SessionEvent::Active, "ms-teams.exe", t1) {
        Some(MeetingEvent::AppsChanged { at, apps }) => {
            assert_eq!(at, t1);
            assert_eq!(
                apps,
                vec![
                    app("com.microsoft.teams2", "Microsoft Teams"),
                    app("us.zoom.xos", "Zoom"),
                ]
            );
        }
        other => panic!("expected AppsChanged, got {other:?}"),
    }

    // One of the two drops -> AppsChanged back to a single app.
    match tracker.observe(SessionEvent::Inactive, "zoom.exe", t2) {
        Some(MeetingEvent::AppsChanged { at, apps }) => {
            assert_eq!(at, t2);
            assert_eq!(apps, vec![app("com.microsoft.teams2", "Microsoft Teams")]);
        }
        other => panic!("expected AppsChanged, got {other:?}"),
    }

    // Last app expires -> Ended, duration from the first Started.
    match tracker.observe(SessionEvent::Expired, "ms-teams.exe", t3) {
        Some(MeetingEvent::Ended { at, duration }) => {
            assert_eq!(at, t3);
            assert_eq!(duration, std::time::Duration::from_secs(100));
        }
        other => panic!("expected Ended, got {other:?}"),
    }
}

#[test]
fn tracker_ignores_unknown_processes_and_noop_events() {
    let mut tracker = SessionTracker::new();
    let now = Utc.timestamp_opt(2_000, 0).unwrap();

    // Unknown process: ignored regardless of event kind.
    assert_eq!(
        tracker.observe(SessionEvent::Active, "chrome.exe", now),
        None
    );
    assert_eq!(
        tracker.observe(SessionEvent::Inactive, "notepad.exe", now),
        None
    );

    // New / DefaultDeviceChanged never move the active set, so the first
    // real Active still reads as the meeting's start.
    assert_eq!(tracker.observe(SessionEvent::New, "zoom.exe", now), None);
    assert_eq!(
        tracker.observe(SessionEvent::DefaultDeviceChanged, "zoom.exe", now),
        None
    );
    assert!(matches!(
        tracker.observe(SessionEvent::Active, "zoom.exe", now),
        Some(MeetingEvent::Started { .. })
    ));
}

#[test]
fn tracker_dedups_repeated_active_for_same_app() {
    let mut tracker = SessionTracker::new();
    let now = Utc.timestamp_opt(3_000, 0).unwrap();

    assert!(matches!(
        tracker.observe(SessionEvent::Active, "zoom.exe", now),
        Some(MeetingEvent::Started { .. })
    ));
    // Same app active again (a redundant transition) -> no event.
    assert_eq!(tracker.observe(SessionEvent::Active, "zoom.exe", now), None);
    // Inactive for a different unknown app -> no event, meeting holds.
    assert_eq!(
        tracker.observe(SessionEvent::Inactive, "discord.exe", now),
        None
    );
    // The real app goes inactive -> Ended.
    assert!(matches!(
        tracker.observe(SessionEvent::Inactive, "zoom.exe", now),
        Some(MeetingEvent::Ended { .. })
    ));
}
