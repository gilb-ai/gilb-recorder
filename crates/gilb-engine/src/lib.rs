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
use gilb_shipper::{spawn_loop, HttpDestination, HttpScreenshotDestination, ShipperOpts};

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
/// Local-maintenance cadence (unshipped screenshot cap + orphan file sweep).
/// Runs regardless of onboarding — it's what bounds disk growth on a device
/// that never ships.
const JANITOR_INTERVAL: Duration = Duration::from_secs(3600);

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
    /// device isn't onboarded (no credentials) → no shipper running. Behind a
    /// Mutex because sign-in / sign-out swap the shipper in-session
    /// ([`Engine::start_shipper`] / [`Engine::stop_shipper`]).
    shipper_shutdown: Mutex<Option<oneshot::Sender<()>>>,
    /// Shutdown for the always-on local janitor (unshipped cap + orphan sweep).
    janitor_shutdown: Option<oneshot::Sender<()>>,
}

impl Drop for EngineInner {
    fn drop(&mut self) {
        // Signal the background loops to exit on engine shutdown. (The oneshot
        // receivers also resolve if the senders are merely dropped, so dropping
        // the Engine stops the loops either way.)
        if let Some(tx) = self.shipper_shutdown.lock().take() {
            let _ = tx.send(());
        }
        if let Some(tx) = self.janitor_shutdown.take() {
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
        // The janitor runs regardless of onboarding: it caps unshipped
        // screenshots and sweeps orphaned JPEGs — the only bound on disk
        // growth for a device that never ships.
        let janitor_shutdown = {
            let (tx, rx) = oneshot::channel();
            let dir = gilb_config::data_dir()
                .map(|d| d.join("screenshots"))
                .unwrap_or_else(|_| std::path::PathBuf::from("screenshots"));
            gilb_shipper::spawn_janitor(db.clone(), dir, JANITOR_INTERVAL, rx);
            Some(tx)
        };
        let engine = Self {
            inner: Arc::new(EngineInner {
                db,
                event_bus,
                platform,
                state: Mutex::new(None),
                shipper_shutdown: Mutex::new(None),
                janitor_shutdown,
            }),
        };
        // If the device is onboarded (has gilb-web credentials), start the
        // background shipper. No credentials → no shipper; capture still
        // works, events buffer locally. Sign-in/sign-out later in the session
        // swap the shipper via start_shipper/stop_shipper — no restart needed.
        match load_credentials() {
            Ok(Some(creds)) => engine.start_shipper(creds.gilb_web_url, creds.token),
            Ok(None) => {}
            Err(err) => {
                // A corrupted credentials file is NOT "not onboarded" — say so,
                // or a broken shipper looks identical to a fresh install.
                warn!(?err, "failed to load credentials; shipper NOT started");
            }
        }
        Ok(engine)
    }

    /// Start (or replace) the background shipper with these credentials.
    /// Called at open when the device is already onboarded, and by the auth
    /// deep-link callback right after sign-in — shipping begins immediately,
    /// not at the next app launch.
    pub fn start_shipper(&self, gilb_web_url: impl Into<String>, token: impl Into<String>) {
        let gilb_web_url = gilb_web_url.into();
        let token = token.into();
        let dest: Arc<dyn gilb_shipper::Destination> =
            Arc::new(HttpDestination::new(gilb_web_url.clone(), token.clone()));
        let shot_dest: Arc<dyn gilb_shipper::ScreenshotDestination> =
            Arc::new(HttpScreenshotDestination::new(gilb_web_url, token));
        let (tx, rx) = oneshot::channel();
        // The JoinHandle is intentionally dropped: the loop is detached and
        // stopped via `tx` on Engine drop / stop_shipper. A batch in flight at
        // shutdown is abandoned, not awaited — the server dedups by event_id,
        // so it re-ships next time.
        spawn_loop(
            self.inner.db.clone(),
            dest,
            shot_dest,
            ShipperOpts {
                interval: SHIP_INTERVAL,
                batch: SHIP_BATCH,
                event_bus: Some(self.inner.event_bus.clone()),
                ..Default::default()
            },
            rx,
        );
        // Replace (and thereby stop) any previous shipper — e.g. re-sign-in
        // with a fresh token.
        if let Some(old) = self.inner.shipper_shutdown.lock().replace(tx) {
            let _ = old.send(());
        }
        info!("shipper started");
    }

    /// Stop the background shipper. Called on sign-out: the in-memory token
    /// must die with the session — before this, a signed-out device kept
    /// shipping until the next app restart. Idempotent.
    pub fn stop_shipper(&self) {
        if let Some(tx) = self.inner.shipper_shutdown.lock().take() {
            let _ = tx.send(());
            info!("shipper stopped (sign-out)");
        }
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
