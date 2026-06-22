//! gilb-shipper — ship recorded actions from the local SQLite buffer to
//! gilb-web's ingest endpoint as JSONL.
//!
//! Pipeline: cursor over unshipped actions (`shipped_at IS NULL`) → JSONL
//! batch → `POST {GILB_WEB_URL}/api/v1/ingest` (Bearer ApiToken) → on ack,
//! mark `shipped_at`. Idempotent: the server dedups by `event_id`, so retrying
//! the same batch after a partial/ambiguous failure is safe. Reference:
//! `screenpipe-sync` (minus encrypt). See GILB_CAPTURE_PLAN.md §2F.
//!
//! Ships UNCOMPRESSED JSONL (zstd is a later optimization — the web ingest
//! endpoint accepts plain JSONL first).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Serialize;
use sqlx::Row;
use tokio::sync::oneshot;
use tracing::{info, warn};

use gilb_db::Db;

/// One event on the wire (one JSONL line). `event_id` is the local
/// `actions.id` — the server dedups repeats by it.
#[derive(Debug, Serialize)]
pub struct EventRow {
    pub event_id: i64,
    pub session_id: i64,
    pub captured_at: String,
    pub kind: String,
    pub app_bundle_id: Option<String>,
    pub app_name: Option<String>,
    pub app_pid: Option<i64>,
    pub window_title: Option<String>,
    pub browser_url: Option<String>,
    pub element_role: Option<String>,
    pub element_name: Option<String>,
    pub element_value: Option<String>,
    pub element_identifier: Option<String>,
    pub text_content: Option<String>,
    pub password_flag: bool,
    pub clipboard_op: Option<String>,
    pub content_hash: Option<String>,
    pub extra: Option<serde_json::Value>,
}

/// Where a batch goes. [`HttpDestination`] is prod; tests use a mock.
#[async_trait]
pub trait Destination: Send + Sync {
    /// Send one JSONL body (newline-joined). `Ok` only on a 2xx ack so the
    /// caller advances its cursor.
    async fn send(&self, jsonl_body: &str) -> Result<()>;
}

/// POSTs JSONL batches to gilb-web `/api/v1/ingest` with a Bearer ApiToken.
pub struct HttpDestination {
    client: reqwest::Client,
    ingest_url: String,
    token: String,
}

impl HttpDestination {
    pub fn new(gilb_web_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .use_rustls_tls()
                .build()
                .expect("reqwest client build"),
            ingest_url: format!(
                "{}/api/v1/ingest",
                gilb_web_url.into().trim_end_matches('/')
            ),
            token: token.into(),
        }
    }
}

#[async_trait]
impl Destination for HttpDestination {
    async fn send(&self, jsonl_body: &str) -> Result<()> {
        let resp = self
            .client
            .post(&self.ingest_url)
            .bearer_auth(&self.token)
            .header("Content-Type", "application/x-jsonlines")
            .body(jsonl_body.to_string())
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("ingest request failed: {e}"))?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(anyhow::anyhow!("ingest returned {status}"))
        }
    }
}

/// Tuning for [`ship`].
#[derive(Debug, Clone, Copy)]
pub struct ShipConfig {
    pub max_retries: u32,
    pub base_backoff: Duration,
}

impl Default for ShipConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_backoff: Duration::from_secs(2),
        }
    }
}

/// Serialize a batch to one JSONL body (one `EventRow` per line).
pub fn to_jsonl(rows: &[EventRow]) -> Result<String> {
    let mut out = String::new();
    for r in rows {
        let line = serde_json::to_string(r).context("serialize event row")?;
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

/// Send a batch with exponential backoff. Idempotent: the body carries
/// `event_id`s, so re-sending after a partial/ambiguous failure is safe
/// (the server dedups). Returns `Ok` only after an ack.
pub async fn ship(dest: &dyn Destination, rows: &[EventRow], cfg: &ShipConfig) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let body = to_jsonl(rows)?;
    let mut attempt: u32 = 0;
    loop {
        match dest.send(&body).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                attempt += 1;
                if attempt > cfg.max_retries {
                    return Err(e)
                        .with_context(|| format!("ship failed after {} retries", cfg.max_retries));
                }
                let backoff =
                    (cfg.base_backoff * 2u32.saturating_pow(attempt)).min(Duration::from_secs(30));
                warn!(attempt, backoff_secs = backoff.as_secs(), error = %e, "ship failed; retrying");
                tokio::time::sleep(backoff).await;
            }
        }
    }
}

const FETCH_COLUMNS: &str = "id, session_id, captured_at, kind, \
app_bundle_id, app_name, app_pid, window_title, browser_url, \
element_role, element_name, element_value, element_id, \
text_content, password_flag, extra_json, clipboard_op, content_hash";

