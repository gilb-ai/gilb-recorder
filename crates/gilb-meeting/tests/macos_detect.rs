//! Unit tests for the pure parts of the macOS meeting detector: the
//! attribution-line parser, the allowlist filter, and the count state
//! machine. These run on every platform (no `log` subprocess); the live
//! subprocess path is smoke-tested by hand on a macOS host.

use chrono::{TimeZone, Utc};
use gilb_meeting::allowlist;
use gilb_meeting::{parse_attribution_line, MeetingApp, MeetingEvent, Tracker};

/// Build the NDJSON line `log stream --style ndjson` emits, with the
/// given attributions array embedded in `eventMessage`.
fn log_line(attributions: &str) -> String {
    let message = format!("Active activity attributions changed to {attributions}");
    serde_json::json!({
        "subsystem": "com.apple.controlcenter",
        "category": "sensor-indicators",
        "eventMessage": message,
    })
    .to_string()
}

fn app(bundle_id: &str, display_name: &str) -> MeetingApp {
    MeetingApp {
        bundle_id: bundle_id.to_string(),
        display_name: display_name.to_string(),
    }
}

#[test]
fn parse_extracts_mic_bundle_ids_and_ignores_other_sensors() {
    let line =
        log_line(r#"["mic:us.zoom.xos", "camera:com.apple.FaceTime", "mic:com.apple.Safari"]"#);
    let mics = parse_attribution_line(&line).expect("attribution line");
    // No allowlist filtering at this layer: both mic entries come back,
    // camera is dropped.
    assert_eq!(mics, vec!["us.zoom.xos", "com.apple.Safari"]);
}

#[test]
fn parse_returns_empty_set_for_meeting_ended_line() {
    let line = log_line("[]");
    // Matched the predicate but no mic apps -> Some(empty), the signal
    // the tracker turns into `Ended`.
    assert_eq!(parse_attribution_line(&line), Some(vec![]));

    let camera_only = log_line(r#"["camera:com.apple.FaceTime"]"#);
    assert_eq!(parse_attribution_line(&camera_only), Some(vec![]));
}

#[test]
fn parse_ignores_non_attribution_lines() {
    assert_eq!(
        parse_attribution_line("Filtering the log data using ..."),
        None
    );
    assert_eq!(parse_attribution_line(""), None);
    assert_eq!(parse_attribution_line("{not json"), None);
    // Valid JSON, but an unrelated message.
    let other = serde_json::json!({ "eventMessage": "something else" }).to_string();
    assert_eq!(parse_attribution_line(&other), None);
}

#[test]
fn allowlist_filter_keeps_known_apps_only() {
    assert!(allowlist::is_allowlisted("us.zoom.xos"));
    assert!(allowlist::is_allowlisted("com.microsoft.teams2"));
    assert!(!allowlist::is_allowlisted("com.apple.Safari"));
    assert!(!allowlist::is_allowlisted("com.google.Chrome"));

    assert_eq!(allowlist::display_name("us.zoom.xos"), Some("Zoom"));
    assert_eq!(allowlist::display_name("com.apple.Safari"), None);
}

#[test]
fn tracker_emits_started_appschanged_then_ended() {
    let mut tracker = Tracker::new();
    let t0 = Utc.timestamp_opt(1_000, 0).unwrap();
    let t1 = Utc.timestamp_opt(1_010, 0).unwrap();
    let t2 = Utc.timestamp_opt(1_100, 0).unwrap();

    // Idle -> no apps is a no-op.
    assert_eq!(tracker.observe(vec![], t0), None);

    let zoom = app("us.zoom.xos", "Zoom");
    match tracker.observe(vec![zoom.clone()], t0) {
        Some(MeetingEvent::Started { at, apps }) => {
            assert_eq!(at, t0);
            assert_eq!(apps, vec![zoom.clone()]);
        }
        other => panic!("expected Started, got {other:?}"),
    }

    // Same set again -> no event.
    assert_eq!(tracker.observe(vec![zoom.clone()], t1), None);

    let teams = app("com.microsoft.teams2", "Microsoft Teams");
    match tracker.observe(vec![zoom.clone(), teams.clone()], t1) {
        Some(MeetingEvent::AppsChanged { at, apps }) => {
            assert_eq!(at, t1);
            // Sorted by bundle id: com.microsoft.* < us.zoom.*
            assert_eq!(apps, vec![teams, zoom.clone()]);
        }
        other => panic!("expected AppsChanged, got {other:?}"),
    }

    match tracker.observe(vec![], t2) {
        Some(MeetingEvent::Ended { at, duration }) => {
            assert_eq!(at, t2);
            // Wall time from the Started at t0 to Ended at t2.
            assert_eq!(duration, std::time::Duration::from_secs(100));
        }
        other => panic!("expected Ended, got {other:?}"),
    }
}

#[test]
fn tracker_ignores_reordering_and_duplicates() {
    let mut tracker = Tracker::new();
    let now = Utc.timestamp_opt(2_000, 0).unwrap();
    let zoom = app("us.zoom.xos", "Zoom");
    let teams = app("com.microsoft.teams2", "Microsoft Teams");

    assert!(matches!(
        tracker.observe(vec![zoom.clone(), teams.clone()], now),
        Some(MeetingEvent::Started { .. })
    ));
    // Same set, different order and with a duplicate -> no event.
    assert_eq!(
        tracker.observe(vec![teams.clone(), zoom.clone(), zoom], now),
        None
    );
}
