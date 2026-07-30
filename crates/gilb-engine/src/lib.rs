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
use tokio::sync::mpsc;
use tracing::{info, warn};

use gilb_a11y::{current_platform, CapturePlatform, Permissions, RunningCapture, StartContext};
use gilb_config::RecordingSettings;
use gilb_core::{SessionId, WriterMessage};
use gilb_db::{actions, meetings, open_db, sessions, tree_snapshots, write_batch, Db};
use gilb_events::EventBus;

const ACTION_CHANNEL_CAPACITY: usize = 4096;

/// Flush the writer buffer once it holds this many messages. Bounds the size
/// (and memory) of a single transaction; well under SQLite's per-statement
/// limits since each message is its own `INSERT` inside the batch.
const WRITER_BATCH_MAX: usize = 256;

/// Flush a non-empty writer buffer at least this often even if it hasn't
/// filled, so the latency from capture to a queryable row stays bounded during
/// light activity.
const WRITER_FLUSH_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Clone)]
pub struct Engine {
    inner: Arc<EngineInner>,
}

struct EngineInner {
    db: Db,
    event_bus: EventBus,
    platform: Box<dyn CapturePlatform>,
    state: Mutex<Option<ActiveSession>>,
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

        // Nothing can still be recording at startup, so any row that says it is
        // was stranded by a previous run that never reached the recorder's stop
        // path — a crash or a force-quit mid-call. Retire them here or they stay
        // `recording` forever and misreport an active capture. Non-fatal: a
        // failure here costs accurate history, not the session.
        match meetings::fail_stale_recordings(&db).await {
            Ok(0) => {}
            Ok(n) => warn!(
                count = n,
                "retired meetings left mid-recording by a previous run"
            ),
            Err(err) => warn!(error = %err, "could not retire stale recording rows"),
        }

        let event_bus = EventBus::new();
        let platform = current_platform();
        Ok(Self {
            inner: Arc::new(EngineInner {
                db,
                event_bus,
                platform,
                state: Mutex::new(None),
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
    }
}
