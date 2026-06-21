//! Correctness tests for the transactional batch writer
//! ([`gilb_db::write_batch`]).

use std::env;
use std::path::PathBuf;

use chrono::Utc;
use gilb_core::{Action, ActionKind, AppInfo, ElementContext, TreeSnapshot, WriterMessage};
use gilb_db::{actions, open_db, sessions, tree_snapshots, write_batch};
use uuid::Uuid;

fn temp_db_path() -> PathBuf {
    let mut p = env::temp_dir();
    p.push(format!("gilb-write-batch-test-{}.sqlite", Uuid::new_v4()));
    p
}

fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}

fn action(session_id: i64, i: usize) -> Action {
    Action {
        session_id,
        captured_at: Utc::now(),
        kind: ActionKind::Click,
        app: AppInfo {
            bundle_id: Some("com.example.app".into()),
            name: Some("App".into()),
            pid: Some(1234),
            window_title: Some(format!("Window {i}")),
            browser_url: None,
        },
        element: ElementContext::default(),
        text_content: Some(format!("text-{i}")),
        password_flag: false,
        tree_snapshot_id: None,
        extra_json: None,
        clipboard_op: None,
        content_hash: None,
    }
}

fn snapshot(session_id: i64, i: usize) -> TreeSnapshot {
    TreeSnapshot {
        session_id,
        captured_at: Utc::now(),
        app: AppInfo {
            bundle_id: Some("com.example.app".into()),
            name: Some("App".into()),
            pid: Some(1234),
            window_title: Some(format!("Window {i}")),
            browser_url: None,
        },
        simhash: i as i64,
        root_json: format!(r#"{{"role":"AXWindow","i":{i}}}"#),
    }
}

#[tokio::test]
async fn empty_batch_is_a_noop() {
    let path = temp_db_path();
    let db = open_db(&path).await.expect("open_db");
    write_batch(&db, &[]).await.expect("empty batch ok");
    db.close().await;
    cleanup(&path);
}

#[tokio::test]
async fn batch_persists_mixed_messages_in_one_go() {
    let path = temp_db_path();
    let db = open_db(&path).await.expect("open_db");
    let session_id = sessions::start_session(&db).await.expect("start_session");

    // Interleave actions and tree snapshots to exercise both branches and
    // confirm arrival order doesn't matter for persistence.
    let mut batch: Vec<WriterMessage> = Vec::new();
    for i in 0..50 {
        batch.push(WriterMessage::Action(action(session_id, i)));
        if i % 5 == 0 {
            batch.push(WriterMessage::TreeSnapshot(snapshot(session_id, i)));
        }
    }
    let expected_actions = 50;
    let expected_snapshots = batch.len() as i64 - expected_actions;

    write_batch(&db, &batch).await.expect("write_batch");

    let action_count = actions::count_in_session(&db, session_id)
        .await
        .expect("count_in_session");
    assert_eq!(action_count, expected_actions);

    let snap_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM tree_snapshots WHERE session_id = ?")
            .bind(session_id)
            .fetch_one(&db)
            .await
            .expect("count snapshots");
    assert_eq!(snap_count.0, expected_snapshots);

    db.close().await;
    cleanup(&path);
}

#[tokio::test]
async fn batch_matches_per_row_inserts() {
    let path = temp_db_path();
    let db = open_db(&path).await.expect("open_db");
    let session_id = sessions::start_session(&db).await.expect("start_session");

    // Per-row reference: insert 10 directly.
    for i in 0..10 {
        actions::insert_action(&db, &action(session_id, i))
            .await
            .expect("insert_action");
    }
    // Batched: insert 10 more via a single transaction.
    let batch: Vec<WriterMessage> = (10..20)
        .map(|i| WriterMessage::Action(action(session_id, i)))
        .collect();
    write_batch(&db, &batch).await.expect("write_batch");

    let count = actions::count_in_session(&db, session_id)
        .await
        .expect("count_in_session");
    assert_eq!(count, 20);

    db.close().await;
    cleanup(&path);
}

#[tokio::test]
async fn failed_batch_rolls_back_atomically() {
    let path = temp_db_path();
    let db = open_db(&path).await.expect("open_db");
    let session_id = sessions::start_session(&db).await.expect("start_session");

    // A snapshot referencing a non-existent session violates the FK, so the
    // whole batch must roll back — including the otherwise-valid action that
    // precedes it.
    let bad_session = session_id + 9999;
    let batch = vec![
        WriterMessage::Action(action(session_id, 0)),
        WriterMessage::TreeSnapshot(snapshot(bad_session, 0)),
    ];
    let res = write_batch(&db, &batch).await;
    assert!(res.is_err(), "FK violation should fail the batch");

    let count = actions::count_in_session(&db, session_id)
        .await
        .expect("count_in_session");
    assert_eq!(count, 0, "valid action must have been rolled back too");

    // Direct insert still works afterwards — the connection isn't poisoned.
    tree_snapshots::insert_tree_snapshot(&db, &snapshot(session_id, 1))
        .await
        .expect("insert after rollback");

    db.close().await;
    cleanup(&path);
}
