//! Startup reconciliation of the `meetings` table
//! ([`gilb_db::meetings::fail_stale_recordings`]).

use std::env;
use std::path::PathBuf;

use gilb_db::{meetings, open_db};
use uuid::Uuid;

fn temp_db_path() -> PathBuf {
    let mut p = env::temp_dir();
    p.push(format!("gilb-meetings-test-{}.sqlite", Uuid::new_v4()));
    p
}

fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}

#[tokio::test]
async fn stale_recording_rows_are_retired_and_terminal_rows_are_left_alone() {
    let path = temp_db_path();
    let db = open_db(&path).await.expect("open+migrate db");

    // Two rows stranded mid-recording by a previous run...
    let stranded_a = meetings::insert_meeting(&db, 1_000, "us.zoom.xos")
        .await
        .expect("insert");
    let stranded_b = meetings::insert_meeting(&db, 2_000, "us.zoom.xos")
        .await
        .expect("insert");
    // ...and three that already reached a terminal state.
    let completed = meetings::insert_meeting(&db, 3_000, "us.zoom.xos")
        .await
        .expect("insert");
    meetings::finish_meeting(&db, completed, 4_000, "completed")
        .await
        .expect("finish");
    let cancelled = meetings::insert_meeting(&db, 5_000, "us.zoom.xos")
        .await
        .expect("insert");
    meetings::finish_meeting(&db, cancelled, 6_000, "cancelled")
        .await
        .expect("finish");
    let failed = meetings::insert_meeting(&db, 7_000, "us.zoom.xos")
        .await
        .expect("insert");
    meetings::finish_meeting(&db, failed, 8_000, "failed")
        .await
        .expect("finish");

    let retired = meetings::fail_stale_recordings(&db)
        .await
        .expect("retire stale rows");
    assert_eq!(retired, 2, "only the two `recording` rows are touched");

    for id in [stranded_a, stranded_b] {
        let m = meetings::get_meeting(&db, id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(m.status, "failed");
        // The real end is unknown, so it stays unknown rather than being
        // backfilled with a wrong timestamp.
        assert!(
            m.ended_at.is_none(),
            "ended_at must not be invented for a stranded row"
        );
    }

    // Terminal rows keep both their status and their recorded end.
    for (id, status, ended_at) in [
        (completed, "completed", 4_000),
        (cancelled, "cancelled", 6_000),
        (failed, "failed", 8_000),
    ] {
        let m = meetings::get_meeting(&db, id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(m.status, status);
        assert_eq!(m.ended_at, Some(ended_at));
    }

    // Idempotent: a second startup has nothing left to do.
    assert_eq!(
        meetings::fail_stale_recordings(&db)
            .await
            .expect("second sweep"),
        0
    );

    // A retired row must not become a transcription candidate — that query only
    // accepts `completed`, and re-running it here guards against the sweep being
    // changed to something that would sneak these rows in.
    let pending = meetings::pending_transcriptions(&db)
        .await
        .expect("pending");
    assert!(
        !pending.contains(&stranded_a) && !pending.contains(&stranded_b),
        "stranded rows are not queued for transcription: {pending:?}"
    );

    db.close().await;
    cleanup(&path);
}