/// Fetch up to `limit` unshipped actions (`shipped_at IS NULL`), oldest first.
/// Returns `(actions.id, EventRow)` pairs so the caller acks by id.
pub async fn fetch_unshipped(db: &Db, limit: i64) -> Result<Vec<(i64, EventRow)>> {
    let sql =
        format!("SELECT {FETCH_COLUMNS} FROM actions WHERE shipped_at IS NULL ORDER BY id LIMIT ?");
    let rows = sqlx::query(&sql)
        .bind(limit)
        .fetch_all(db)
        .await
        .context("fetch_unshipped query")?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let id: i64 = r.try_get("id")?;
        let extra_json: Option<String> = r.try_get("extra_json")?;
        let password_flag: i64 = r.try_get("password_flag")?;
        out.push((
            id,
            EventRow {
                event_id: id,
                session_id: r.try_get("session_id")?,
                captured_at: r.try_get("captured_at")?,
                kind: r.try_get("kind")?,
                app_bundle_id: r.try_get("app_bundle_id")?,
                app_name: r.try_get("app_name")?,
                app_pid: r.try_get("app_pid")?,
                window_title: r.try_get("window_title")?,
                browser_url: r.try_get("browser_url")?,
                element_role: r.try_get("element_role")?,
                element_name: r.try_get("element_name")?,
                element_value: r.try_get("element_value")?,
                element_identifier: r.try_get("element_id")?,
                text_content: r.try_get("text_content")?,
                password_flag: password_flag != 0,
                clipboard_op: r.try_get("clipboard_op")?,
                content_hash: r.try_get("content_hash")?,
                extra: extra_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok()),
            },
        ));
    }
    Ok(out)
}

