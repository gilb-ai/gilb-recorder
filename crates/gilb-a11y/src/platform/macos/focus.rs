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
    kAXChildrenAttribute, kAXDocumentAttribute, kAXErrorSuccess, kAXFocusedWindowAttribute,
    kAXRoleAttribute, kAXTitleAttribute, kAXValueAttribute, AXError, AXUIElementCopyAttributeValue,
    AXUIElementCreateApplication, AXUIElementRef, AXUIElementSetMessagingTimeout,
};
use arc_swap::ArcSwap;
use core_foundation::array::{CFArray, CFArrayRef};
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

/// Max depth for the URL-bar walk fallback. Browsers expose the address
/// bar within 3-5 levels in practice; bounding it keeps the worst-case
/// cost predictable on pages with large DOMs.
const URL_WALK_MAX_DEPTH: usize = 5;

/// Lowercase substrings of `localizedName` we treat as browsers for the
/// purpose of URL extraction. Order is just for readability.
const BROWSER_NAMES: &[&str] = &[
    "chrome",
    "google chrome",
    "safari",
    "firefox",
    "edge",
    "microsoft edge",
    "brave",
    "brave browser",
    "arc",
    "chromium",
    "vivaldi",
    "opera",
    "zen",
    "comet",
];

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
        browser_url: None,
    }
}

/// Same as [`frontmost_app`] but additionally queries the focused window
/// title via AX, and — when the foreground app is a browser — the URL of
/// the active tab. Used by the focus-poll tick so that a window change
/// inside the same app still surfaces as a `focus_change` event, and so
/// every event we emit while a browser is foreground carries the URL.
///
/// Bounded by `AX_TITLE_TIMEOUT` per AX call. Returns `None` for
/// `window_title` / `browser_url` on timeout or when the attribute isn't
/// exposed.
pub fn frontmost_app_with_window() -> AppInfo {
    let workspace = NSWorkspace::sharedWorkspace();
    let Some(app) = workspace.frontmostApplication() else {
        return AppInfo::default();
    };
    let pid = app.processIdentifier();
    let name = ns_string_to_rust(app.localizedName().as_deref());
    let bundle_id = ns_string_to_rust(app.bundleIdentifier().as_deref());

    let (window_title, browser_url) = focused_window_and_url(pid, name.as_deref());

    AppInfo {
        bundle_id,
        name,
        pid: Some(pid),
        window_title,
        browser_url,
    }
}

/// Read AXFocusedWindow once, then extract title and (for browsers) URL
/// from the same handle so we don't pay the AX-create cost twice.
fn focused_window_and_url(pid: i32, app_name: Option<&str>) -> (Option<String>, Option<String>) {
    // SAFETY: AX functions are documented thread-safe; we own the
    // AXUIElementRefs created here and release them before returning.
    let app_elem: AXUIElementRef = unsafe { AXUIElementCreateApplication(pid) };
    if app_elem.is_null() {
        return (None, None);
    }
    unsafe {
        let _ = AXUIElementSetMessagingTimeout(app_elem, AX_TITLE_TIMEOUT.as_secs_f32());
    }

    let window = copy_attr_ref(app_elem, kAXFocusedWindowAttribute);
    // SAFETY: AXUIElementCreateApplication returns +1 retain; release now.
    unsafe { CFRelease(app_elem as CFTypeRef) };

    let Some(window) = window else {
        return (None, None);
    };

    unsafe {
        let _ = AXUIElementSetMessagingTimeout(window, AX_TITLE_TIMEOUT.as_secs_f32());
    }
    let title = read_string_attr(window, kAXTitleAttribute);
    let url = if is_browser_app(app_name) {
        extract_browser_url(window)
    } else {
        None
    };

    // SAFETY: copy_attr_ref returned +1; release it.
    unsafe { CFRelease(window as CFTypeRef) };
    (title, url)
}

fn is_browser_app(name: Option<&str>) -> bool {
    let Some(name) = name else { return false };
    let lower = name.to_lowercase();
    BROWSER_NAMES.iter().any(|b| lower.contains(b))
}

/// Three-tier URL extraction from a focused browser window.
///
/// Mirrors prior-art/src/tree/macos.rs `extract_browser_url`:
///
/// 1. `AXDocument` on the window — works for Safari, Chrome (the value
///    is the active tab's URL when not navigated to a local file).
/// 2. (TODO) Arc — needs `osascript`; deferred until we have a Therblig
///    that depends on it.
/// 3. Shallow walk for an `AXTextField` / `AXComboBox` whose value
///    `looks_like_url`. Catches Firefox / Brave / Edge / Vivaldi /
///    most chromium-derivatives.
fn extract_browser_url(window: AXUIElementRef) -> Option<String> {
    // Tier 1: AXDocument
    if let Some(doc) = read_string_attr(window, kAXDocumentAttribute) {
        if looks_like_url(&doc) {
            return Some(doc);
        }
    }

    // Tier 3: shallow recursive walk for an address-bar-looking element
    find_url_in_walk(window, 0)
}

fn find_url_in_walk(elem: AXUIElementRef, depth: usize) -> Option<String> {
    if depth >= URL_WALK_MAX_DEPTH {
        return None;
    }

    // Inspect this element itself if it's an AXTextField / AXComboBox.
    if let Some(role) = read_string_attr(elem, kAXRoleAttribute) {
        if role == "AXTextField" || role == "AXComboBox" {
            if let Some(value) = read_string_attr(elem, kAXValueAttribute) {
                if looks_like_url(&value) {
                    return Some(value);
                }
            }
        }
    }

    // Recurse into children.
    let children = copy_children(elem)?;
    for i in 0..children.len() {
        let child = unsafe { *children.get(i)? };
        // SAFETY: items returned by CFArray are +0 (owned by the array).
        // We only borrow; do NOT release `child` here.
        unsafe {
            let _ = AXUIElementSetMessagingTimeout(
                child as AXUIElementRef,
                AX_TITLE_TIMEOUT.as_secs_f32(),
            );
        }
        if let Some(url) = find_url_in_walk(child as AXUIElementRef, depth + 1) {
            return Some(url);
        }
    }
    None
}

/// Read `kAXChildrenAttribute` as a CFArray of AXUIElementRefs.
/// The returned CFArray owns the references; callers must NOT release the
/// individual children, only the array (via `wrap_under_create_rule`).
fn copy_children(elem: AXUIElementRef) -> Option<CFArray<*const c_void>> {
    let attr = CFString::new(kAXChildrenAttribute);
    let mut value: CFTypeRef = ptr::null_mut() as *const c_void;
    let res =
        unsafe { AXUIElementCopyAttributeValue(elem, attr.as_concrete_TypeRef(), &mut value) };
    if res != kAXErrorSuccess as AXError || value.is_null() {
        return None;
    }
    // SAFETY: AXChildren returns a CFArray when present; we own +1.
    Some(unsafe { CFArray::<*const c_void>::wrap_under_create_rule(value as CFArrayRef) })
}

fn looks_like_url(s: &str) -> bool {
    let t = s.trim();
    t.starts_with("http://") || t.starts_with("https://")
}

/// Copy an attribute that returns an AXUIElement (e.g. AXFocusedWindow).
/// Caller owns the returned reference and must release it.
fn copy_attr_ref(elem: AXUIElementRef, attr: &str) -> Option<AXUIElementRef> {
    let attr = CFString::new(attr);
    let mut value: CFTypeRef = ptr::null_mut() as *const c_void;
    let res =
        unsafe { AXUIElementCopyAttributeValue(elem, attr.as_concrete_TypeRef(), &mut value) };
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
    let res =
        unsafe { AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value) };
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
