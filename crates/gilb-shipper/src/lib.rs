//! gilb-shipper — ship recorded actions from the local SQLite buffer to
//! gilb-web's ingest endpoint as JSONL.
//!
//! Pipeline: cursor over unshipped actions (`shipped_at IS NULL`) → JSONL
//! batch → `POST {GILB_WEB_URL}/api/v1/ingest` (Bearer ApiToken) → on ack,
//! mark `shipped_at`. Idempotent: the server dedups by `event_id`, so retrying
//! the same batch after a partial/ambiguous failure is safe.
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

/// Why a [`Destination::send`] failed — governs whether [`ship`] retries.
#[derive(Debug)]
pub enum SendError {
    /// Network error, timeout, 5xx, or a rate-limit (408/429) — retrying the
    /// same body may succeed, so [`ship`] backs off and retries.
    Transient(anyhow::Error),
    /// The server rejected the batch as-is (a non-auth, non-rate-limit 4xx,
    /// e.g. 400 malformed / 413 too large). Re-sending the identical body will
    /// fail the same way, so [`ship`] gives up immediately — and the caller
    /// may dead-letter the offending row (see [`run_once`]).
    Permanent(anyhow::Error),
    /// 401/403 — the *credentials* are bad, not the payload. Must never be
    /// dead-lettered (the data is fine and ships once the token is fixed);
    /// surfaced separately so the loop can raise a health event.
    Auth(anyhow::Error),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Transient(e) => write!(f, "transient: {e}"),
            SendError::Permanent(e) => write!(f, "permanent: {e}"),
            SendError::Auth(e) => write!(f, "auth: {e}"),
        }
    }
}

impl std::error::Error for SendError {}

/// Marker error placed at the root of the chain when a pass fails on 401/403,
/// so [`spawn_loop`] can tell an auth outage apart from ordinary failures
/// (`err.root_cause().downcast_ref::<AuthFailed>()`).
#[derive(Debug)]
pub struct AuthFailed;

impl std::fmt::Display for AuthFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ingest auth failed (401/403) — token invalid or revoked")
    }
}

impl std::error::Error for AuthFailed {}

/// Classify an HTTP response status. 2xx is handled by callers; here:
/// 5xx/408/429 → [`SendError::Transient`], 401/403 → [`SendError::Auth`],
/// any other 4xx → [`SendError::Permanent`].
fn classify_status(status: reqwest::StatusCode, err: anyhow::Error) -> SendError {
    use reqwest::StatusCode as S;
    if status.is_server_error() || status == S::REQUEST_TIMEOUT || status == S::TOO_MANY_REQUESTS {
        SendError::Transient(err)
    } else if status == S::UNAUTHORIZED || status == S::FORBIDDEN {
        SendError::Auth(err)
    } else {
        SendError::Permanent(err)
    }
}

/// Request timeouts for both HTTP destinations. Without an explicit timeout
/// `reqwest` waits forever; a stalled connection would freeze the shipper loop
/// (the select arm awaits `run_once` to completion).
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .use_rustls_tls()
        .timeout(HTTP_TIMEOUT)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .build()
        .expect("reqwest client build")
}

/// Where a batch goes. [`HttpDestination`] is prod; tests use a mock.
#[async_trait]
pub trait Destination: Send + Sync {
    /// Send one JSONL body (newline-joined). `Ok` only on a 2xx ack so the
    /// caller advances its cursor. On failure, classify it so [`ship`] knows
    /// whether a retry can help (see [`SendError`]).
    async fn send(&self, jsonl_body: &str) -> std::result::Result<(), SendError>;
}

/// Supplies the Bearer token for each send — the seam that keeps the HTTP
/// destinations agnostic of the server's auth scheme. gilb-web uses a static
/// long-lived ApiToken ([`StaticToken`]); a host with short-lived tokens
/// (e.g. a 1-hour JWT behind a refresh flow) implements this to return a
/// currently-valid token per request.
#[async_trait]
pub trait TokenProvider: Send + Sync {
    /// Return a token valid right now. Classify failures like a send:
    /// no credentials / refresh rejected → [`SendError::Auth`]; a network
    /// failure while refreshing → [`SendError::Transient`].
    async fn bearer_token(&self) -> std::result::Result<String, SendError>;
}

/// The static long-lived token gilb-web issues at onboarding.
pub struct StaticToken(pub String);

#[async_trait]
impl TokenProvider for StaticToken {
    async fn bearer_token(&self) -> std::result::Result<String, SendError> {
        Ok(self.0.clone())
    }
}

/// POSTs JSONL batches to `{base}/api/v1/ingest` with a Bearer token from a
/// [`TokenProvider`], fetched fresh per request.
pub struct HttpDestination {
    client: reqwest::Client,
    ingest_url: String,
    token: Arc<dyn TokenProvider>,
    /// Extra headers sent with every request (e.g. `X-App-Version`).
    headers: Vec<(String, String)>,
}

impl HttpDestination {
    pub fn new(gilb_web_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self::with_provider(gilb_web_url, Arc::new(StaticToken(token.into())))
    }

    pub fn with_provider(gilb_web_url: impl Into<String>, token: Arc<dyn TokenProvider>) -> Self {
        Self {
            client: http_client(),
            ingest_url: format!(
                "{}/api/v1/ingest",
                gilb_web_url.into().trim_end_matches('/')
            ),
            token,
            headers: Vec::new(),
        }
    }

