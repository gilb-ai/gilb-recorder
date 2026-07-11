//! Capture session lifecycle.
//!
//! - [`Engine`] is the long-lived process-wide object created at app start.
//!   It owns the DB pool, event bus, and the current `CaptureSession`.
//! - [`Engine::start_capture`] opens a session row and spawns the platform
//!   capture worker + a DB writer.
//! - [`Engine::stop_capture`] tears the worker down and closes the row.
//!
//! The DB writer batches: it buffers incoming [`WriterMessage`]s and commits
//! them in one transaction once the buffer fills ([`WRITER_BATCH_MAX`]) or a
//! flush tick elapses ([`WRITER_FLUSH_INTERVAL`]), collapsing N per-row commits
//! into one fsync. If a batch transaction fails it falls back to per-row
//! inserts so one malformed message can't drop the rest of the batch.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use gilb_a11y::{current_platform, CapturePlatform, Permissions, RunningCapture, StartContext};
use gilb_config::{load_credentials, RecordingSettings};
use gilb_core::{SessionId, WriterMessage};
use gilb_db::{actions, open_db, screenshots, sessions, tree_snapshots, write_batch, Db};
use gilb_events::EventBus;
use gilb_shipper::{spawn_loop, HttpDestination, HttpScreenshotDestination, ShipConfig};

const ACTION_CHANNEL_CAPACITY: usize = 4096;

/// Flush the writer buffer once it holds this many messages. Bounds the size
/// (and memory) of a single transaction; well under SQLite's per-statement
/// limits since each message is its own `INSERT` inside the batch.
const WRITER_BATCH_MAX: usize = 256;

/// Flush a non-empty writer buffer at least this often even if it hasn't
/// filled, so the latency from capture to a queryable row stays bounded during
/// light activity.
const WRITER_FLUSH_INTERVAL: Duration = Duration::from_millis(200);

/// Ship buffered actions to gilb-web at least this often. 60s — the server-side
/// mining is retrospective (case/variant analysis), so it doesn't need
/// near-real-time freshness; a minute keeps the device lean (~1440 req/day)
/// while still shipping one batch per tick (run_once ships ≤ SHIP_BATCH).
/// (GILB-96/97/99.)
const SHIP_INTERVAL: Duration = Duration::from_secs(60);
/// Max actions per shipping pass — absorbs a burst, well under the web
/// endpoint's 4MB body cap (GILB-96/97).
const SHIP_BATCH: i64 = 2000;

#[derive(Clone)]
pub struct Engine {
    inner: Arc<EngineInner>,
}

struct EngineInner {
    db: Db,
    event_bus: EventBus,
    platform: Box<dyn CapturePlatform>,
    state: Mutex<Option<ActiveSession>>,
    /// Shutdown signal for the background shipper (GILB-96). `None` when the
    /// device isn't onboarded (no credentials) → no shipper running.
    shipper_shutdown: Option<oneshot::Sender<()>>,
}

impl Drop for EngineInner {
    fn drop(&mut self) {
        // Signal the shipper loop to exit on engine shutdown. (The oneshot
        // receiver also resolves if this sender is merely dropped, so dropping
        // the Engine stops the loop either way.)
        if let Some(tx) = self.shipper_shutdown.take() {
            let _ = tx.send(());
        }
    }
}

struct ActiveSession {
    session_id: SessionId,
    handle: RunningCapture,
    writer_join: tokio::task::JoinHandle<()>,
    writer_shutdown: tokio::sync::oneshot::Sender<()>,
}

/// Snapshot of the engine state surfaced to the UI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EngineStatus {
    pub recording: bool,
    pub session_id: Option<SessionId>,
    pub permissions: Permissions,
    pub platform: &'static str,
}

impl Engine {
    pub async fn open(db_path: std::path::PathBuf) -> Result<Self> {
        let db = open_db(&db_path).await?;
        let event_bus = EventBus::new();
        let platform = current_platform();
        // If the device is onboarded (has gilb-web credentials), start a
        // background shipper that drains the local buffer to /api/v1/ingest.
        // No credentials → no shipper; capture still works, events buffer.
        //
        // Credentials are read once, here. A device that onboards *after* the
        // Engine is open won't ship until the next app start — acceptable while
        // onboarding is a first-run step; revisit if login moves in-session.
        let shipper_shutdown = match load_credentials().ok().flatten() {
            Some(creds) => {
                let dest: Arc<dyn gilb_shipper::Destination> = Arc::new(HttpDestination::new(
                    creds.gilb_web_url.clone(),
                    creds.token.clone(),
                ));
                let shot_dest: Arc<dyn gilb_shipper::ScreenshotDestination> = Arc::new(
                    HttpScreenshotDestination::new(creds.gilb_web_url, creds.token),
                );
                let (tx, rx) = oneshot::channel();
                // The JoinHandle is intentionally dropped: the loop is detached
                // and stopped via `tx` (the oneshot in `shipper_shutdown`) on
                // Engine drop. A batch in flight at shutdown is abandoned, not
                // awaited — the server dedups by event_id, so it re-ships next
                // launch.
                spawn_loop(
                    db.clone(),
                    dest,
                    shot_dest,
                    SHIP_INTERVAL,
                    SHIP_BATCH,
                    ShipConfig::default(),
                    rx,
                );
                Some(tx)
            }
            None => None,
        };
        Ok(Self {
            inner: Arc::new(EngineInner {
                db,
                event_bus,
                platform,
                state: Mutex::new(None),
                shipper_shutdown,
            }),
        })
    }

