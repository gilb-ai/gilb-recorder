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
    kAXErrorSuccess, kAXHelpAttribute, kAXIdentifierAttribute, kAXRoleAttribute,
    kAXTitleAttribute, kAXValueAttribute, AXError, AXUIElementCopyAttributeValue,
    AXUIElementCopyElementAtPosition, AXUIElementCreateSystemWide, AXUIElementRef,
    AXUIElementSetMessagingTimeout,
};
use core_foundation::base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType};
use core_foundation::string::{CFString, CFStringRef};
use crossbeam_channel as cc;
use parking_lot::Mutex;
use tracing::debug;

use gilb_core::ElementContext;

use super::focus::FocusState;

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
    pub fn submit(&self, job: AxJob) {
        if let Err(err) = self.tx.try_send(job) {
            debug!(?err, "ax-worker: queue full, dropping job");
        }
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

    debug!("ax-worker: ready");
    loop {
        cc::select! {
            recv(shutdown) -> _ => break,
            recv(rx) -> job => {
                let Ok(job) = job else { break };
                let ctx = match AX_QUERY_LOCK.try_lock() {
                    Some(_guard) => element_at(system, job.x, job.y, &focus),
                    None => {
                        debug!("ax-worker: query lock busy, dropping job");
                        None
                    }
                };
                let _ = job.reply.send(ctx);
            }
        }
    }

    // SAFETY: We created the system-wide ref; release it on shutdown.
    unsafe { CFRelease(system as CFTypeRef) };
    debug!("ax-worker: shut down");
}

fn element_at(
    system: AXUIElementRef,
    x: f64,
    y: f64,
    focus: &FocusState,
) -> Option<ElementContext> {
    // SAFETY: Apple's AX functions are documented thread-safe.
    let mut element: AXUIElementRef = ptr::null_mut();
    let res = unsafe {
        AXUIElementCopyElementAtPosition(system, x as f32, y as f32, &mut element)
    };
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

fn ax_attr_name(name: &str) -> CFString {
    CFString::new(name)
}

fn read_string_attr(element: AXUIElementRef, attr: CFString) -> Option<String> {
    let mut value: CFTypeRef = ptr::null_mut() as *const c_void;
    let res = unsafe {
        AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value)
    };
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