    /// Add a header sent with every request.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

#[async_trait]
impl Destination for HttpDestination {
    async fn send(&self, jsonl_body: &str) -> std::result::Result<(), SendError> {
        let token = self.token.bearer_token().await?;
        let mut req = self
            .client
            .post(&self.ingest_url)
            .bearer_auth(&token)
            .header("Content-Type", "application/x-jsonlines");
        for (name, value) in &self.headers {
            req = req.header(name, value);
        }
        let resp = req
            .body(jsonl_body.to_string())
            .send()
            .await
            // A transport-level failure (DNS, connect, TLS, timeout) is always
            // worth retrying.
            .map_err(|e| SendError::Transient(anyhow::anyhow!("ingest request failed: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        Err(classify_status(
            status,
            anyhow::anyhow!("ingest returned {status}"),
        ))
    }
}

/// Tuning for [`ship`].
#[derive(Debug, Clone, Copy)]
pub struct ShipConfig {
    pub max_retries: u32,
    pub base_backoff: Duration,
    /// Max serialized JSONL body per send. A fetched batch whose body would
    /// exceed this is split into sub-batches (see [`run_once`]) so it can never
    /// trigger a permanent 413 that wedges the cursor (GILB-114). Default 3 MiB,
    /// comfortably under the web ingest 4 MB cap.
    pub max_body_bytes: usize,
}

impl Default for ShipConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_backoff: Duration::from_secs(2),
            max_body_bytes: 3 * 1024 * 1024,
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

/// Run `op` with exponential backoff on [`SendError::Transient`]; fail fast on
/// `Permanent`/`Auth` (a retry cannot change either). Shared by the JSONL and
/// screenshot paths so retry semantics can't drift between them.
async fn send_with_retry<F, Fut>(cfg: &ShipConfig, mut op: F) -> std::result::Result<(), SendError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<(), SendError>>,
{
    let mut attempt: u32 = 0;
    loop {
        match op().await {
            Ok(()) => return Ok(()),
            // Permanent/auth rejections won't change on retry — surface now
            // instead of burning the full backoff schedule every pass.
            Err(e @ (SendError::Permanent(_) | SendError::Auth(_))) => return Err(e),
            Err(SendError::Transient(e)) => {
                attempt += 1;
                if attempt > cfg.max_retries {
                    return Err(SendError::Transient(
                        e.context(format!("send failed after {} retries", cfg.max_retries)),
                    ));
                }
                let backoff =
                    (cfg.base_backoff * 2u32.saturating_pow(attempt)).min(Duration::from_secs(30));
                warn!(attempt, backoff_secs = backoff.as_secs(), error = %e, "send failed; retrying");
                tokio::time::sleep(backoff).await;
            }
        }
    }
}

/// Send a batch with exponential backoff. Idempotent: the body carries
/// `event_id`s, so re-sending after a partial/ambiguous failure is safe
/// (the server dedups). Returns `Ok` only after an ack; the error keeps the
/// [`SendError`] classification so callers can react per category.
pub async fn ship(
    dest: &dyn Destination,
    rows: &[EventRow],
    cfg: &ShipConfig,
) -> std::result::Result<(), SendError> {
    if rows.is_empty() {
        return Ok(());
    }
    let body = to_jsonl(rows).map_err(SendError::Permanent)?;
    send_with_retry(cfg, || dest.send(&body)).await
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
        let masked = password_flag != 0;
        // Password-field rows are stored raw; the local MCP read masks them via
        // a SQL CASE (`apps/gilb-mcp/src/queries.rs`). The shipper is a SECOND
        // reader and must apply the SAME masking so secure-field values never
        // leave the device over the wire (GILB-111).
        let mask = |v: Option<String>| {
            if masked {
                Some("[masked]".to_string())
            } else {
                v
            }
        };
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
                element_value: mask(r.try_get("element_value")?),
                element_identifier: r.try_get("element_id")?,
                text_content: mask(r.try_get("text_content")?),
                password_flag: masked,
                clipboard_op: r.try_get("clipboard_op")?,
                content_hash: r.try_get("content_hash")?,
                // Best-effort: malformed stored `extra_json` is dropped to
                // `None` rather than failing the whole batch. Written by us, so
                // this should never fire; a bad row still ships (minus `extra`)
                // instead of wedging the cursor.
                extra: extra_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok()),
            },
        ));
    }
    Ok(out)
}

/// Mark the given action ids as shipped at `ts` (RFC3339), in one transaction.
/// One `UPDATE ... WHERE id IN (...)` per chunk rather than one statement per
/// id, chunked well under SQLite's bound-parameter cap so even a full
/// `SHIP_BATCH` collapses to a handful of statements.
pub async fn mark_shipped(db: &Db, ids: &[i64], ts: &str) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut tx = db.begin().await.context("begin mark_shipped tx")?;
    for chunk in ids.chunks(512) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("UPDATE actions SET shipped_at = ? WHERE id IN ({placeholders})");
        let mut q = sqlx::query(&sql).bind(ts);
        for id in chunk {
            q = q.bind(id);
        }
        q.execute(&mut *tx).await?;
    }
    tx.commit().await.context("commit mark_shipped")?;
    Ok(())
}

/// One shipping pass: fetch a batch, ship it (retry), mark shipped on ack.
/// Returns the number of actions shipped. On Transient failure (after retries)
/// or an auth failure the cursor is NOT advanced, so the batch is retried next
/// pass — safe because the server dedups by `event_id`.
///
/// The fetched batch is split into sub-batches whose serialized JSONL body
/// stays under `cfg.max_body_bytes`, so a single send can never exceed the web
/// ingest cap and 413 (GILB-114). Each sub-batch is shipped and marked before
/// the next, so a mid-batch failure still advances the acked prefix.
///
/// A `Permanent` rejection bisects the sub-batch to isolate the poison row(s);
/// a single row rejected permanently is **dead-lettered** — marked `shipped_at`
/// (so the cursor advances and prune reclaims it after retention) and logged
/// loudly. Without this, one oversized/malformed row at the head of the queue
/// would halt the entire actions stream forever. `Auth` errors are NEVER
/// dead-lettered: the payload is fine, only the token is bad.
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

    // Serialize each row exactly once; everything downstream works on the
    // pre-rendered lines (the send body is a simple join).
    let mut lines: Vec<(i64, String)> = Vec::with_capacity(rows.len());
    for (id, ev) in &rows {
        let line = serde_json::to_string(ev).context("serialize event row")?;
        lines.push((*id, line));
    }

    let mut shipped = 0usize;
    let mut start = 0usize;
    let mut bytes = 0usize;
    for i in 0..lines.len() {
        let line_bytes = lines[i].1.len() + 1; // +1 for the trailing newline
                                               // Flush the accumulated sub-batch before it would overflow the budget.
                                               // The `i > start` guard guarantees at least one row per send, so an
                                               // oversized single row still makes progress (via dead-letter below).
        if i > start && bytes + line_bytes > cfg.max_body_bytes {
            shipped += ship_chunk(db, dest, &lines[start..i], cfg).await?;
            start = i;
            bytes = 0;
        }
        bytes += line_bytes;
    }
    shipped += ship_chunk(db, dest, &lines[start..], cfg).await?;
    Ok(shipped)
}

/// Ship one sub-batch of pre-serialized lines; on ack, mark them shipped.
/// On `Permanent`, bisect down to the poison row and dead-letter it (see
/// [`run_once`]). Returns rows *delivered* (dead-lettered rows don't count).
fn ship_chunk<'a>(
    db: &'a Db,
    dest: &'a dyn Destination,
    chunk: &'a [(i64, String)],
    cfg: &'a ShipConfig,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<usize>> + Send + 'a>> {
    Box::pin(async move {
        if chunk.is_empty() {
            return Ok(0);
        }
        let mut body = String::with_capacity(chunk.iter().map(|(_, l)| l.len() + 1).sum());
        for (_, line) in chunk {
            body.push_str(line);
            body.push('\n');
        }
        match send_with_retry(cfg, || dest.send(&body)).await {
            Ok(()) => {
                let ids: Vec<i64> = chunk.iter().map(|(id, _)| *id).collect();
                let ts = chrono::Utc::now().to_rfc3339();
                mark_shipped(db, &ids, &ts).await?;
                Ok(ids.len())
            }
            Err(SendError::Auth(e)) => Err(anyhow::Error::new(AuthFailed).context(e)),
            Err(SendError::Transient(e)) => Err(e.context("ship failed (transient)")),
            Err(SendError::Permanent(e)) => {
                if chunk.len() == 1 {
                    // Poison row: the server will never accept it. Dead-letter
                    // so the queue keeps moving; the row stays local (visible
                    // via gilb-mcp) until retention prunes it.
                    let id = chunk[0].0;
                    warn!(
                        action_id = id,
                        error = %e,
                        "poison row rejected permanently; dead-lettering (marked shipped, NOT delivered)"
                    );
                    let ts = chrono::Utc::now().to_rfc3339();
                    mark_shipped(db, &[id], &ts).await?;
                    Ok(0)
                } else {
                    // Isolate the poison row(s); healthy rows still ship.
                    let mid = chunk.len() / 2;
                    let a = ship_chunk(db, dest, &chunk[..mid], cfg).await?;
                    let b = ship_chunk(db, dest, &chunk[mid..], cfg).await?;
                    Ok(a + b)
                }
            }
        }
    })
}

