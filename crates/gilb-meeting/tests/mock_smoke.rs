use std::time::Duration;

use chrono::Utc;
use gilb_meeting::{MeetingApp, MeetingDetector, MeetingEvent, MockDetector};

#[tokio::test]
async fn mock_detector_emits_started_then_ended_in_order() {
    let detector = MockDetector::new();
    let mut rx = detector.start().await.expect("start succeeds");

    let zoom = MeetingApp {
        bundle_id: "us.zoom.xos".to_string(),
        display_name: "Zoom".to_string(),
    };

    let started_at = Utc::now();
    detector
        .fire(MeetingEvent::Started {
            at: started_at,
            apps: vec![zoom.clone()],
        })
        .await
        .expect("fire Started");

    let ended_at = Utc::now();
    detector
        .fire(MeetingEvent::Ended {
            at: ended_at,
            duration: Duration::from_secs(42),
        })
        .await
        .expect("fire Ended");

    match rx.recv().await.expect("Started event") {
        MeetingEvent::Started { at, apps } => {
            assert_eq!(at, started_at);
            assert_eq!(apps, vec![zoom]);
        }
        other => panic!("expected Started, got {other:?}"),
    }

    match rx.recv().await.expect("Ended event") {
        MeetingEvent::Ended { at, duration } => {
            assert_eq!(at, ended_at);
            assert_eq!(duration, Duration::from_secs(42));
        }
        other => panic!("expected Ended, got {other:?}"),
    }

    detector.stop().await.expect("stop succeeds");
}
