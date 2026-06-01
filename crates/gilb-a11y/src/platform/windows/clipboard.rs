//! Clipboard poller.
//!
//! Windows exposes a system-wide monotonic counter via
//! `GetClipboardSequenceNumber()` — the exact analog of macOS's
//! `NSPasteboard.changeCount`. We poll it on the same 750ms cadence and read
//! the text via `arboard` when it advances, so the clipboard path mirrors the
//! macOS one without needing a hidden message-only window.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info};

use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;

use crate::events::ClipboardChange;

pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(750);

/// Spawn a poller that emits a [`ClipboardChange`] every time the system
/// clipboard sequence number increments. Stops when `shutdown` resolves.
pub fn spawn_poller(
    tx: mpsc::Sender<ClipboardChange>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last = u32::MAX;
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
                    let seq = unsafe { GetClipboardSequenceNumber() };
                    if seq != last {
                        last = seq;
                        let text = read_clipboard_text();
                        let change = ClipboardChange { change_count: seq as i64, text };
                        if tx.send(change).await.is_err() {
                            debug!("receiver dropped");
                            break;
                        }
                    }
                }
            }
        }
    })
}

fn read_clipboard_text() -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let text = clipboard.get_text().ok()?;
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}
