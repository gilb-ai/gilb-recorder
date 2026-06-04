//! Wires the Windows submodules into a single [`CapturePlatform`].

use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use gilb_events::HealthEvent;

use crate::events::{ClipboardChange, RawEvent};
use crate::focus::FocusState;
use crate::normalizer::Normalizer;
use crate::tree::snapshotter;
use crate::{CapturePlatform, Permissions, RunningCapture, StartContext};

use super::clipboard;
use super::focus::WindowsFocusProvider;
use super::hooks::HookThread;
use super::uia::UiaWorker;

/// Bound on the channel from the hook thread to the normalizer. Small to keep
/// latency low; overflow drops events (the normalizer reports drop stats).
const RAW_EVENT_CAPACITY: usize = 256;

/// Bound on the clipboard channel — increments are rare (~750ms).
const CLIPBOARD_CAPACITY: usize = 16;

pub struct WindowsPlatform {
    focus: FocusState,
}

impl WindowsPlatform {
    pub fn new() -> Self {
        Self {
            focus: FocusState::new(),
        }
    }
}

impl Default for WindowsPlatform {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CapturePlatform for WindowsPlatform {
    fn name(&self) -> &'static str {
        "windows"
    }

    async fn permissions(&self) -> Permissions {
        // Windows does not gate low-level hooks or UI Automation behind a
        // user-granted permission, so capture is always available.
        Permissions {
            accessibility: true,
            input_monitoring: true,
        }
    }

    async fn start(&self, ctx: StartContext) -> Result<RunningCapture> {
        let (raw_tx, raw_rx) = mpsc::channel::<RawEvent>(RAW_EVENT_CAPACITY);
        let (clip_tx, clip_rx) = mpsc::channel::<ClipboardChange>(CLIPBOARD_CAPACITY);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (clip_shutdown_tx, clip_shutdown_rx) = oneshot::channel::<()>();

        let mut hook_thread =
            HookThread::spawn(raw_tx).map_err(|e| anyhow!("failed to start input hooks: {e}"))?;

        let (uia_worker, uia_handle) = UiaWorker::spawn(self.focus.clone());

        ctx.event_bus.publish_health(HealthEvent::Started);

        let clipboard_handle = if ctx.settings.capture_clipboard {
            Some(clipboard::spawn_poller(clip_tx, clip_shutdown_rx))
        } else {
            drop(clip_tx);
            drop(clip_shutdown_rx);
            None
        };

        // Tree snapshots via UIA (walker_windows) on focus change. Cloned
        // writer_tx before the normalizer takes ownership below.
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
            focus_provider: Box::new(WindowsFocusProvider),
            element_resolver: Box::new(uia_worker),
            snapshot_tx,
        };

        // Shutdown choreography:
        //  1. normalizer.run returns.
        //  2. Stop the hook thread (post WM_QUIT, join).
        //  3. Signal the clipboard poller to exit and await it.
        //  4. Drop the UIA handle (joins its thread).
        let supervisor = tokio::spawn(async move {
            normalizer.run(raw_rx, clip_rx, shutdown_rx).await;
            hook_thread.stop();
            let _ = clip_shutdown_tx.send(());
            if let Some(handle) = clipboard_handle {
                if let Err(err) = tokio::time::timeout(Duration::from_secs(2), handle).await {
                    warn!(?err, "clipboard poller did not stop in time");
                }
            }
            // The normalizer dropped its snapshot_tx on return, so the worker's
            // focus channel is closed; await its in-flight walk.
            if let Some(handle) = snapshot_handle {
                if let Err(err) = tokio::time::timeout(Duration::from_secs(2), handle).await {
                    warn!(?err, "snapshot worker did not stop in time");
                }
            }
            drop(uia_handle);
            info!("shut down cleanly");
        });

        Ok(RunningCapture::new(shutdown_tx, supervisor))
    }
}
