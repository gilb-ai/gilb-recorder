//! Wires the macOS submodules into a single [`CapturePlatform`].

use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use gilb_events::HealthEvent;

use crate::{CapturePlatform, Permissions, RunningCapture, StartContext};

use super::ax_worker::AxWorker;
use super::event_tap::EventTap;
use super::focus::FocusState;
use super::normalizer::Normalizer;
use super::pasteboard::{self, ClipboardChange};
use super::permissions;
use crate::tree::snapshotter;

/// Bound on the in-process channel from the event-tap thread to the
/// normalizer. Small to keep latency low; overflow drops events (and the
/// normalizer publishes drop stats once per second).
const RAW_EVENT_CAPACITY: usize = 256;

/// Bound on the clipboard channel — increments are rare (~750ms).
const CLIPBOARD_CAPACITY: usize = 16;

pub struct MacosPlatform {
    focus: FocusState,
}

impl MacosPlatform {
    pub fn new() -> Self {
        Self {
            focus: FocusState::new(),
        }
    }
}

impl Default for MacosPlatform {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CapturePlatform for MacosPlatform {
    fn name(&self) -> &'static str {
        "macos"
    }

    async fn permissions(&self) -> Permissions {
        Permissions {
            accessibility: permissions::accessibility_granted(),
            input_monitoring: permissions::input_monitoring_granted(),
        }
    }

    async fn start(&self, ctx: StartContext) -> Result<RunningCapture> {
        if !permissions::accessibility_granted() {
            return Err(anyhow!(
                "Accessibility permission is not granted (System Settings → Privacy & Security → Accessibility)"
            ));
        }
        if !permissions::input_monitoring_granted() {
            return Err(anyhow!(
                "Input Monitoring permission is not granted (System Settings → Privacy & Security → Input Monitoring)"
            ));
        }

        let (raw_tx, raw_rx) = mpsc::channel(RAW_EVENT_CAPACITY);
        let (clip_tx, clip_rx) = mpsc::channel::<ClipboardChange>(CLIPBOARD_CAPACITY);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (clip_shutdown_tx, clip_shutdown_rx) = oneshot::channel::<()>();

        let event_tap =
            EventTap::spawn(raw_tx).map_err(|e| anyhow!("failed to start event tap: {e}"))?;

        let (ax_worker, ax_handle) = AxWorker::spawn(self.focus.clone());

        let clipboard_handle = pasteboard::spawn_poller(clip_tx, clip_shutdown_rx);

        ctx.event_bus.publish_health(HealthEvent::Started);

        // Snapshot worker lives on its own task — heavy AX walk + JSON
        // serialize happen on tokio's blocking pool so the normalizer
        // task is never parked on AX. Only spawned when enabled.
        let (snapshot_tx, snapshot_handle) = if ctx.settings.capture_tree_snapshots {
            let (tx, h) = snapshotter::spawn_worker(
                ctx.session_id,
                ctx.writer_tx.clone(),
                ctx.event_bus.clone(),
            );
            (Some(tx), Some(h))
        } else {
            (None, None)
        };

        let normalizer = Normalizer {
            session_id: ctx.session_id,
            writer_tx: ctx.writer_tx,
            event_bus: ctx.event_bus.clone(),
            settings: ctx.settings,
            focus: self.focus.clone(),
            ax_worker,
            snapshot_tx,
        };

        // Shutdown choreography:
        //  1. normalizer.run returns → its snapshot_tx is dropped, snapshot
        //     worker sees the channel close and exits.
        //  2. Stop the event-tap thread (no more raw events).
        //  3. Signal the clipboard poller to exit.
        //  4. Await the snapshot worker (any in-flight walk completes).
        //  5. Drop the ax-worker handle (joins its thread).
        let supervisor = tokio::spawn(async move {
            normalizer.run(raw_rx, clip_rx, shutdown_rx).await;
            drop(event_tap);
            let _ = clip_shutdown_tx.send(());
            if let Err(err) = tokio::time::timeout(Duration::from_secs(2), clipboard_handle).await {
                warn!(?err, "clipboard poller did not stop in time");
            }
            if let Some(handle) = snapshot_handle {
                if let Err(err) = tokio::time::timeout(Duration::from_secs(2), handle).await {
                    warn!(?err, "snapshot worker did not stop in time");
                }
            }
            drop(ax_handle);
            info!("shut down cleanly");
        });

        Ok(RunningCapture::new(shutdown_tx, supervisor))
    }
}