/// Local retention for shipped rows: keep a week queryable via gilb-mcp; older
/// shipped rows are already on the server (`received_batches`, GILB-83), so
/// they're pruned to bound SQLite growth (GILB-98).
pub const RETAIN_SHIPPED: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How many rows a single `prune_shipped` DELETE removes before yielding the
/// SQLite write lock. Small enough that a large backlog prune can't stall the
/// capture writer (GILB-116).
const PRUNE_CHUNK: i64 = 500;

/// Delete shipped (`shipped_at IS NOT NULL`) actions older than `retain`
/// (now − retain). Returns rows deleted. Unshipped rows are never touched;
/// shipped rows are already server-side before `shipped_at` is set.
///
/// Deletes in bounded chunks so a large first prune (after a long backlog)
/// never holds the write lock long enough to stall the capture writer. Uses the
/// portable `id IN (SELECT … LIMIT …)` form because `DELETE … LIMIT` needs a
/// non-default SQLite compile flag.
pub async fn prune_shipped(db: &Db, retain: Duration) -> Result<u64> {
    let cutoff =
        (chrono::Utc::now() - chrono::Duration::seconds(retain.as_secs() as i64)).to_rfc3339();
    let mut total = 0u64;
    loop {
        let res = sqlx::query(
            "DELETE FROM actions WHERE id IN \
             (SELECT id FROM actions \
              WHERE shipped_at IS NOT NULL AND shipped_at < ? \
              ORDER BY id LIMIT ?)",
        )
        .bind(&cutoff)
        .bind(PRUNE_CHUNK)
        .execute(db)
        .await
        .context("prune_shipped delete")?;
        let n = res.rows_affected();
        total += n;
        if n < PRUNE_CHUNK as u64 {
            break;
        }
    }
    Ok(total)
}

/// Run a long-lived shipper: tick every `interval`, call [`run_once`], log the
/// result, and exit when `shutdown` fires (or its sender is dropped). The
/// Engine spawns this when onboarding Credentials are present; it drains the
/// buffer independent of capture sessions.
/// Everything [`spawn_loop`] needs beyond the DB, destinations, and shutdown
/// signal, in one place (was 4 positional params + 3 hardcoded constants).
pub struct ShipperOpts {
    /// Shipping cadence — both streams are attempted once per tick.
    pub interval: Duration,
    /// Max action rows fetched per pass.
    pub batch: i64,
    /// Max screenshot rows fetched per pass (images are heavy; each is its own
    /// multipart request).
    pub screenshot_batch: i64,
    /// How often the retention prune runs.
    pub prune_interval: Duration,
    /// Local retention for shipped rows (see [`RETAIN_SHIPPED`]).
    pub retain: Duration,
    /// Retry/backoff/body-budget tuning.
    pub cfg: ShipConfig,
    /// When set, the loop publishes [`HealthEvent::ShipperAuthFailed`] on
    /// 401/403 so the UI can prompt a re-sign-in (once per auth outage, not
    /// once per tick).
    pub event_bus: Option<gilb_events::EventBus>,
}

impl Default for ShipperOpts {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
            batch: 2000,
            screenshot_batch: SCREENSHOT_SHIP_BATCH,
            prune_interval: Duration::from_secs(3600),
            retain: RETAIN_SHIPPED,
            cfg: ShipConfig::default(),
            event_bus: None,
        }
    }
}

/// True when `err`'s chain bottoms out in [`AuthFailed`] (401/403).
pub fn is_auth_error(err: &anyhow::Error) -> bool {
    err.root_cause().downcast_ref::<AuthFailed>().is_some()
}

pub fn spawn_loop(
    db: Db,
    dest: Arc<dyn Destination>,
    shot_dest: Arc<dyn ScreenshotDestination>,
    opts: ShipperOpts,
    mut shutdown: oneshot::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(opts.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut prune_tick = tokio::time::interval(opts.prune_interval);
        prune_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Consume the interval's immediate first fire so the first prune happens
        // after one prune interval, not at startup.
        prune_tick.tick().await;
        // Rising-edge latch: publish ShipperAuthFailed once per outage, not
        // every 60s tick while the token stays bad.
        let mut auth_down = false;
        let on_result = |res: Result<usize>, what: &str, auth_down: &mut bool| match res {
            Ok(0) => *auth_down = false,
            Ok(n) => {
                *auth_down = false;
                info!(n, what, "shipper shipped");
            }
            Err(e) => {
                if is_auth_error(&e) {
                    if !*auth_down {
                        *auth_down = true;
                        tracing::error!(error = %e, "shipper auth failed (401/403); shipping stalled until re-onboarding");
                        if let Some(bus) = &opts.event_bus {
                            bus.publish_health(gilb_events::HealthEvent::ShipperAuthFailed);
                        }
                    }
                } else {
                    warn!(error = %e, what, "shipper pass failed; will retry next tick");
                }
            }
        };
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!("shipper loop stopped");
                    break;
                }
                _ = tick.tick() => {
                    let r = run_once(&db, dest.as_ref(), opts.batch, &opts.cfg).await;
                    on_result(r, "events", &mut auth_down);
                    // Screenshots ship on the same tick via their own cursor,
                    // smaller batch (images are heavy). Independent of events.
                    let r = run_once_screenshots(&db, shot_dest.as_ref(), opts.screenshot_batch, &opts.cfg).await;
                    on_result(r, "screenshots", &mut auth_down);
                }
                _ = prune_tick.tick() => {
                    match prune_shipped(&db, opts.retain).await {
                        Ok(0) => {}
                        Ok(n) => info!(n, "shipper pruned old shipped rows"),
                        Err(e) => warn!(error = %e, "shipper prune failed"),
                    }
                    match prune_shipped_screenshots(&db, opts.retain).await {
                        Ok(0) => {}
                        Ok(n) => info!(n, "shipper pruned old shipped screenshots"),
                        Err(e) => warn!(error = %e, "shipper screenshot prune failed"),
                    }
                }
            }
        }
    })
}

// ===================== Screenshots (GILB-93) =====================

/// Max screenshots shipped per pass — images are heavy, so keep it small
/// (each is a separate multipart request).
pub const SCREENSHOT_SHIP_BATCH: i64 = 50;

/// One screenshot row read for shipping (metadata; image bytes are on disk).
#[derive(Debug, Clone)]
pub struct ScreenshotRow {
    pub id: i64,
    pub session_id: i64,
    pub captured_at: String,
    pub app_bundle_id: Option<String>,
    pub app_name: Option<String>,
    pub window_title: Option<String>,
    pub screenshot_id: String,
    pub image_path: String,
    pub width: i64,
    pub height: i64,
}

