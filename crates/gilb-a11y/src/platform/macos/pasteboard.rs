//! 750ms NSPasteboard.changeCount poller.

use std::time::Duration;

use objc2_app_kit::NSPasteboard;
use objc2_foundation::NSString;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info};

use crate::events::ClipboardChange;

pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(750);

/// Spawn a poller that emits a [`ClipboardChange`] every time
/// `NSPasteboard.changeCount` increments. Stops when `shutdown` resolves.
pub fn spawn_poller(
    tx: mpsc::Sender<ClipboardChange>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last = i64::MIN;
        let mut tick = tokio::time::interval(DEFAULT_POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        info!(interval = ?DEFAULT_POLL_INTERVAL, "started");
        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    debug!("shutting down");
                    break;
                }
                _ = tick.tick() => {
                    let snapshot = read_clipboard();
                    if let Some(snap) = snapshot {
                        if snap.change_count != last {
                            last = snap.change_count;
                            if tx.send(snap).await.is_err() {
                                debug!("receiver dropped");
                                break;
                            }
                        }
                    }
                }
            }
        }
    })
}

fn read_clipboard() -> Option<ClipboardChange> {
    let pb = NSPasteboard::generalPasteboard();
    let count = pb.changeCount() as i64;
    let text = pb.stringForType(&NSString::from_str("public.utf8-plain-text"));
    Some(ClipboardChange {
        change_count: count,
        text: text.map(|s| s.to_string()),
    })
}
