//! Capture session lifecycle.
//!
//! - [`Engine`] is the long-lived process-wide object created at app start.
//!   It owns the DB pool, event bus, and the current `CaptureSession`.
//! - [`Engine::start_capture`] opens a session row and spawns the platform
//!   capture worker + a DB writer.
//! - [`Engine::stop_capture`] tears the worker down and closes the row.
//!
//! Phase 3 will replace the direct `insert_action` writer with a batched
//! write queue.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{info, warn};

use gilb_a11y::{current_platform, CapturePlatform, Permissions, RunningCapture, StartContext};
use gilb_config::RecordingSettings;
use gilb_core::SessionId;
use gilb_db::{actions, open_db, sessions, Db};
use gilb_events::EventBus;

const ACTION_CHANNEL_CAPACITY: usize = 4096;

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
    pub actions_today: i64,
    pub platform: &'static str,
}

impl Engine {
    pub async fn open(db_path: std::path::PathBuf) -> Result<Self> {
        let db = open_db(&db_path).await?;
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
        let actions_today = actions::count_today(&self.inner.db).await?;
        let (recording, session_id) = match self.inner.state.lock().as_ref() {
            Some(s) => (true, Some(s.session_id)),
            None => (false, None),
        };
        Ok(EngineStatus {
            recording,
            session_id,
            permissions,
            actions_today,
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
        info!(%session_id, "engine: session started");

        let (action_tx, action_rx) = mpsc::channel(ACTION_CHANNEL_CAPACITY);
        let (writer_shutdown_tx, writer_shutdown_rx) = tokio::sync::oneshot::channel();
        let writer_join = spawn_writer(self.inner.db.clone(), action_rx, writer_shutdown_rx);

        let handle = self
            .inner
            .platform
            .start(StartContext {
                session_id,
                action_tx,
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
        info!(session_id = %active.session_id, "engine: session stopped");
        Ok(())
    }
}

fn spawn_writer(
    db: Db,
    mut rx: mpsc::Receiver<gilb_core::Action>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    // Drain anything still pending, then exit.
                    while let Ok(action) = rx.try_recv() {
                        if let Err(err) = actions::insert_action(&db, &action).await {
                            warn!(?err, "failed to insert action on shutdown drain");
                        }
                    }
                    break;
                }
                maybe_action = rx.recv() => {
                    match maybe_action {
                        Some(action) => {
                            if let Err(err) = actions::insert_action(&db, &action).await {
                                warn!(?err, "failed to insert action");
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    })
}