/// Where a screenshot goes. [`HttpScreenshotDestination`] is prod; tests mock.
#[async_trait]
pub trait ScreenshotDestination: Send + Sync {
    /// Send one screenshot: a JSON `meta` part + the raw image bytes (borrowed
    /// — one buffer lives across retries). Classify failures like
    /// [`Destination::send`] (see [`SendError`]).
    async fn send(
        &self,
        meta_json: &str,
        image: &[u8],
        filename: &str,
    ) -> std::result::Result<(), SendError>;
}

/// Multipart POST to `{base}/api/v1/ingest/screenshots` with a Bearer token
/// from a [`TokenProvider`], fetched fresh per request.
pub struct HttpScreenshotDestination {
    client: reqwest::Client,
    ingest_url: String,
    token: Arc<dyn TokenProvider>,
    /// Extra headers sent with every request (e.g. `X-App-Version`).
    headers: Vec<(String, String)>,
}

impl HttpScreenshotDestination {
    pub fn new(gilb_web_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self::with_provider(gilb_web_url, Arc::new(StaticToken(token.into())))
    }

    pub fn with_provider(gilb_web_url: impl Into<String>, token: Arc<dyn TokenProvider>) -> Self {
        Self {
            client: http_client(),
            ingest_url: format!(
                "{}/api/v1/ingest/screenshots",
                gilb_web_url.into().trim_end_matches('/')
            ),
            token,
            headers: Vec::new(),
        }
    }

    /// Add a header sent with every request.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

#[async_trait]
impl ScreenshotDestination for HttpScreenshotDestination {
    async fn send(
        &self,
        meta_json: &str,
        image: &[u8],
        filename: &str,
    ) -> std::result::Result<(), SendError> {
        // One copy per attempt (multipart wants owned bytes) — the caller keeps
        // a single buffer across retries instead of cloning up front.
        let part = reqwest::multipart::Part::bytes(image.to_vec())
            .file_name(filename.to_string())
            .mime_str("image/jpeg")
            .expect("static mime");
        let form = reqwest::multipart::Form::new()
            .text("meta", meta_json.to_string())
            .part("image", part);
        let token = self.token.bearer_token().await?;
        let mut req = self.client.post(&self.ingest_url).bearer_auth(&token);
        for (name, value) in &self.headers {
            req = req.header(name, value);
        }
        let resp = req
            .multipart(form)
            .send()
            .await
            .map_err(|e| SendError::Transient(anyhow::anyhow!("screenshot request failed: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        Err(classify_status(
            status,
            anyhow::anyhow!("screenshot ingest returned {status}"),
        ))
    }
}

const SCREENSHOT_FETCH_COLUMNS: &str = "id, session_id, captured_at, \
app_bundle_id, app_name, window_title, screenshot_id, image_path, width, height";

/// Fetch up to `limit` unshipped screenshots (`shipped_at IS NULL`), oldest first.
pub async fn fetch_unshipped_screenshots(db: &Db, limit: i64) -> Result<Vec<ScreenshotRow>> {
    let sql = format!(
        "SELECT {SCREENSHOT_FETCH_COLUMNS} FROM screenshots WHERE shipped_at IS NULL ORDER BY id LIMIT ?"
    );
    let rows = sqlx::query(&sql)
        .bind(limit)
        .fetch_all(db)
        .await
        .context("fetch_unshipped_screenshots query")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(ScreenshotRow {
            id: r.try_get("id")?,
            session_id: r.try_get("session_id")?,
            captured_at: r.try_get("captured_at")?,
            app_bundle_id: r.try_get("app_bundle_id")?,
            app_name: r.try_get("app_name")?,
            window_title: r.try_get("window_title")?,
            screenshot_id: r.try_get("screenshot_id")?,
            image_path: r.try_get("image_path")?,
            width: r.try_get("width")?,
            height: r.try_get("height")?,
        });
    }
    Ok(out)
}

fn screenshot_meta_json(row: &ScreenshotRow) -> String {
    serde_json::json!({
        "screenshot_id": row.screenshot_id,
        "session_id": row.session_id,
        "captured_at": row.captured_at,
        "app_bundle_id": row.app_bundle_id,
        "app_name": row.app_name,
        "window_title": row.window_title,
        "width": row.width,
        "height": row.height,
    })
    .to_string()
}

/// Ship one screenshot with backoff retry (Transient only), reading its image
/// file from disk. Classified errors: a **missing image file** is `Permanent`
/// for this row (it can never ship — the caller tombstones it); other read
/// errors are `Transient` (disk hiccup); HTTP statuses classify as usual.
pub async fn ship_screenshot(
    dest: &dyn ScreenshotDestination,
    row: &ScreenshotRow,
    cfg: &ShipConfig,
) -> std::result::Result<(), SendError> {
    let image = match tokio::fs::read(&row.image_path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SendError::Permanent(anyhow::anyhow!(
                "screenshot image missing on disk: {}",
                row.image_path
            )));
        }
        Err(e) => {
            return Err(SendError::Transient(anyhow::anyhow!(
                "read screenshot image {}: {e}",
                row.image_path
            )));
        }
    };
    let meta = screenshot_meta_json(row);
    let filename = format!("{}.jpg", row.screenshot_id);
    send_with_retry(cfg, || dest.send(&meta, &image, &filename)).await
}

/// Mark screenshots shipped at `ts`, chunked (mirrors [`mark_shipped`]).
pub async fn mark_shipped_screenshots(db: &Db, ids: &[i64], ts: &str) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut tx = db
        .begin()
        .await
        .context("begin mark_shipped_screenshots tx")?;
    for chunk in ids.chunks(512) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("UPDATE screenshots SET shipped_at = ? WHERE id IN ({placeholders})");
        let mut q = sqlx::query(&sql).bind(ts);
        for id in chunk {
            q = q.bind(id);
        }
        q.execute(&mut *tx).await?;
    }
    tx.commit()
        .await
        .context("commit mark_shipped_screenshots")?;
    Ok(())
}

/// One screenshot pass: fetch a batch, ship each (one multipart request per
/// image), mark shipped on ack. Each acked row advances the cursor, so a
/// mid-batch failure keeps the successes. Server dedups by `screenshot_id`.
///
/// A row that fails **permanently** (server rejects it, or its image file is
/// gone from disk) is tombstoned — marked `shipped_at` and skipped — so one
/// bad row at the head of the queue can't block every screenshot behind it
/// forever. `Transient`/`Auth` failures stop the pass without advancing past
/// the failing row (retried next tick / once the token is fixed).
pub async fn run_once_screenshots(
    db: &Db,
    dest: &dyn ScreenshotDestination,
    batch: i64,
    cfg: &ShipConfig,
) -> Result<usize> {
    let rows = fetch_unshipped_screenshots(db, batch).await?;
    if rows.is_empty() {
        return Ok(0);
    }
    let mut acked: Vec<i64> = Vec::new();
    let mut shipped = 0usize;
    let ts = chrono::Utc::now().to_rfc3339();
    for row in &rows {
        match ship_screenshot(dest, row, cfg).await {
            Ok(()) => {
                acked.push(row.id);
                shipped += 1;
            }
            Err(SendError::Permanent(e)) => {
                // Tombstone: this row can never ship. Mark it so the queue
                // moves on; the file (if present) is reclaimed by prune after
                // retention, like any shipped row.
                warn!(
                    screenshot_id = %row.screenshot_id,
                    error = %e,
                    "screenshot rejected permanently; tombstoning (marked shipped, NOT delivered)"
                );
                acked.push(row.id);
            }
            Err(SendError::Auth(e)) => {
                mark_shipped_screenshots(db, &acked, &ts).await?;
                return Err(anyhow::Error::new(AuthFailed).context(e));
            }
            Err(SendError::Transient(e)) => {
                mark_shipped_screenshots(db, &acked, &ts).await?;
                return Err(e.context("screenshot ship failed (transient)"));
            }
        }
    }
    mark_shipped_screenshots(db, &acked, &ts).await?;
    Ok(shipped)
}

