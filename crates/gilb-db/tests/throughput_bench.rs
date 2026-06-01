//! Synthetic insert-throughput benchmark for `actions`.
//!
//! Inserts 10K varied `Action`s one-by-one through `actions::insert_action` and
//! reports the achieved rate. Establishes a baseline number for the
//! single-row, unbatched path so future batched-writer work (Phase 3, see
//! `tauri-plan.md`) has something concrete to defend its improvements against.
//!
//! Run:
//!
//! ```sh
//! cargo test -p gilb-db throughput_bench -- --nocapture
//! ```

use std::env;
use std::path::PathBuf;

use chrono::Utc;
use gilb_core::{Action, ActionKind, AppInfo, ElementContext, Frame, WriterMessage};
use gilb_db::{actions, open_db, sessions, write_batch};
use uuid::Uuid;

const N: usize = 10_000;
const MIN_RATE: f64 = 1_000.0;

/// Batch size used by the batched-path bench. Matches `WRITER_BATCH_MAX` in
/// `gilb-engine` so the number is representative of the real writer.
const BENCH_BATCH: usize = 256;

fn temp_db_path() -> PathBuf {
    let mut p = env::temp_dir();
    p.push(format!("gilb-throughput-bench-{}.sqlite", Uuid::new_v4()));
    p
}

fn synthetic_action(session_id: i64, i: usize) -> Action {
    let kind = match i % 3 {
        0 => ActionKind::Key,
        1 => ActionKind::Click,
        _ => ActionKind::FocusChange,
    };

    let app = AppInfo {
        bundle_id: Some(format!("com.example.app{}", i % 5)),
        name: Some(format!("App {}", i % 5)),
        pid: Some(1000 + (i as i32 % 50)),
        window_title: Some(format!("Window title {} — some realistic length", i)),
        browser_url: None,
    };

    let element = ElementContext {
        role: Some("AXButton".to_string()),
        name: Some(format!("Button label {}", i)),
        value: Some(format!("value-{}", i)),
        help: Some("Tooltip help text describing this element".to_string()),
        identifier: Some(format!("id-{}", i)),
        frame: Some(Frame {
            x: (i % 1920) as f64,
            y: (i % 1080) as f64,
            w: 120.0,
            h: 32.0,
        }),
    };

    let text_content = match kind {
        ActionKind::Key => Some("a".to_string()),
        ActionKind::Click => None,
        _ => Some(format!("focus-target-{}", i)),
    };

    Action {
        session_id,
        captured_at: Utc::now(),
        kind,
        app,
        element,
        text_content,
        password_flag: false,
        tree_snapshot_id: None,
        extra_json: None,
    }
}

#[tokio::test]
async fn throughput_bench() {
    let path = temp_db_path();
    let db = open_db(&path).await.expect("open_db");
    let session_id = sessions::start_session(&db).await.expect("start_session");

    let start = std::time::Instant::now();
    for i in 0..N {
        let action = synthetic_action(session_id, i);
        actions::insert_action(&db, &action)
            .await
            .expect("insert_action");
    }
    let elapsed = start.elapsed();

    let rate = N as f64 / elapsed.as_secs_f64();
    println!(
        "throughput_bench: inserted {} actions in {:.2?} = {:.0} inserts/sec",
        N, elapsed, rate
    );

    sessions::stop_session(&db, session_id, "bench-done")
        .await
        .expect("stop_session");
    db.close().await;
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));

    assert!(
        rate > MIN_RATE,
        "insert rate {:.0}/s below smoke threshold {:.0}/s",
        rate,
        MIN_RATE
    );
}

/// Same N rows as [`throughput_bench`], but committed through the batched
/// `write_batch` path in chunks of `BENCH_BATCH`. Printed side-by-side with the
/// per-row number, this is the concrete win the batched writer buys.
#[tokio::test]
async fn throughput_bench_batched() {
    let path = temp_db_path();
    let db = open_db(&path).await.expect("open_db");
    let session_id = sessions::start_session(&db).await.expect("start_session");

    let start = std::time::Instant::now();
    let mut buf: Vec<WriterMessage> = Vec::with_capacity(BENCH_BATCH);
    for i in 0..N {
        buf.push(WriterMessage::Action(synthetic_action(session_id, i)));
        if buf.len() >= BENCH_BATCH {
            write_batch(&db, &buf).await.expect("write_batch");
            buf.clear();
        }
    }
    write_batch(&db, &buf).await.expect("write_batch tail");
    let elapsed = start.elapsed();

    let rate = N as f64 / elapsed.as_secs_f64();
    println!(
        "throughput_bench_batched: inserted {} actions in {:.2?} = {:.0} inserts/sec (batch={})",
        N, elapsed, rate, BENCH_BATCH
    );

    let count = actions::count_in_session(&db, session_id)
        .await
        .expect("count_in_session");
    assert_eq!(count, N as i64, "all rows must be persisted");

    sessions::stop_session(&db, session_id, "bench-done")
        .await
        .expect("stop_session");
    db.close().await;
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));

    assert!(
        rate > MIN_RATE,
        "batched insert rate {:.0}/s below smoke threshold {:.0}/s",
        rate,
        MIN_RATE
    );
}
