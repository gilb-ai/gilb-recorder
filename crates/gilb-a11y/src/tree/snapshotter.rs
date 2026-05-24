//! Decide when to persist an AX-tree snapshot, off the normalizer's task.
//!
//! Wiring:
//!   normalizer focus tick ─try_send(AppInfo)─► snapshot_rx
//!     │
//!     │  Snapshotter::run consumes focus events:
//!     │    1. spawn_blocking { walk_focused_window + simhash + serialize }
//!     │    2. cache.should_store → maybe emit
//!     │    3. try_send TreeSnapshot to writer_tx (lossy; HealthEvent on drop)
//!
//! Heavy AX walk + JSON serialize live on tokio's blocking pool so the
//! normalizer task is never parked on AX. Backpressure on either channel
//! is lossy — losing a snapshot is fine, blocking input capture is not.

use chrono::Utc;
use tokio::sync::mpsc;
use tracing::debug;

use gilb_core::{AppInfo, SessionId, TreeSnapshot, WriterMessage};
use gilb_events::{EventBus, HealthEvent};

use super::cache::SnapshotCache;

#[cfg(target_os = "macos")]
use super::cache::simhash;
#[cfg(target_os = "macos")]
use super::walker_macos::{tokens_for_simhash, walk_focused_window};

/// Bound on the focus→snapshotter channel. Focus changes are rare
/// (~once per second peak); a small buffer is enough, and when the
/// walker is still busy on the prior change we'd rather drop the
/// intermediate hop than let the normalizer block.
pub const SNAPSHOT_CHANNEL_CAPACITY: usize = 8;

pub struct Snapshotter {
    session_id: SessionId,
    cache: SnapshotCache,
}

impl Snapshotter {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            cache: SnapshotCache::new(),
        }
    }

    /// Run loop owned by [`spawn_worker`]. Exits when `focus_rx` is closed.
    async fn run(
        mut self,
        mut focus_rx: mpsc::Receiver<AppInfo>,
        writer_tx: mpsc::Sender<WriterMessage>,
        event_bus: EventBus,
    ) {
        while let Some(app) = focus_rx.recv().await {
            self.capture(app, &writer_tx, &event_bus).await;
        }
        debug!("snapshot worker: focus channel closed; exiting");
    }

    async fn capture(
        &mut self,
        app: AppInfo,
        writer_tx: &mpsc::Sender<WriterMessage>,
        event_bus: &EventBus,
    ) {
        let Some(pid) = app.pid else { return };

        // AX walk + simhash + JSON serialize on tokio's blocking pool so
        // the normalizer's tokio task is never parked on AX. Order matters:
        // serialize BEFORE cache decision so a serde failure doesn't poison
        // the cache (entry would be stored without a row landing).
        let captured = tokio::task::spawn_blocking(move || walk_and_serialize(pid))
            .await
            .ok()
            .flatten();
        let Some((hash, root_json)) = captured else {
            return;
        };

        let Some(snap) = self.decide(app, hash, root_json) else {
            return;
        };

        if writer_tx
            .try_send(WriterMessage::TreeSnapshot(snap))
            .is_err()
        {
            event_bus.publish_health(HealthEvent::DroppedEvent {
                reason: "writer_tx_full_snapshot".into(),
                count: 1,
            });
        }
    }

    /// Cache-aware decision: returns the [`TreeSnapshot`] iff dedup says
    /// we should persist. Pure (no IO), so easy to test.
    fn decide(&mut self, app: AppInfo, hash: u64, root_json: String) -> Option<TreeSnapshot> {
        let app_key = app.bundle_id.as_deref().unwrap_or("-");
        // SPA navigation keeps window_title stable across URL transitions, so
        // the dedup key must include browser_url — otherwise two distinct
        // Crunchbase company pages with the same tab title collide.
        let window_key = match (app.window_title.as_deref(), app.browser_url.as_deref()) {
            (Some(t), Some(u)) => format!("{t}\t{u}"),
            (Some(t), None) => t.to_string(),
            (None, Some(u)) => u.to_string(),
            (None, None) => "-".to_string(),
        };

        if !self.cache.should_store(app_key, &window_key, hash) {
            return None;
        }

        Some(TreeSnapshot {
            session_id: self.session_id,
            captured_at: Utc::now(),
            app,
            simhash: hash as i64,
            root_json,
        })
    }
}

/// Spawn the snapshot worker. Returns the focus-event sender (small,
/// bounded; normalizer try_sends into it) and the join handle (drop the
/// sender to shut down).
pub fn spawn_worker(
    session_id: SessionId,
    writer_tx: mpsc::Sender<WriterMessage>,
    event_bus: EventBus,
) -> (mpsc::Sender<AppInfo>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(SNAPSHOT_CHANNEL_CAPACITY);
    let snapshotter = Snapshotter::new(session_id);
    let handle = tokio::spawn(async move { snapshotter.run(rx, writer_tx, event_bus).await });
    (tx, handle)
}

#[cfg(target_os = "macos")]
fn walk_and_serialize(pid: i32) -> Option<(u64, String)> {
    let nodes = walk_focused_window(pid)?;
    let tokens = tokens_for_simhash(&nodes);
    let hash = simhash(&tokens);
    let root_json = serde_json::to_string(&nodes).ok()?;
    Some((hash, root_json))
}

#[cfg(not(target_os = "macos"))]
fn walk_and_serialize(_pid: i32) -> Option<(u64, String)> {
    None
}