/// Delete shipped screenshots older than `retain`, removing BOTH the row and
/// the on-disk image file. Chunked like [`prune_shipped`].
pub async fn prune_shipped_screenshots(db: &Db, retain: Duration) -> Result<u64> {
    let cutoff =
        (chrono::Utc::now() - chrono::Duration::seconds(retain.as_secs() as i64)).to_rfc3339();
    let mut total = 0u64;
    loop {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, image_path FROM screenshots \
             WHERE shipped_at IS NOT NULL AND shipped_at < ? ORDER BY id LIMIT ?",
        )
        .bind(&cutoff)
        .bind(PRUNE_CHUNK)
        .fetch_all(db)
        .await
        .context("prune_shipped_screenshots select")?;
        if rows.is_empty() {
            break;
        }
        let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("DELETE FROM screenshots WHERE id IN ({placeholders})");
        let mut q = sqlx::query(&sql);
        for id in &ids {
            q = q.bind(id);
        }
        let res = q
            .execute(db)
            .await
            .context("prune_shipped_screenshots delete")?;
        // Best-effort file removal — the row is authoritative; a stray file is
        // harmless and gets caught by the next data-dir cleanup.
        for (_, path) in &rows {
            let _ = tokio::fs::remove_file(path).await;
        }
        total += res.rows_affected();
        if (ids.len() as i64) < PRUNE_CHUNK {
            break;
        }
    }
    Ok(total)
}

// ===================== Local janitor =====================
//
// Bounds local growth even when the shipper never runs (device not onboarded,
// token revoked) and reclaims files that lost their DB row. The Engine spawns
// [`spawn_janitor`] unconditionally — unlike the shipper, it needs no
// credentials.

/// Cap on *unshipped* screenshot rows kept locally. At the capture ceiling of
/// 40 shots/hour this is ~25h of backlog; beyond it the oldest unshipped
/// screenshots (rows + files) are dropped. Actions are not capped — they are
/// small text rows, and dropping them silently would lose real data.
pub const MAX_UNSHIPPED_SCREENSHOTS: i64 = 1000;

/// Ignore files younger than this in the orphan sweep — a just-written JPEG's
/// row may still be sitting in the engine writer's batch buffer.
const ORPHAN_MIN_AGE: Duration = Duration::from_secs(10 * 60);

/// Delete the oldest unshipped screenshots (rows + files) beyond `max_rows`.
/// Keeps a never-shipping device's screenshot store bounded.
pub async fn prune_unshipped_screenshots(db: &Db, max_rows: i64) -> Result<u64> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, image_path FROM screenshots WHERE shipped_at IS NULL \
         ORDER BY id DESC LIMIT -1 OFFSET ?",
    )
    .bind(max_rows)
    .fetch_all(db)
    .await
    .context("prune_unshipped_screenshots select")?;
    if rows.is_empty() {
        return Ok(0);
    }
    let mut total = 0u64;
    for chunk in rows.chunks(PRUNE_CHUNK as usize) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("DELETE FROM screenshots WHERE id IN ({placeholders})");
        let mut q = sqlx::query(&sql);
        for (id, _) in chunk {
            q = q.bind(id);
        }
        let res = q
            .execute(db)
            .await
            .context("prune_unshipped_screenshots delete")?;
        for (_, path) in chunk {
            let _ = tokio::fs::remove_file(path).await;
        }
        total += res.rows_affected();
    }
    warn!(
        n = total,
        max_rows, "dropped oldest unshipped screenshots over the local cap (device not shipping?)"
    );
    Ok(total)
}

/// Remove `*.jpg` files in `dir` that no `screenshots` row references and that
/// are older than [`ORPHAN_MIN_AGE`]. Covers every row-loss path (writer
/// channel full, insert failure, crash between file write and batch flush,
/// prune's row-then-file window).
pub async fn sweep_orphan_screenshots(db: &Db, dir: &std::path::Path) -> Result<u64> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        // No directory yet (screenshots never enabled) — nothing to sweep.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).context("sweep_orphan_screenshots read_dir"),
    };
    let referenced: Vec<(String,)> = sqlx::query_as("SELECT image_path FROM screenshots")
        .fetch_all(db)
        .await
        .context("sweep_orphan_screenshots select")?;
    let referenced: std::collections::HashSet<std::path::PathBuf> =
        referenced.into_iter().map(|(p,)| p.into()).collect();

    let mut removed = 0u64;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jpg") {
            continue;
        }
        if referenced.contains(&path) {
            continue;
        }
        let young = match entry.metadata().await.and_then(|m| m.modified()) {
            Ok(mtime) => mtime
                .elapsed()
                .map(|age| age < ORPHAN_MIN_AGE)
                .unwrap_or(true),
            Err(_) => true, // can't tell — leave it for the next sweep
        };
        if young {
            continue;
        }
        if tokio::fs::remove_file(&path).await.is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        info!(removed, "swept orphaned screenshot files");
    }
    Ok(removed)
}

