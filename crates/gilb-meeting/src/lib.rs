//! Meeting detection surface for gilb.
//!
//! This crate defines the [`MeetingDetector`] trait and the
//! [`MeetingEvent`] enum that platform detectors (macOS unified log,
//! Windows WASAPI session events) emit and downstream consumers (the
//! recording pipeline, UI) subscribe to. The event names and field shape
//! are the contract between them: both platform detectors emit the same
//! sequence, so consumers never branch on the OS.
//!
//! The in-memory [`MockDetector`], the macOS [`MacosDetector`], and the
//! Windows [`WindowsDetector`] (WASAPI session events) live here.

use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};

pub mod allowlist;
mod macos;
mod wasapi;

#[cfg(target_os = "macos")]
pub use macos::MacosDetector;
pub use macos::{parse_attribution_line, Tracker};
pub use wasapi::{SessionEvent, SessionTracker};

#[cfg(target_os = "windows")]
pub use wasapi::WindowsDetector;

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

/// In-memory detector for tests and bring-up of downstream consumers.
///
/// `start` opens an mpsc channel and stashes the sender; [`MockDetector::fire`]
/// pushes synthetic events onto it. `stop` drops the sender, which closes
/// the receiver end.
#[derive(Default)]
pub struct MockDetector {
    sender: Mutex<Option<mpsc::Sender<MeetingEvent>>>,
}

impl MockDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a synthetic event to the receiver returned by [`start`].
    pub async fn fire(&self, event: MeetingEvent) -> Result<()> {
        let guard = self.sender.lock().await;
        let tx = guard
            .as_ref()
            .ok_or_else(|| anyhow!("MockDetector::fire called before start"))?;
        tx.send(event)
            .await
            .map_err(|_| anyhow!("MockDetector receiver dropped"))
    }
}

#[async_trait]
impl MeetingDetector for MockDetector {
    async fn start(&self) -> Result<mpsc::Receiver<MeetingEvent>> {
        let mut guard = self.sender.lock().await;
        if guard.is_some() {
            return Err(anyhow!("MockDetector already started"));
        }
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        *guard = Some(tx);
        Ok(rx)
    }

    async fn stop(&self) -> Result<()> {
        let mut guard = self.sender.lock().await;
        *guard = None;
        Ok(())
    }
}
