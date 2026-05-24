//! Foreground-app introspection via `NSWorkspace.frontmostApplication`,
//! plus best-effort focused-window-title via AX.
//!
//! Called from the normalizer on every event — cheap (microseconds for
//! the NSWorkspace part; bounded by `AX_TITLE_TIMEOUT` for the AX part).

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::time::Duration;

use accessibility_sys::{
    kAXErrorSuccess, kAXFocusedWindowAttribute, kAXTitleAttribute, AXError,
    AXUIElementCopyAttributeValue, AXUIElementCreateApplication, AXUIElementRef,
    AXUIElementSetMessagingTimeout,
};
use arc_swap::ArcSwap;
use core_foundation::base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType};
use core_foundation::string::{CFString, CFStringRef};
use objc2::rc::Retained;
use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_foundation::NSString;

use gilb_core::AppInfo;

/// AX message timeout when reading the focused-window title. Kept short
/// because this runs in the normalizer's focus-poll tick — a hung AX
/// call here delays the whole tick.
const AX_TITLE_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Default, Clone)]
pub struct FocusSnapshot {
    pub app: AppInfo,
    /// AX role of the currently focused element, if it could be obtained.
    pub focused_role: Option<String>,
}

/// Cheap-clone holder of the latest focus snapshot, updated from the AX worker
/// and read on every event by the normalizer.
#[derive(Clone, Default)]
pub struct FocusState {
    inner: Arc<ArcSwap<FocusSnapshot>>,
}

impl FocusState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(FocusSnapshot::default())),
        }
    }

    pub fn current(&self) -> Arc<FocusSnapshot> {
        self.inner.load_full()
    }

    pub fn set(&self, snap: FocusSnapshot) {
        self.inner.store(Arc::new(snap));
    }

    pub fn update_app(&self, app: AppInfo) {
        let prev = self.inner.load_full();
        let next = FocusSnapshot {
            app,
            focused_role: prev.focused_role.clone(),
        };
        self.inner.store(Arc::new(next));
    }

    pub fn set_focused_role(&self, role: Option<String>) {
        let prev = self.inner.load_full();
        let next = FocusSnapshot {
            app: prev.app.clone(),
            focused_role: role,
        };
        self.inner.store(Arc::new(next));
    }
}

/// Look up the frontmost application via NSWorkspace. May be called from any
/// thread (Apple guarantees `frontmostApplication` is thread-safe).
/// Does NOT populate `window_title` — use [`frontmost_app_with_window`] when
/// the caller wants the active window's title as well.
pub fn frontmost_app() -> AppInfo {
    let workspace = NSWorkspace::sharedWorkspace();
    let Some(app) = workspace.frontmostApplication() else {
        return AppInfo::default();
    };
    AppInfo {
        bundle_id: ns_string_to_rust(app.bundleIdentifier().as_deref()),
        name: ns_string_to_rust(app.localizedName().as_deref()),
        pid: Some(app.processIdentifier()),
        window_title: None,
    }
}

/// Same as [`frontmost_app`] but additionally queries the focused window
/// title via AX. Used by the focus-poll tick so that a window change inside
/// the same app still surfaces as a `focus_change` event.
///
/// Bounded by `AX_TITLE_TIMEOUT` per call. Returns `window_title: None` if
/// the AX call times out, the process is unresponsive, or the focused
/// element doesn't expose a title.
pub fn frontmost_app_with_window() -> AppInfo {
    let workspace = NSWorkspace::sharedWorkspace();
    let Some(app) = workspace.frontmostApplication() else {
        return AppInfo::default();
    };
    let pid = app.processIdentifier();
    AppInfo {
        bundle_id: ns_string_to_rust(app.bundleIdentifier().as_deref()),
        name: ns_string_to_rust(app.localizedName().as_deref()),
        pid: Some(pid),
        window_title: focused_window_title(pid),
    }
}

/// Read AXFocusedWindow → AXTitle for the given pid. Single bounded AX call,
/// no shared state — safe to call from the normalizer tick.
fn focused_window_title(pid: i32) -> Option<String> {
    // SAFETY: AX functions are documented thread-safe; we own the AXUIElementRef
    // we create here and release it before returning.
    let app_elem: AXUIElementRef = unsafe { AXUIElementCreateApplication(pid) };
    if app_elem.is_null() {
        return None;
    }
    unsafe {
        let _ = AXUIElementSetMessagingTimeout(app_elem, AX_TITLE_TIMEOUT.as_secs_f32());
    }

    let window = copy_attr_ref(app_elem, kAXFocusedWindowAttribute);
    // SAFETY: AXUIElementCreateApplication returns +1 retain; release now.
    unsafe { CFRelease(app_elem as CFTypeRef) };
    let window = window?;

    unsafe {
        let _ = AXUIElementSetMessagingTimeout(window, AX_TITLE_TIMEOUT.as_secs_f32());
    }
    let title = read_string_attr(window, kAXTitleAttribute);
    // SAFETY: copy_attr_ref returned +1; release it.
    unsafe { CFRelease(window as CFTypeRef) };
    title
}

/// Copy an attribute that returns an AXUIElement (e.g. AXFocusedWindow).
/// Caller owns the returned reference and must release it.
fn copy_attr_ref(elem: AXUIElementRef, attr: &str) -> Option<AXUIElementRef> {
    let attr = CFString::new(attr);
    let mut value: CFTypeRef = ptr::null_mut() as *const c_void;
    let res = unsafe {
        AXUIElementCopyAttributeValue(elem, attr.as_concrete_TypeRef(), &mut value)
    };
    if res != kAXErrorSuccess as AXError || value.is_null() {
        return None;
    }
    Some(value as AXUIElementRef)
}

/// Read a CFString attribute. Mirrors the version in `ax_worker.rs` but kept
/// local so this module doesn't reach into worker internals.
fn read_string_attr(element: AXUIElementRef, attr: &str) -> Option<String> {
    let attr = CFString::new(attr);
    let mut value: CFTypeRef = ptr::null_mut() as *const c_void;
    let res = unsafe {
        AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value)
    };
    if res != kAXErrorSuccess as AXError || value.is_null() {
        return None;
    }
    let s = if unsafe { CFGetTypeID(value) } == CFString::type_id() {
        let cf_string_ref = value as CFStringRef;
        Some(unsafe { CFString::wrap_under_create_rule(cf_string_ref) }.to_string())
    } else {
        unsafe { CFRelease(value) };
        None
    };
    s.filter(|t| !t.is_empty())
}

#[allow(dead_code)]
fn first_running_app() -> Option<Retained<NSRunningApplication>> {
    NSWorkspace::sharedWorkspace().frontmostApplication()
}

fn ns_string_to_rust(s: Option<&NSString>) -> Option<String> {
    s.map(|ns| ns.to_string())
}
