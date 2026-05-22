//! In-process pub/sub bus for permission / health / lifecycle events.
//!
//! Phase 0 wires the channel plumbing; producers/consumers across crates
//! are added incrementally in later phases.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Capacity of the broadcast channel — small, since lagging subscribers should
/// notice and re-sync rather than keep stale state.
const BUS_CAPACITY: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionEvent {
    AccessibilityGranted,
    AccessibilityLost,
    InputMonitoringGranted,
    InputMonitoringLost,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HealthEvent {
    Started,
    Stopped { reason: String },
    DroppedEvent { reason: String, count: u64 },
    AxQueryTimeout { ms: u64 },
    SleepDetected,
    WakeDetected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusMessage<T> {
    pub at: DateTime<Utc>,
    pub payload: T,
}

impl<T> BusMessage<T> {
    pub fn now(payload: T) -> Self {
        Self {
            at: Utc::now(),
            payload,
        }
    }
}

/// Bus carrying both permission and health events.
///
/// Cheap to clone — internally an `Arc` of two `broadcast::Sender`s.
#[derive(Clone)]
pub struct EventBus {
    permission_tx: broadcast::Sender<BusMessage<PermissionEvent>>,
    health_tx: broadcast::Sender<BusMessage<HealthEvent>>,
}

impl EventBus {
    pub fn new() -> Self {
        let (permission_tx, _) = broadcast::channel(BUS_CAPACITY);
        let (health_tx, _) = broadcast::channel(BUS_CAPACITY);
        Self {
            permission_tx,
            health_tx,
        }
    }

    pub fn publish_permission(&self, ev: PermissionEvent) {
        let _ = self.permission_tx.send(BusMessage::now(ev));
    }

    pub fn publish_health(&self, ev: HealthEvent) {
        let _ = self.health_tx.send(BusMessage::now(ev));
    }

    pub fn subscribe_permission(&self) -> broadcast::Receiver<BusMessage<PermissionEvent>> {
        self.permission_tx.subscribe()
    }

    pub fn subscribe_health(&self) -> broadcast::Receiver<BusMessage<HealthEvent>> {
        self.health_tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
