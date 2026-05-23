//! `CGEventTap` (listen-only) running on a dedicated thread with its own
//! `CFRunLoop`.
//!
//! The callback is invoked on the event-tap thread and must finish quickly —
//! we just decode minimal data into a [`RawEvent`] and push it through a
//! `tokio::mpsc::Sender::try_send`. If the channel is full, the event is
//! dropped (a [`HealthEvent::DroppedEvent`] is published once per second by
//! the normalizer).

use std::thread::JoinHandle;

use anyhow::{anyhow, Result};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    CallbackResult, EventField,
};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Bit-mask of all event types we are interested in.
fn events_of_interest() -> Vec<CGEventType> {
    vec![
        CGEventType::KeyDown,
        CGEventType::FlagsChanged,
        CGEventType::LeftMouseDown,
        CGEventType::RightMouseDown,
        CGEventType::OtherMouseDown,
        CGEventType::ScrollWheel,
    ]
}

/// Minimal representation handed to the normalizer. `KeyDown.text` is
/// extracted in the event-tap callback via `CGEventKeyboardGetUnicodeString`
/// — see [`super::ffi`] for the rationale.
#[derive(Debug, Clone)]
pub enum RawEvent {
    KeyDown {
        keycode: u16,
        flags: u64,
        text: Option<String>,
    },
    FlagsChanged {
        keycode: u16,
        flags: u64,
    },
    MouseDown {
        button: MouseButton,
        x: f64,
        y: f64,
        flags: u64,
    },
    Scroll {
        delta_y: i64,
        delta_x: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Other,
}

/// Owns the event-tap thread + its CFRunLoop.
pub struct EventTap {
    runloop: Mutex<Option<CFRunLoop>>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl EventTap {
    /// Spawns the event-tap thread. Returns once the tap is installed and the
    /// runloop is running, or an error if installation fails (commonly:
    /// Input Monitoring permission missing).
    pub fn spawn(tx: mpsc::Sender<RawEvent>) -> Result<Self> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<CFRunLoop, String>>();

        let join = std::thread::Builder::new()
            .name("gilb-event-tap".into())
            .spawn(move || run_thread(tx, ready_tx))
            .map_err(|e| anyhow!("failed to spawn event-tap thread: {e}"))?;

        match ready_rx.recv() {
            Ok(Ok(runloop)) => Ok(Self {
                runloop: Mutex::new(Some(runloop)),
                join: Mutex::new(Some(join)),
            }),
            Ok(Err(msg)) => {
                let _ = join.join();
                Err(anyhow!(msg))
            }
            Err(e) => {
                let _ = join.join();
                Err(anyhow!("event-tap thread exited without signalling: {e}"))
            }
        }
    }

    /// Stop the event-tap and join the thread.
    pub fn stop(&self) {
        if let Some(rl) = self.runloop.lock().take() {
            rl.stop();
        }
        if let Some(handle) = self.join.lock().take() {
            let _ = handle.join();
        }
    }
}

impl Drop for EventTap {
    fn drop(&mut self) {
        self.stop();
    }
}

// SAFETY: CFRunLoop is internally thread-safe for `stop`, which is the only
// thing we call from outside its owning thread. The handle is also Send.
unsafe impl Send for EventTap {}
unsafe impl Sync for EventTap {}

fn run_thread(
    tx: mpsc::Sender<RawEvent>,
    ready_tx: std::sync::mpsc::Sender<Result<CFRunLoop, String>>,
) {
    let tap_result = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        events_of_interest(),
        move |_proxy, etype, event| {
            if let Some(raw) = decode_event(etype, event) {
                if let Err(err) = tx.try_send(raw) {
                    if matches!(err, mpsc::error::TrySendError::Full(_)) {
                        // Drop silently — normalizer publishes drop health.
                    } else {
                        warn!(?err, "event-tap: tx closed, dropping event");
                    }
                }
            }
            CallbackResult::Keep
        },
    );

    let tap = match tap_result {
        Ok(t) => t,
        Err(_) => {
            let _ = ready_tx.send(Err(
                "CGEventTap::new failed (Input Monitoring not granted?)".into(),
            ));
            return;
        }
    };

    // SAFETY: CFRunLoopSource lives as long as `loop_source`; we add it to the
    // current runloop and keep both alive while the runloop runs.
    let loop_source = match tap.mach_port().create_runloop_source(0) {
        Ok(src) => src,
        Err(_) => {
            let _ = ready_tx.send(Err("create_runloop_source failed".into()));
            return;
        }
    };

    let current_run_loop = CFRunLoop::get_current();
    unsafe {
        current_run_loop.add_source(&loop_source, kCFRunLoopCommonModes);
    }
    tap.enable();

    if ready_tx.send(Ok(current_run_loop.clone())).is_err() {
        error!("event-tap: parent dropped the ready channel; exiting");
        return;
    }
    info!("event-tap: runloop starting");
    CFRunLoop::run_current();
    debug!("event-tap: runloop exited");
}

fn decode_event(etype: CGEventType, event: &core_graphics::event::CGEvent) -> Option<RawEvent> {
    let flags = event.get_flags().bits();
    let raw = match etype {
        CGEventType::KeyDown => {
            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let text = extract_unicode(event);
            RawEvent::KeyDown {
                keycode,
                flags,
                text,
            }
        }
        CGEventType::FlagsChanged => {
            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            RawEvent::FlagsChanged { keycode, flags }
        }
        CGEventType::LeftMouseDown | CGEventType::RightMouseDown | CGEventType::OtherMouseDown => {
            let p = event.location();
            let button = match etype {
                CGEventType::LeftMouseDown => MouseButton::Left,
                CGEventType::RightMouseDown => MouseButton::Right,
                _ => MouseButton::Other,
            };
            RawEvent::MouseDown {
                button,
                x: p.x,
                y: p.y,
                flags,
            }
        }
        CGEventType::ScrollWheel => {
            let dy = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
            let dx = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2);
            RawEvent::Scroll {
                delta_y: dy,
                delta_x: dx,
            }
        }
        _ => return None,
    };
    Some(raw)
}

/// Read the Unicode characters this keyboard event represents (already
/// composed with the current layout + modifiers + dead-key state by the OS).
/// Returns `None` for events with no printable characters.
fn extract_unicode(event: &core_graphics::event::CGEvent) -> Option<String> {
    use foreign_types_shared::ForeignType;

    // 8 UniChars covers everything sane (emoji surrogate pairs included).
    let mut buf: [u16; 8] = [0; 8];
    let mut actual_len: libc::c_ulong = 0;
    let event_ref = event.as_ptr() as super::ffi::CGEventRef;

    // SAFETY: `event_ref` is non-null and lives for the duration of this
    // callback (CGEvent borrowed). The function writes at most `buf.len()`
    // UniChars and sets `actual_len`.
    unsafe {
        super::ffi::CGEventKeyboardGetUnicodeString(
            event_ref,
            buf.len() as libc::c_ulong,
            &mut actual_len,
            buf.as_mut_ptr(),
        );
    }

    if actual_len == 0 {
        return None;
    }
    let s = String::from_utf16_lossy(&buf[..actual_len as usize]);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
