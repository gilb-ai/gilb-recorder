//! Dedicated thread that runs AX queries on demand.
//!
//! AX calls are blocking and may freeze for hundreds of ms when the target
//! process is unresponsive. We hide that latency behind:
//!
//! - a bounded(4) crossbeam channel of jobs — if full, callers drop the job
//!   (the event itself is still recorded, just without element context);
//! - an `AX_QUERY_LOCK` `try_lock` — if a query is already in flight, the new
//!   job is dropped to keep latency bounded;
//! - a hard timeout via [`AXUIElementSetMessagingTimeout`] equivalent (we set
//!   it on the system-wide element once at startup).

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use accessibility_sys::{
    kAXErrorSuccess, kAXHelpAttribute, kAXIdentifierAttribute, kAXRoleAttribute, kAXTitleAttribute,
    kAXValueAttribute, AXError, AXUIElementCopyAttributeValue, AXUIElementCopyElementAtPosition,
    AXUIElementCreateSystemWide, AXUIElementRef, AXUIElementSetMessagingTimeout,
};
use core_foundation::base::{CFGetTypeID, CFRelease, CFType, CFTypeRef, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::window::{
    copy_window_info, kCGNullWindowID, kCGWindowBounds, kCGWindowListOptionAll, kCGWindowOwnerPID,
};
use crossbeam_channel as cc;
use parking_lot::Mutex;
use tracing::{debug, warn};

use gilb_core::ElementContext;

use crate::focus::FocusState;
use crate::ElementResolver;

pub const QUEUE_CAPACITY: usize = 4;
pub const QUERY_TIMEOUT: Duration = Duration::from_millis(150);

/// One AX query request. The reply channel may already be dropped when we
/// finish — that's fine, the consumer just decided to give up.
pub struct AxJob {
    pub x: f64,
    pub y: f64,
    pub reply: tokio::sync::oneshot::Sender<Option<ElementContext>>,
}

#[derive(Clone)]
pub struct AxWorker {
    tx: cc::Sender<AxJob>,
}

pub struct AxWorkerHandle {
    join: Option<JoinHandle<()>>,
    shutdown: cc::Sender<()>,
}

impl AxWorker {
    /// Spawn the worker thread. Returns the worker handle (clone-able sender)
    /// and a shutdown handle.
    pub fn spawn(focus: FocusState) -> (AxWorker, AxWorkerHandle) {
        let (tx, rx) = cc::bounded::<AxJob>(QUEUE_CAPACITY);
        let (shutdown_tx, shutdown_rx) = cc::bounded::<()>(1);

        // If the OS refuses to spawn this thread the capture pipeline
        // cannot function — there is no useful degraded mode. Panic at
        // init rather than carry a None handle through the engine.
        let join = std::thread::Builder::new()
            .name("gilb-ax-worker".into())
            .spawn(move || run_loop(rx, shutdown_rx, focus))
            .expect("spawn gilb-ax-worker");

        (
            AxWorker { tx },
            AxWorkerHandle {
                join: Some(join),
                shutdown: shutdown_tx,
            },
        )
    }

    /// Best-effort enqueue. Drops the request if the channel is full.
    pub fn enqueue(&self, job: AxJob) {
        if let Err(err) = self.tx.try_send(job) {
            debug!(?err, "queue full, dropping job");
        }
    }
}

impl ElementResolver for AxWorker {
    fn submit(&self, x: f64, y: f64, reply: tokio::sync::oneshot::Sender<Option<ElementContext>>) {
        self.enqueue(AxJob { x, y, reply });
    }
}

impl Drop for AxWorkerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

static AX_QUERY_LOCK: Mutex<()> = Mutex::new(());

fn run_loop(rx: cc::Receiver<AxJob>, shutdown: cc::Receiver<()>, focus: FocusState) {
    // SAFETY: Create the system-wide AX element once and set a hard timeout
    // so calls cannot hang the worker.
    let system = unsafe { AXUIElementCreateSystemWide() };
    unsafe {
        let _ = AXUIElementSetMessagingTimeout(system, QUERY_TIMEOUT.as_secs_f32());
    }

    debug!("ready");
    loop {
        cc::select! {
            recv(shutdown) -> _ => break,
            recv(rx) -> job => {
                let Ok(job) = job else { break };
                let ctx = match AX_QUERY_LOCK.try_lock() {
                    Some(_guard) => element_at(system, job.x, job.y, &focus),
                    None => {
                        debug!("query lock busy, dropping job");
                        None
                    }
                };
                let _ = job.reply.send(ctx);
            }
        }
    }

    // SAFETY: We created the system-wide ref; release it on shutdown.
    unsafe { CFRelease(system as CFTypeRef) };
    debug!("shut down");
}

fn element_at(
    system: AXUIElementRef,
    x: f64,
    y: f64,
    focus: &FocusState,
) -> Option<ElementContext> {
    // An AX hit-test is a cross-process call serviced on the *target* app's
    // main thread — EXCEPT when the point lands on our OWN UI (the tray status
    // item or our window). Then AppKit short-circuits and runs
    // `accessibilityHitTest:` in-process on THIS worker thread; NSAccessibility
    // is main-thread-only and corrupts memory (observed: a pointer-auth trap
    // inside CFDictionarySetValue when the cursor is over the menu-bar status
    // item). Skip points over our own windows — we lose only the element
    // context for clicks on our own chrome; cross-process queries are unaffected.
    // Diagnostic: the self-window guard enumerates all windows via CoreGraphics
    // on every hit-test. Warn if that turns expensive (it shouldn't), so a
    // perf regression here is visible rather than a silent drag on capture.
    let guard_started = std::time::Instant::now();
    let over_own = point_over_own_window(x, y);
    let guard_ms = guard_started.elapsed().as_millis() as u64;
    if guard_ms > 30 {
        warn!(guard_ms, "slow self-window check (CGWindowList)");
    }
    if over_own {
        return None;
    }

    let mut element: AXUIElementRef = ptr::null_mut();
    let res = unsafe { AXUIElementCopyElementAtPosition(system, x as f32, y as f32, &mut element) };
    if res != kAXErrorSuccess as AXError || element.is_null() {
        return None;
    }

    let role = read_string_attr(element, ax_attr_name(kAXRoleAttribute));
    let name = read_string_attr(element, ax_attr_name(kAXTitleAttribute));
    let value = read_string_attr(element, ax_attr_name(kAXValueAttribute));
    let help = read_string_attr(element, ax_attr_name(kAXHelpAttribute));
    let identifier = read_string_attr(element, ax_attr_name(kAXIdentifierAttribute));

    // Update the focused-role hint for the next text-flush decision.
    if let Some(r) = role.as_deref() {
        focus.set_focused_role(Some(r.to_string()));
    } else {
        focus.set_focused_role(None);
    }

    // SAFETY: AXUIElementCopyElementAtPosition gives us +1 retain; release it.
    unsafe { CFRelease(element as CFTypeRef) };

    Some(ElementContext {
        role,
        name,
        value,
        help,
        identifier,
        frame: None,
    })
}

/// True when the global point `(x, y)` lies over a window owned by our own
/// process. Uses CoreGraphics window enumeration only (no AppKit), so it is
/// safe on the worker thread — unlike the AX hit-test it guards.
///
/// Conservative: if one of our windows underlaps the point we return `true`
/// even when another app's window is in front. That costs only an occasional
/// dropped element-context sample, never a recorded event; the dangerous case
/// (our window actually on top, e.g. the tray status item) is always caught.
fn point_over_own_window(x: f64, y: f64) -> bool {
    let our_pid = std::process::id() as i64;
    // `All` (not OnScreenOnly): AppKit short-circuits the AX hit-test based on
    // the point belonging to our process, not on "is on screen". A status item
    // or panel that is appearing / momentarily off the on-screen list would
    // still route in-process, so consider every window we own.
    let Some(windows) = copy_window_info(kCGWindowListOptionAll, kCGNullWindowID) else {
        return false;
    };
    // SAFETY: reading the framework's CFString key globals.
    let pid_key = unsafe { CFString::wrap_under_get_rule(kCGWindowOwnerPID) };
    let bounds_key = unsafe { CFString::wrap_under_get_rule(kCGWindowBounds) };

    for item in windows.iter() {
        // SAFETY: each array element is a CFDictionary; borrow it (get rule) so
        // the wrapper balances the array's retain on drop.
        let dict = unsafe {
            CFDictionary::<CFString, CFType>::wrap_under_get_rule(*item as CFDictionaryRef)
        };

        let owner = dict
            .find(&pid_key)
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_i64());
        if owner != Some(our_pid) {
            continue;
        }

        let Some(bounds) = dict.find(&bounds_key) else {
            continue;
        };
        // kCGWindowBounds is documented to always be a CFDictionary, but verify
        // before the raw cast rather than trust the invariant.
        if !bounds.instance_of::<CFDictionary>() {
            continue;
        }
        // SAFETY: type-checked above; its keys (X/Y/Width/Height) are CFStrings.
        let bounds = unsafe {
            CFDictionary::<CFString, CFType>::wrap_under_get_rule(
                bounds.as_CFTypeRef() as CFDictionaryRef
            )
        };
        if rect_contains(&bounds, x, y) {
            return true;
        }
    }
    false
}