    pub fn db(&self) -> &Db {
        &self.inner.db
    }

    pub fn event_bus(&self) -> &EventBus {
        &self.inner.event_bus
    }

    pub async fn status(&self) -> Result<EngineStatus> {
        let permissions = self.inner.platform.permissions().await;
        let (recording, session_id) = match self.inner.state.lock().as_ref() {
            Some(s) => (true, Some(s.session_id)),
            None => (false, None),
        };
        Ok(EngineStatus {
            recording,
            session_id,
            permissions,
            platform: self.inner.platform.name(),
        })
    }

    pub async fn start_capture(&self, settings: RecordingSettings) -> Result<SessionId> {
        {
            let guard = self.inner.state.lock();
            if guard.is_some() {
                return Err(anyhow!("capture is already running"));
            }
        }

        let session_id = sessions::start_session(&self.inner.db)
            .await
            .context("failed to insert session row")?;
        info!(%session_id, "session started");

        let (writer_tx, writer_rx) = mpsc::channel(ACTION_CHANNEL_CAPACITY);
        let (writer_shutdown_tx, writer_shutdown_rx) = tokio::sync::oneshot::channel();
        let writer_join = spawn_writer(self.inner.db.clone(), writer_rx, writer_shutdown_rx);

        let handle = self
            .inner
            .platform
            .start(StartContext {
                session_id,
                writer_tx,
                event_bus: self.inner.event_bus.clone(),
                settings,
            })
            .await
            .context("platform.start failed")?;

        let mut guard = self.inner.state.lock();
        *guard = Some(ActiveSession {
            session_id,
            handle,
            writer_join,
            writer_shutdown: writer_shutdown_tx,
        });
        Ok(session_id)
    }

    pub async fn stop_capture(&self, reason: impl Into<String>) -> Result<()> {
        let active = {
            let mut guard = self.inner.state.lock();
            guard.take()
        };
        let Some(active) = active else {
            return Err(anyhow!("capture is not running"));
        };

        active.handle.stop().await?;
        let _ = active.writer_shutdown.send(());
        if let Err(err) = active.writer_join.await {
            warn!(?err, "writer task panicked");
        }

        sessions::stop_session(&self.inner.db, active.session_id, reason.into()).await?;
        info!(session_id = %active.session_id, "session stopped");
        Ok(())
    }
}

fn spawn_writer(
    db: Db,
    mut rx: mpsc::Receiver<WriterMessage>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf: Vec<WriterMessage> = Vec::with_capacity(WRITER_BATCH_MAX);
        let mut flush_tick = tokio::time::interval(WRITER_FLUSH_INTERVAL);
        flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    // The platform capture is already stopped before shutdown is
                    // signalled, so drain whatever is still queued and flush it.
                    while let Ok(msg) = rx.try_recv() {
                        buf.push(msg);
                        if buf.len() >= WRITER_BATCH_MAX {
                            flush_batch(&db, &mut buf).await;
                        }
                    }
                    flush_batch(&db, &mut buf).await;
                    break;
                }
                _ = flush_tick.tick() => {
                    flush_batch(&db, &mut buf).await;
                }
                maybe_msg = rx.recv() => {
                    match maybe_msg {
                        Some(msg) => {
                            buf.push(msg);
                            if buf.len() >= WRITER_BATCH_MAX {
                                flush_batch(&db, &mut buf).await;
                            }
                        }
                        None => {
                            flush_batch(&db, &mut buf).await;
                            break;
                        }
                    }
                }
            }
        }
    })
}

/// Commit `buf` in one transaction, clearing it. On transaction failure, fall
/// back to per-row inserts so one bad message doesn't drop the whole batch.
async fn flush_batch(db: &Db, buf: &mut Vec<WriterMessage>) {
    if buf.is_empty() {
        return;
    }
    match write_batch(db, buf).await {
        Ok(()) => buf.clear(),
        Err(err) => {
            warn!(
                ?err,
                count = buf.len(),
                "batch write failed; falling back to per-row inserts"
            );
            for msg in buf.drain(..) {
                write_one(db, msg).await;
            }
        }
    }
}

async fn write_one(db: &Db, msg: WriterMessage) {
    match msg {
        WriterMessage::Action(action) => {
            if let Err(err) = actions::insert_action(db, &action).await {
                warn!(?err, "failed to insert action");
            }
        }
        WriterMessage::TreeSnapshot(snap) => {
            if let Err(err) = tree_snapshots::insert_tree_snapshot(db, &snap).await {
                warn!(?err, "failed to insert tree_snapshot");
            }
        }
        WriterMessage::Screenshot(shot) => {
            if let Err(err) = screenshots::insert_screenshot(db, &shot).await {
                warn!(?err, "failed to insert screenshot");
            }
        }
    }
}