/// Mark the given action ids as shipped at `ts` (RFC3339), in one transaction.
pub async fn mark_shipped(db: &Db, ids: &[i64], ts: &str) -> Result<()> {
    let mut tx = db.begin().await.context("begin mark_shipped tx")?;
    for id in ids {
        sqlx::query("UPDATE actions SET shipped_at = ? WHERE id = ?")
            .bind(ts)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await.context("commit mark_shipped")?;
    Ok(())
}

/// One shipping pass: fetch a batch, ship it (retry), mark shipped on ack.
/// Returns the number of actions shipped. On ship failure (after retries) the
/// cursor is NOT advanced, so the same batch is retried next pass — safe
/// because the server dedups by `event_id`.
pub async fn run_once(
    db: &Db,
    dest: &dyn Destination,
    batch: i64,
    cfg: &ShipConfig,
) -> Result<usize> {
    let rows = fetch_unshipped(db, batch).await?;
    if rows.is_empty() {
        return Ok(0);
    }
    let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
    let events: Vec<EventRow> = rows.into_iter().map(|(_, e)| e).collect();
    ship(dest, &events, cfg).await?;
    let ts = chrono::Utc::now().to_rfc3339();
    mark_shipped(db, &ids, &ts).await?;
    Ok(ids.len())
}

/// Run a long-lived shipper: tick every `interval`, call [`run_once`], log the
/// result, and exit when `shutdown` fires (or its sender is dropped). The
/// Engine spawns this when onboarding Credentials are present; it drains the
/// buffer independent of capture sessions.
pub fn spawn_loop(
    db: Db,
    dest: Arc<dyn Destination>,
    interval: Duration,
    batch: i64,
    cfg: ShipConfig,
    mut shutdown: oneshot::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!("shipper loop stopped");
                    break;
                }
                _ = tick.tick() => {
                    match run_once(&db, dest.as_ref(), batch, &cfg).await {
                        Ok(0) => {}
                        Ok(n) => info!(n, "shipper shipped events"),
                        Err(e) => warn!(error = %e, "shipper run_once failed; will retry next tick"),
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    use uuid::Uuid;

    /// Mock destination that fails the first N sends, then succeeds. Records
    /// every body it received (so tests can assert call count + JSONL shape).
    struct MockDestination {
        fail_first_n: AtomicU32,
        calls: Mutex<Vec<String>>,
    }

    impl MockDestination {
        fn fail_first(n: u32) -> Self {
            Self {
                fail_first_n: AtomicU32::new(n),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn always_ok() -> Self {
            Self {
                fail_first_n: AtomicU32::new(0),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl Destination for MockDestination {
        async fn send(&self, body: &str) -> Result<()> {
            self.calls.lock().unwrap().push(body.to_string());
            // Decrement-and-test: fail while the counter is positive.
            let prev = self.fail_first_n.load(Ordering::SeqCst);
            if prev > 0 {
                self.fail_first_n.store(prev - 1, Ordering::SeqCst);
                Err(anyhow::anyhow!("mock failure"))
            } else {
                Ok(())
            }
        }
    }

    fn sample_row(id: i64) -> EventRow {
        EventRow {
            event_id: id,
            session_id: 1,
            captured_at: "2026-06-22T00:00:00Z".into(),
            kind: "click".into(),
            app_bundle_id: Some("com.example".into()),
            app_name: Some("App".into()),
            app_pid: Some(1234),
            window_title: None,
            browser_url: None,
            element_role: Some("AXButton".into()),
            element_name: None,
            element_value: None,
            element_identifier: None,
            text_content: None,
            password_flag: false,
            clipboard_op: None,
            content_hash: None,
            extra: None,
        }
    }

    #[test]
    fn jsonl_is_one_line_per_row() {
        let body = to_jsonl(&[sample_row(1), sample_row(2)]).unwrap();
        let lines: Vec<&str> = body.trim_end().lines().collect();
        assert_eq!(lines.len(), 2);
        // Each line is valid JSON carrying event_id.
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["event_id"], 1);
        assert_eq!(first["kind"], "click");
    }

    #[tokio::test]
    async fn ship_retries_then_succeeds() {
        let mock = MockDestination::fail_first(2);
        let cfg = ShipConfig {
            max_retries: 3,
            base_backoff: Duration::ZERO,
        };
        ship(&mock, &[sample_row(1), sample_row(2)], &cfg)
            .await
            .expect("succeeds after retries");
        // 2 failures + 1 success = 3 sends; same body each time (idempotent).
        assert_eq!(mock.call_count(), 3);
    }

    #[tokio::test]
    async fn ship_all_fail_returns_err_after_retries() {
        let mock = MockDestination::fail_first(100);
        let cfg = ShipConfig {
            max_retries: 1,
            base_backoff: Duration::ZERO,
        };
        let res = ship(&mock, &[sample_row(1)], &cfg).await;
        assert!(res.is_err(), "exhausted retries must error");
        // initial attempt + 1 retry = 2 sends.
        assert_eq!(mock.call_count(), 2);
    }

    // ----- DB-backed integration tests -------------------------------------

    fn temp_db_path() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("gilb-shipper-test-{}.sqlite", Uuid::new_v4()));
        p
    }

    fn cleanup(path: &PathBuf) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    }

    async fn insert_click(db: &Db, session_id: i64, i: usize) {
        use gilb_core::{Action, ActionKind, AppInfo};
        let action = Action {
            session_id,
            captured_at: chrono::Utc::now(),
            kind: ActionKind::Click,
            app: AppInfo {
                name: Some(format!("App{i}")),
                ..Default::default()
            },
            element: Default::default(),
            text_content: Some(format!("t{i}")),
            password_flag: false,
            tree_snapshot_id: None,
            extra_json: None,
            clipboard_op: None,
            content_hash: None,
        };
        gilb_db::actions::insert_action(db, &action).await.unwrap();
    }

    #[tokio::test]
    async fn run_once_ships_and_marks_cursor() {
        let path = temp_db_path();
        let db = gilb_db::open_db(&path).await.expect("open_db");
        let session_id = gilb_db::sessions::start_session(&db).await.unwrap();

        for i in 0..3 {
            insert_click(&db, session_id, i).await;
        }
        assert_eq!(fetch_unshipped(&db, 10).await.unwrap().len(), 3);

        let mock = MockDestination::always_ok();
        let cfg = ShipConfig::default();
        let n = run_once(&db, &mock, 10, &cfg).await.expect("run_once");
        assert_eq!(n, 3);
        // Cursor advanced: nothing unshipped now.
        assert!(fetch_unshipped(&db, 10).await.unwrap().is_empty());

        db.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn run_once_does_not_advance_cursor_on_failure() {
        let path = temp_db_path();
        let db = gilb_db::open_db(&path).await.expect("open_db");
        let session_id = gilb_db::sessions::start_session(&db).await.unwrap();
        insert_click(&db, session_id, 0).await;

        let mock = MockDestination::fail_first(100);
        let cfg = ShipConfig {
            max_retries: 0,
            base_backoff: Duration::ZERO,
        };
        let res = run_once(&db, &mock, 10, &cfg).await;
        assert!(res.is_err(), "ship failure must propagate");
        // Cursor NOT advanced → row is still unshipped (retried next pass).
        assert_eq!(fetch_unshipped(&db, 10).await.unwrap().len(), 1);

        db.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn spawn_loop_drains_backlog_and_stops_on_shutdown() {
        let path = temp_db_path();
        let db = gilb_db::open_db(&path).await.expect("open_db");
        let session_id = gilb_db::sessions::start_session(&db).await.unwrap();
        for i in 0..2 {
            insert_click(&db, session_id, i).await;
        }
        assert_eq!(fetch_unshipped(&db, 10).await.unwrap().len(), 2);

        let dest: Arc<dyn Destination> = Arc::new(MockDestination::always_ok());
        let (tx, rx) = oneshot::channel();
        let handle = spawn_loop(
            db.clone(),
            dest,
            Duration::from_millis(50),
            10,
            ShipConfig {
                max_retries: 0,
                base_backoff: Duration::ZERO,
            },
            rx,
        );

        // Wait past one tick so the loop ships the backlog.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            fetch_unshipped(&db, 10).await.unwrap().is_empty(),
            "backlog drained by the loop"
        );

        let _ = tx.send(());
        handle.await.unwrap();

        db.close().await;
        cleanup(&path);
    }
}