/// Long-lived local maintenance loop: cap unshipped screenshots + sweep
/// orphaned files every `interval`. First pass runs shortly after start (so a
/// backlog left by a crash is reclaimed promptly), then on the interval.
pub fn spawn_janitor(
    db: Db,
    screenshots_dir: std::path::PathBuf,
    interval: Duration,
    mut shutdown: oneshot::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => break,
                _ = tick.tick() => {
                    if let Err(e) = prune_unshipped_screenshots(&db, MAX_UNSHIPPED_SCREENSHOTS).await {
                        warn!(error = %e, "janitor: unshipped screenshot cap failed");
                    }
                    if let Err(e) = sweep_orphan_screenshots(&db, &screenshots_dir).await {
                        warn!(error = %e, "janitor: orphan sweep failed");
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

    /// A failing [`TokenProvider`] short-circuits the send with its own
    /// classification — no HTTP request is attempted (the URL below isn't
    /// routable, so reaching the network would surface as Transient instead).
    #[tokio::test]
    async fn token_provider_failure_classifies_before_network() {
        struct NoCreds;
        #[async_trait]
        impl TokenProvider for NoCreds {
            async fn bearer_token(&self) -> std::result::Result<String, SendError> {
                Err(SendError::Auth(anyhow::anyhow!("not signed in")))
            }
        }
        let dest = HttpDestination::with_provider("http://invalid.invalid", Arc::new(NoCreds));
        match dest.send("{}\n").await {
            Err(SendError::Auth(_)) => {}
            other => panic!("expected Auth error from the provider, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn static_token_provider_returns_token() {
        let tok = StaticToken("gilb_abc".into()).bearer_token().await.unwrap();
        assert_eq!(tok, "gilb_abc");
    }

    /// Mock destination that fails the first N sends, then succeeds. Records
    /// every body it received (so tests can assert call count + JSONL shape).
    /// `poison` mode instead rejects (Permanent) exactly the bodies containing
    /// the given marker — used by the dead-letter/bisect tests.
    struct MockDestination {
        fail_first_n: AtomicU32,
        /// When set, the first `fail_first_n` failures are Permanent (not
        /// retryable) instead of Transient.
        permanent: bool,
        /// When set, failures are Auth (401/403) instead.
        auth: bool,
        /// When set, any body containing this substring is rejected Permanent
        /// regardless of the counters.
        poison: Option<String>,
        calls: Mutex<Vec<String>>,
    }

    impl MockDestination {
        fn fail_first(n: u32) -> Self {
            Self {
                fail_first_n: AtomicU32::new(n),
                permanent: false,
                auth: false,
                poison: None,
                calls: Mutex::new(Vec::new()),
            }
        }
        fn always_ok() -> Self {
            Self {
                fail_first_n: AtomicU32::new(0),
                permanent: false,
                auth: false,
                poison: None,
                calls: Mutex::new(Vec::new()),
            }
        }
        fn fail_permanent() -> Self {
            Self {
                fail_first_n: AtomicU32::new(1),
                permanent: true,
                auth: false,
                poison: None,
                calls: Mutex::new(Vec::new()),
            }
        }
        fn fail_auth() -> Self {
            Self {
                fail_first_n: AtomicU32::new(u32::MAX),
                permanent: false,
                auth: true,
                poison: None,
                calls: Mutex::new(Vec::new()),
            }
        }
        fn reject_poison(marker: &str) -> Self {
            Self {
                fail_first_n: AtomicU32::new(0),
                permanent: false,
                auth: false,
                poison: Some(marker.to_string()),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl Destination for MockDestination {
        async fn send(&self, body: &str) -> std::result::Result<(), SendError> {
            self.calls.lock().unwrap().push(body.to_string());
            if let Some(marker) = &self.poison {
                if body.contains(marker.as_str()) {
                    return Err(SendError::Permanent(anyhow::anyhow!("mock poison row")));
                }
                return Ok(());
            }
            // Decrement-and-test: fail while the counter is positive.
            let prev = self.fail_first_n.load(Ordering::SeqCst);
            if prev > 0 {
                self.fail_first_n
                    .store(prev.saturating_sub(1), Ordering::SeqCst);
                let e = anyhow::anyhow!("mock failure");
                return Err(if self.auth {
                    SendError::Auth(e)
                } else if self.permanent {
                    SendError::Permanent(e)
                } else {
                    SendError::Transient(e)
                });
            }
            Ok(())
        }
    }

    /// Mock screenshot destination: records (meta, bytes); can fail the first N
    /// sends (transient or permanent).
    struct MockShotDest {
        fail_first_n: AtomicU32,
        permanent: bool,
        calls: Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl MockShotDest {
        fn always_ok() -> Self {
            Self {
                fail_first_n: AtomicU32::new(0),
                permanent: false,
                calls: Mutex::new(Vec::new()),
            }
        }
        fn fail_first(n: u32, permanent: bool) -> Self {
            Self {
                fail_first_n: AtomicU32::new(n),
                permanent,
                calls: Mutex::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl ScreenshotDestination for MockShotDest {
        async fn send(
            &self,
            meta_json: &str,
            image: &[u8],
            _filename: &str,
        ) -> std::result::Result<(), SendError> {
            self.calls
                .lock()
                .unwrap()
                .push((meta_json.to_string(), image.to_vec()));
            let prev = self.fail_first_n.load(Ordering::SeqCst);
            if prev > 0 {
                self.fail_first_n.store(prev - 1, Ordering::SeqCst);
                let e = anyhow::anyhow!("mock shot failure");
                if self.permanent {
                    Err(SendError::Permanent(e))
                } else {
                    Err(SendError::Transient(e))
                }
            } else {
                Ok(())
            }
        }
    }

    /// Insert a screenshots row pointing at `path`; returns its rowid.
    async fn insert_screenshot_row(db: &Db, session_id: i64, path: &str) -> i64 {
        use gilb_core::{AppInfo, Screenshot};
        let shot = Screenshot {
            session_id,
            captured_at: chrono::Utc::now(),
            app: AppInfo {
                name: Some("App".into()),
                ..Default::default()
            },
            screenshot_id: Uuid::new_v4().to_string(),
            image_path: path.to_string(),
            width: 1440,
            height: 900,
        };
        gilb_db::screenshots::insert_screenshot(db, &shot)
            .await
            .unwrap()
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
            ..Default::default()
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
            ..Default::default()
        };
        let res = ship(&mock, &[sample_row(1)], &cfg).await;
        assert!(res.is_err(), "exhausted retries must error");
        // initial attempt + 1 retry = 2 sends.
        assert_eq!(mock.call_count(), 2);
    }

    #[tokio::test]
    async fn ship_permanent_error_does_not_retry() {
        let mock = MockDestination::fail_permanent();
        let cfg = ShipConfig {
            max_retries: 5,
            base_backoff: Duration::ZERO,
            ..Default::default()
        };
        let res = ship(&mock, &[sample_row(1)], &cfg).await;
        assert!(res.is_err(), "permanent rejection must error");
        // A permanent error is not retried despite max_retries=5: exactly 1 send.
        assert_eq!(mock.call_count(), 1);
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

    /// Insert a click whose focused element carries a value + typed text, with
    /// `password_flag` set — i.e. a secure-field interaction (GILB-111).
    async fn insert_secure_action(db: &Db, session_id: i64) {
        use gilb_core::{Action, ActionKind, AppInfo, ElementContext};
        let action = Action {
            session_id,
            captured_at: chrono::Utc::now(),
            kind: ActionKind::Click,
            app: AppInfo {
                name: Some("SecureApp".into()),
                ..Default::default()
            },
            element: ElementContext {
                value: Some("s3cr3t-value".into()),
                ..Default::default()
            },
            text_content: Some("s3cr3t-text".into()),
            password_flag: true,
            tree_snapshot_id: None,
            extra_json: None,
            clipboard_op: None,
            content_hash: None,
        };
        gilb_db::actions::insert_action(db, &action).await.unwrap();
    }

    /// Insert a click with an explicit `text_content` (to control row size).
    async fn insert_click_with_text(db: &Db, session_id: i64, text: &str) {
        use gilb_core::{Action, ActionKind, AppInfo};
        let action = Action {
            session_id,
            captured_at: chrono::Utc::now(),
            kind: ActionKind::Click,
            app: AppInfo {
                name: Some("App".into()),
                ..Default::default()
            },
            element: Default::default(),
            text_content: Some(text.to_string()),
            password_flag: false,
            tree_snapshot_id: None,
            extra_json: None,
            clipboard_op: None,
            content_hash: None,
        };
        gilb_db::actions::insert_action(db, &action).await.unwrap();
    }

    /// Bulk-insert `n` already-shipped rows dated in the distant past, in one
    /// transaction (fast enough to cross PRUNE_CHUNK in a test).
    async fn insert_old_shipped(db: &Db, session_id: i64, n: usize) {
        let mut tx = db.begin().await.unwrap();
        for _ in 0..n {
            sqlx::query(
                "INSERT INTO actions (session_id, captured_at, kind, password_flag, shipped_at) \
                 VALUES (?, '2020-01-01T00:00:00Z', 'click', 0, '2020-01-01T00:00:00Z')",
            )
            .bind(session_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
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
            ..Default::default()
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
        let shot_dest: Arc<dyn ScreenshotDestination> = Arc::new(MockShotDest::always_ok());
        let (tx, rx) = oneshot::channel();
        let handle = spawn_loop(
            db.clone(),
            dest,
            shot_dest,
            ShipperOpts {
                interval: Duration::from_millis(50),
                batch: 10,
                cfg: ShipConfig {
                    max_retries: 0,
                    base_backoff: Duration::ZERO,
                    ..Default::default()
                },
                ..Default::default()
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

    #[tokio::test]
    async fn prune_shipped_drops_old_keeps_recent_and_unshipped() {
        let path = temp_db_path();
        let db = gilb_db::open_db(&path).await.expect("open_db");
        let sid = gilb_db::sessions::start_session(&db).await.unwrap();
        for i in 0..3 {
            insert_click(&db, sid, i).await;
        }
        // ids[0]: ancient shipped · ids[1]: recent shipped · ids[2]: unshipped.
        let ids: Vec<(i64,)> = sqlx::query_as("SELECT id FROM actions ORDER BY id")
            .fetch_all(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE actions SET shipped_at = ? WHERE id = ?")
            .bind("2020-01-01T00:00:00Z")
            .bind(ids[0].0)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE actions SET shipped_at = ? WHERE id = ?")
            .bind(chrono::Utc::now().to_rfc3339())
            .bind(ids[1].0)
            .execute(&db)
            .await
            .unwrap();

        // 7-day retention → the ancient row is past the cutoff; the recent one
        // isn't; the unshipped row is excluded entirely.
        let n = prune_shipped(&db, Duration::from_secs(7 * 24 * 60 * 60))
            .await
            .unwrap();
        assert_eq!(n, 1, "only the ancient shipped row is pruned");

        let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM actions")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(remaining.0, 2, "recent-shipped + unshipped remain");
        assert_eq!(
            fetch_unshipped(&db, 10).await.unwrap().len(),
            1,
            "unshipped row is untouched"
        );

        db.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn password_flag_row_is_masked_on_egress() {
        let path = temp_db_path();
        let db = gilb_db::open_db(&path).await.expect("open_db");
        let sid = gilb_db::sessions::start_session(&db).await.unwrap();
        insert_secure_action(&db, sid).await;

        let rows = fetch_unshipped(&db, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        let ev = &rows[0].1;
        // The raw element value + typed text must NOT leave the device; the
        // shipper mirrors the MCP read-time masking (GILB-111).
        assert!(ev.password_flag);
        assert_eq!(ev.element_value.as_deref(), Some("[masked]"));
        assert_eq!(ev.text_content.as_deref(), Some("[masked]"));

        db.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn run_once_splits_oversized_batch_by_bytes() {
        let path = temp_db_path();
        let db = gilb_db::open_db(&path).await.expect("open_db");
        let sid = gilb_db::sessions::start_session(&db).await.unwrap();
        // Four rows whose per-line size (~1 KB text) each exceeds the tiny
        // budget → one row per send, and every row still ships.
        let big = "x".repeat(1024);
        for _ in 0..4 {
            insert_click_with_text(&db, sid, &big).await;
        }

        let mock = MockDestination::always_ok();
        let cfg = ShipConfig {
            max_body_bytes: 500,
            ..Default::default()
        };
        let n = run_once(&db, &mock, 10, &cfg).await.expect("run_once");
        assert_eq!(n, 4, "all rows shipped");
        assert!(
            mock.call_count() >= 2,
            "oversized batch split into multiple sends, got {}",
            mock.call_count()
        );
        assert!(fetch_unshipped(&db, 10).await.unwrap().is_empty());

        db.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn prune_shipped_chunks_large_backlog() {
        let path = temp_db_path();
        let db = gilb_db::open_db(&path).await.expect("open_db");
        let sid = gilb_db::sessions::start_session(&db).await.unwrap();

        // > one PRUNE_CHUNK of ancient shipped rows forces the delete loop to
        // iterate more than once.
        let old = PRUNE_CHUNK as usize + 3;
        insert_old_shipped(&db, sid, old).await;
        // One recent-shipped row (must survive) + one unshipped row (untouched).
        insert_click(&db, sid, 0).await;
        let recent_id: (i64,) = sqlx::query_as("SELECT MAX(id) FROM actions")
            .fetch_one(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE actions SET shipped_at = ? WHERE id = ?")
            .bind(chrono::Utc::now().to_rfc3339())
            .bind(recent_id.0)
            .execute(&db)
            .await
            .unwrap();
        insert_click(&db, sid, 1).await; // unshipped

        let n = prune_shipped(&db, Duration::from_secs(7 * 24 * 60 * 60))
            .await
            .unwrap();
        assert_eq!(n, old as u64, "all ancient rows pruned across chunks");

        let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM actions")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(remaining.0, 2, "recent-shipped + unshipped remain");
        assert_eq!(fetch_unshipped(&db, 10_000).await.unwrap().len(), 1);

        db.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn run_once_screenshots_ships_marks_then_prune_deletes_row_and_file() {
        let path = temp_db_path();
        let db = gilb_db::open_db(&path).await.expect("open_db");
        let sid = gilb_db::sessions::start_session(&db).await.unwrap();

        let mut img_paths = Vec::new();
        for i in 0..2 {
            let p = std::env::temp_dir().join(format!("gilb-shot-{}-{i}.jpg", Uuid::new_v4()));
            std::fs::write(&p, b"\xff\xd8\xff\xd9fake-jpeg").unwrap();
            insert_screenshot_row(&db, sid, p.to_str().unwrap()).await;
            img_paths.push(p);
        }
        assert_eq!(fetch_unshipped_screenshots(&db, 10).await.unwrap().len(), 2);

        let mock = MockShotDest::always_ok();
        let n = run_once_screenshots(&db, &mock, 10, &ShipConfig::default())
            .await
            .expect("run_once_screenshots");
        assert_eq!(n, 2);
        assert_eq!(mock.call_count(), 2);
        // The image bytes reached the destination.
        assert!(mock
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|(_, b)| b.starts_with(b"\xff\xd8")));
        assert!(fetch_unshipped_screenshots(&db, 10)
            .await
            .unwrap()
            .is_empty());

        // Prune (0 retention) → rows AND files gone.
        let pruned = prune_shipped_screenshots(&db, Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(pruned, 2);
        for p in &img_paths {
            assert!(!p.exists(), "image file removed on prune");
        }

        db.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn screenshot_permanent_error_tombstones_and_continues() {
        let path = temp_db_path();
        let db = gilb_db::open_db(&path).await.expect("open_db");
        let sid = gilb_db::sessions::start_session(&db).await.unwrap();
        // Two rows: the first is rejected permanently, the second must still
        // ship in the SAME pass (no head-of-line wedge).
        let mut paths = Vec::new();
        for i in 0..2 {
            let p = std::env::temp_dir().join(format!("gilb-shot-t-{}-{i}.jpg", Uuid::new_v4()));
            std::fs::write(&p, b"\xff\xd8img").unwrap();
            insert_screenshot_row(&db, sid, p.to_str().unwrap()).await;
            paths.push(p);
        }

        let mock = MockShotDest::fail_first(1, true); // first send → Permanent
        let cfg = ShipConfig {
            max_retries: 0,
            base_backoff: Duration::ZERO,
            ..Default::default()
        };
        let n = run_once_screenshots(&db, &mock, 10, &cfg)
            .await
            .expect("pass completes despite the poison row");
        assert_eq!(n, 1, "only the healthy row counts as shipped");
        // BOTH rows left the queue: one delivered, one tombstoned.
        assert!(fetch_unshipped_screenshots(&db, 10)
            .await
            .unwrap()
            .is_empty());

        for p in &paths {
            let _ = std::fs::remove_file(p);
        }
        db.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn screenshot_missing_file_tombstones_and_continues() {
        let path = temp_db_path();
        let db = gilb_db::open_db(&path).await.expect("open_db");
        let sid = gilb_db::sessions::start_session(&db).await.unwrap();
        // Row 1 points at a file that doesn't exist; row 2 is healthy.
        insert_screenshot_row(&db, sid, "/nonexistent/gilb-gone.jpg").await;
        let p = std::env::temp_dir().join(format!("gilb-shot-ok-{}.jpg", Uuid::new_v4()));
        std::fs::write(&p, b"\xff\xd8img").unwrap();
        insert_screenshot_row(&db, sid, p.to_str().unwrap()).await;

        let mock = MockShotDest::always_ok();
        let n = run_once_screenshots(&db, &mock, 10, &ShipConfig::default())
            .await
            .expect("missing file must not wedge the pass");
        assert_eq!(n, 1, "healthy row shipped; missing-file row tombstoned");
        assert!(fetch_unshipped_screenshots(&db, 10)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(mock.call_count(), 1, "no send attempted for the lost file");

        let _ = std::fs::remove_file(&p);
        db.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn poison_action_row_is_dead_lettered_and_rest_ship() {
        let path = temp_db_path();
        let db = gilb_db::open_db(&path).await.expect("open_db");
        let sid = gilb_db::sessions::start_session(&db).await.unwrap();
        // Rows 1+3 are fine; row 2 carries the poison marker the mock rejects.
        insert_click_with_text(&db, sid, "healthy-a").await;
        insert_click_with_text(&db, sid, "POISON-MARKER").await;
        insert_click_with_text(&db, sid, "healthy-b").await;

        let mock = MockDestination::reject_poison("POISON-MARKER");
        let cfg = ShipConfig {
            max_retries: 0,
            base_backoff: Duration::ZERO,
            ..Default::default()
        };
        let n = run_once(&db, &mock, 10, &cfg).await.expect("run_once");
        assert_eq!(n, 2, "both healthy rows delivered");
        // The poison row was dead-lettered → cursor fully advanced.
        assert!(
            fetch_unshipped(&db, 10).await.unwrap().is_empty(),
            "queue empty: healthy shipped, poison dead-lettered"
        );

        db.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn auth_error_does_not_dead_letter_or_advance() {
        let path = temp_db_path();
        let db = gilb_db::open_db(&path).await.expect("open_db");
        let sid = gilb_db::sessions::start_session(&db).await.unwrap();
        insert_click(&db, sid, 0).await;

        let mock = MockDestination::fail_auth();
        let cfg = ShipConfig {
            max_retries: 3,
            base_backoff: Duration::ZERO,
            ..Default::default()
        };
        let res = run_once(&db, &mock, 10, &cfg).await;
        let err = res.expect_err("auth failure must propagate");
        assert!(is_auth_error(&err), "error is classified as auth");
        // Auth fails fast: no retries burned on a dead token.
        assert_eq!(mock.call_count(), 1);
        // And the row is STILL unshipped — auth is never dead-lettered.
        assert_eq!(fetch_unshipped(&db, 10).await.unwrap().len(), 1);

        db.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn prune_unshipped_screenshots_caps_backlog_oldest_first() {
        let path = temp_db_path();
        let db = gilb_db::open_db(&path).await.expect("open_db");
        let sid = gilb_db::sessions::start_session(&db).await.unwrap();
        let mut paths = Vec::new();
        for i in 0..5 {
            let p = std::env::temp_dir().join(format!("gilb-shot-cap-{}-{i}.jpg", Uuid::new_v4()));
            std::fs::write(&p, b"\xff\xd8img").unwrap();
            insert_screenshot_row(&db, sid, p.to_str().unwrap()).await;
            paths.push(p);
        }

        let n = prune_unshipped_screenshots(&db, 3).await.unwrap();
        assert_eq!(n, 2, "two oldest rows over the cap dropped");
        assert_eq!(fetch_unshipped_screenshots(&db, 10).await.unwrap().len(), 3);
        // Oldest two files deleted, newest three intact.
        assert!(!paths[0].exists() && !paths[1].exists());
        assert!(paths[2].exists() && paths[3].exists() && paths[4].exists());

        for p in &paths {
            let _ = std::fs::remove_file(p);
        }
        db.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn orphan_sweep_removes_unreferenced_old_jpgs_only() {
        let path = temp_db_path();
        let db = gilb_db::open_db(&path).await.expect("open_db");
        let sid = gilb_db::sessions::start_session(&db).await.unwrap();

        let dir = std::env::temp_dir().join(format!("gilb-sweep-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // Referenced file (has a row) — must survive even though it's old.
        let referenced = dir.join("referenced.jpg");
        std::fs::write(&referenced, b"\xff\xd8r").unwrap();
        insert_screenshot_row(&db, sid, referenced.to_str().unwrap()).await;
        // Orphan, old mtime — must be removed.
        let orphan = dir.join("orphan.jpg");
        std::fs::write(&orphan, b"\xff\xd8o").unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        for f in [&referenced, &orphan] {
            let times = std::fs::FileTimes::new().set_modified(old);
            std::fs::File::options()
                .write(true)
                .open(f)
                .unwrap()
                .set_times(times)
                .unwrap();
        }
        // Orphan but FRESH (row may still be in the writer buffer) — kept.
        let fresh = dir.join("fresh.jpg");
        std::fs::write(&fresh, b"\xff\xd8f").unwrap();
        // Non-jpg file — ignored.
        let other = dir.join("notes.txt");
        std::fs::write(&other, b"keep").unwrap();

        let removed = sweep_orphan_screenshots(&db, &dir).await.unwrap();
        assert_eq!(removed, 1, "only the old orphan is swept");
        assert!(referenced.exists());
        assert!(!orphan.exists());
        assert!(fresh.exists());
        assert!(other.exists());

        let _ = std::fs::remove_dir_all(&dir);
        db.close().await;
        cleanup(&path);
    }
}