/// Point-in-rect against a `kCGWindowBounds` dictionary. CGWindow bounds use a
/// top-left origin — the same space as the CGEvent location the event tap hands
/// us — so no flipping is needed.
fn rect_contains(bounds: &CFDictionary<CFString, CFType>, x: f64, y: f64) -> bool {
    let read = |key: &'static str| -> Option<f64> {
        bounds
            .find(CFString::from_static_string(key))
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_f64())
    };
    match (read("X"), read("Y"), read("Width"), read("Height")) {
        (Some(bx), Some(by), Some(bw), Some(bh)) => {
            x >= bx && x < bx + bw && y >= by && y < by + bh
        }
        _ => false,
    }
}

fn ax_attr_name(name: &str) -> CFString {
    CFString::new(name)
}

fn read_string_attr(element: AXUIElementRef, attr: CFString) -> Option<String> {
    let mut value: CFTypeRef = ptr::null_mut() as *const c_void;
    let res =
        unsafe { AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value) };
    if res != kAXErrorSuccess as AXError || value.is_null() {
        return None;
    }

    // `kAXValueAttribute` (and a couple of others) can return CFNumber /
    // CFBoolean / CFData / CFArray depending on the element kind (slider,
    // checkbox, etc.). Treating those as CFString trips an NSException in
    // CF internals, which propagates as a foreign exception through Rust
    // frames and aborts the process. Type-check before wrapping.
    let s = if unsafe { CFGetTypeID(value) } == CFString::type_id() {
        let cf_string_ref = value as CFStringRef;
        Some(unsafe { CFString::wrap_under_create_rule(cf_string_ref) }.to_string())
    } else {
        unsafe { CFRelease(value) };
        None
    };

    s.filter(|t| !t.is_empty())
}

// SAFETY: `AxWorker` only holds a `crossbeam` sender which is Send + Sync.
unsafe impl Send for AxWorker {}
unsafe impl Sync for AxWorker {}

#[allow(dead_code)]
struct _AssertSendSync(Arc<AxWorker>);
