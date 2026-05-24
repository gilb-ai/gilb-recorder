//! Accessibility capture surface for gilb.
//!
//! Phase 0 ships:
//! - [`CapturePlatform`] trait + per-OS impls behind `cfg`.
//! - macOS stub that emits a single "debug" action on start, proving the
//!   end-to-end path action → channel → engine → DB.
//! - skeleton modules for text-buffer / activity-feed / budget / tree-cache
//!   that get filled in by Phase 1+.
//!
//! Real CGEventTap / AX integration lands in Phase 1 inside
//! [`platform::macos`].

pub mod activity_feed;
pub mod budget;
pub mod password_masking;
pub mod platform;
pub mod text_buffer;
pub mod tree;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

use gilb_config::RecordingSettings;
use gilb_core::{SessionId, WriterMessage};
use gilb_events::EventBus;

/// Runtime permission snapshot reported by the platform.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct Permissions {
    pub accessibility: bool,
    pub input_monitoring: bool,
}

/// Context wired into a [`CapturePlatform::start`] invocation.
pub struct StartContext {
    pub session_id: SessionId,
    pub writer_tx: mpsc::Sender<WriterMessage>,
    pub event_bus: EventBus,
    pub settings: RecordingSettings,
}

/// Handle owning the running capture worker. Drop is **not** sufficient to
/// stop capture; callers must `await stop().` explicitly.
pub struct RunningCapture {
    stop_tx: tokio::sync::oneshot::Sender<()>,
    join_handle: tokio::task::JoinHandle<()>,
}

impl RunningCapture {
    pub fn new(
        stop_tx: tokio::sync::oneshot::Sender<()>,
        join_handle: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self {
            stop_tx,
            join_handle,
        }
    }

    pub async fn stop(self) -> Result<()> {
        let _ = self.stop_tx.send(());
        self.join_handle.await?;
        Ok(())
    }
}

#[async_trait]
pub trait CapturePlatform: Send + Sync {
    /// Human-readable platform identifier — useful for logs and UI.
    fn name(&self) -> &'static str;

    /// Live permission snapshot; cheap to call.
    async fn permissions(&self) -> Permissions;

    /// Start capturing. Returns when the worker is up and running.
    async fn start(&self, ctx: StartContext) -> Result<RunningCapture>;
}

/// Returns the platform implementation built into this binary.
pub fn current_platform() -> Box<dyn CapturePlatform> {
    #[cfg(target_os = "macos")]
    {
        Box::new(platform::macos::MacosPlatform::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(platform::windows::WindowsPlatform::new())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Box::new(platform::unsupported::UnsupportedPlatform)
    }
}
