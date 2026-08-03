//! In-process pub/sub bus for capture-health and recording-lifecycle events.
//!
//! Broadcast, not queued: a subscriber that falls behind is told it lagged and
//! re-syncs from current state rather than replaying a backlog. That suits both
//! consumers — the UI wants what is true now, and the recorder reacts to the
//! latest countdown outcome, not an old one.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Capacity of the broadcast channel — small, since lagging subscribers should
/// notice and re-sync rather than keep stale state.
const BUS_CAPACITY: usize = 64;

/// Capture-pipeline diagnostics. Permission changes are deliberately *not*
/// here: the UI polls `status` every few seconds and reads the current grants
/// from the OS, which stays correct even if an event is missed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HealthEvent {
    Started,
    Stopped { reason: String },
    DroppedEvent { reason: String, count: u64 },
}

/// Outcome of the pre-record countdown popup. `Armed` means the user let the
/// countdown finish or pressed Record; `Cancelled` means they backed out. The
/// recorder starts and stops on these, and the assist pipeline uses them to
/// decide whether to keep the speech model resident.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordingEvent {
    Armed { meeting_id: i64 },
    Cancelled { meeting_id: i64 },
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

/// Bus carrying health and recording-lifecycle events.
///
/// Cheap to clone — internally an `Arc` of two `broadcast::Sender`s.
#[derive(Clone)]
pub struct EventBus {
    health_tx: broadcast::Sender<BusMessage<HealthEvent>>,
    recording_tx: broadcast::Sender<BusMessage<RecordingEvent>>,
}

impl EventBus {
    pub fn new() -> Self {
        let (health_tx, _) = broadcast::channel(BUS_CAPACITY);
        let (recording_tx, _) = broadcast::channel(BUS_CAPACITY);
        Self {
            health_tx,
            recording_tx,
        }
    }

    pub fn publish_health(&self, ev: HealthEvent) {
        let _ = self.health_tx.send(BusMessage::now(ev));
    }

    pub fn publish_recording(&self, ev: RecordingEvent) {
        let _ = self.recording_tx.send(BusMessage::now(ev));
    }

    pub fn subscribe_health(&self) -> broadcast::Receiver<BusMessage<HealthEvent>> {
        self.health_tx.subscribe()
    }

    pub fn subscribe_recording(&self) -> broadcast::Receiver<BusMessage<RecordingEvent>> {
        self.recording_tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_event_serde_tagged() {
        let armed = RecordingEvent::Armed { meeting_id: 42 };
        let json = serde_json::to_string(&armed).unwrap();
        assert_eq!(json, r#"{"kind":"armed","meeting_id":42}"#);

        let back: RecordingEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, RecordingEvent::Armed { meeting_id: 42 }));

        let cancelled: RecordingEvent =
            serde_json::from_str(r#"{"kind":"cancelled","meeting_id":7}"#).unwrap();
        assert!(matches!(
            cancelled,
            RecordingEvent::Cancelled { meeting_id: 7 }
        ));
    }

    #[tokio::test]
    async fn recording_publish_subscribe_roundtrip() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe_recording();
        bus.publish_recording(RecordingEvent::Armed { meeting_id: 99 });

        let msg = rx.recv().await.unwrap();
        assert!(matches!(
            msg.payload,
            RecordingEvent::Armed { meeting_id: 99 }
        ));
    }
}
