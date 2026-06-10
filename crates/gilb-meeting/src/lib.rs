//! Meeting detection surface for gilb.
//!
//! This crate defines the [`MeetingDetector`] trait and the
//! [`MeetingEvent`] enum that platform detectors (macOS unified log,
//! Windows WASAPI session events) emit and downstream consumers (the
//! recording pipeline, UI) subscribe to. The event names and field
//! shape are the contract referenced by `research/07-meeting-detection.md`
//! §5 — keep them in sync with that document.
//!
//! The macOS [`MacosDetector`] and Windows [`WindowsDetector`] (WASAPI session
//! events) detectors live here.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub mod allowlist;
mod macos;
mod wasapi;

#[cfg(target_os = "macos")]
pub use macos::MacosDetector;
pub use macos::{parse_attribution_line, Tracker};
pub use wasapi::{SessionEvent, SessionTracker};

#[cfg(target_os = "windows")]
pub use wasapi::WindowsDetector;

#[cfg(any(target_os = "macos", target_os = "windows"))]
const EVENT_CHANNEL_CAPACITY: usize = 64;

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct MeetingApp {
    pub bundle_id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum MeetingEvent {
    Started {
        at: DateTime<Utc>,
        apps: Vec<MeetingApp>,
    },
    AppsChanged {
        at: DateTime<Utc>,
        apps: Vec<MeetingApp>,
    },
    Ended {
        at: DateTime<Utc>,
        duration: Duration,
    },
    HealthDegraded {
        reason: String,
    },
}

#[async_trait]
pub trait MeetingDetector: Send + Sync {
    /// Start the detector. Returns a receiver of [`MeetingEvent`]s. The
    /// detector is responsible for spawning any worker tasks it needs;
    /// dropping the receiver does not stop them — call [`stop`] for that.
    async fn start(&self) -> Result<mpsc::Receiver<MeetingEvent>>;

    /// Stop the detector and release any platform resources it owns.
    async fn stop(&self) -> Result<()>;
}
