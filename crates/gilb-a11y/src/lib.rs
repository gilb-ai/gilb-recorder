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
pub mod events;
pub mod focus;
pub mod idle;
pub mod keyboard;
pub mod normalizer;
pub mod password_masking;
pub mod platform;
pub mod screenshot;
pub mod text_buffer;
pub mod tree;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use gilb_config::RecordingSettings;
use gilb_core::{AppInfo, ElementContext, SessionId, WriterMessage};
use gilb_events::EventBus;

/// Runtime permission snapshot reported by the platform.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct Permissions {
    pub accessibility: bool,
    pub input_monitoring: bool,
    /// Screen Recording (macOS). Required by the meeting recorder, which
    /// captures the call window's video via ScreenCaptureKit.
    pub screen_recording: bool,
    /// Microphone (macOS). Required by the meeting recorder, which captures
    /// mic audio via `cpal` alongside the system audio.
    pub microphone: bool,
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

/// Looks up the foreground application. Implemented per-OS; consumed by the
/// shared [`normalizer::Normalizer`] on its focus-poll tick.
pub trait FocusProvider: Send + Sync {
    /// App identity only (no window title). Cheap.
    fn frontmost(&self) -> AppInfo;
    /// App identity plus focused-window title and, for browsers, the active
    /// tab URL. May be bounded by an internal timeout.
    fn frontmost_with_window(&self) -> AppInfo;
}

/// Best-effort lookup of the accessibility element at a screen coordinate,
/// used to enrich clicks. Implemented per-OS (macOS AX, Windows UIA).
pub trait ElementResolver: Send + Sync {
    /// Resolve the element at `(x, y)` and reply on the channel. The
    /// implementation may drop the request (never replying) when busy —
    /// callers time out and proceed without context.
    fn submit(&self, x: f64, y: f64, reply: oneshot::Sender<Option<ElementContext>>);
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
