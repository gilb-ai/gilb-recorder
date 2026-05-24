//! Walk the AX tree of the focused window of a process.
//!
//! Adapted from prior-art/src/tree/macos.rs `walk_element`. Hard
//! caps on depth / node count / per-element timeout so a runaway tree
//! can never freeze the snapshotter for more than the worst-case budget.
//!
//! Output is a single JSON blob (Vec<Node>) — the analyzer parses on
//! demand. We deliberately do NOT normalise one-row-per-element like
//! prior-art; the trust-architecture comparison shows the blob form
//! gives the same useful signal at a fraction of the row count.

use std::ffi::c_void;
use std::ptr;
use std::time::{Duration, Instant};

use accessibility_sys::{
    kAXChildrenAttribute, kAXErrorSuccess, kAXFocusedWindowAttribute, kAXRoleAttribute,
    kAXTitleAttribute, kAXValueAttribute, AXError, AXUIElementCopyAttributeValue,
    AXUIElementCreateApplication, AXUIElementRef, AXUIElementSetMessagingTimeout,
};
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType};
use core_foundation::string::{CFString, CFStringRef};
use serde::{Deserialize, Serialize};

const MAX_DEPTH: usize = 12;
const MAX_NODES: usize = 800;
const PER_ELEMENT_TIMEOUT: Duration = Duration::from_millis(100);
const TOTAL_BUDGET: Duration = Duration::from_millis(800);

/// One node in the serialised tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub role: String,
    /// AXTitle. Absent for elements with no title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// AXValue, only when it's a non-empty CFString (TextField contents etc).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub depth: u8,
}

/// Walk the focused window of `pid` and return its tree. Returns
/// `None` if the AX session is unavailable, the focused window can't
/// be read, or the walk hits the total budget before producing any
/// useful nodes.
pub fn walk_focused_window(pid: i32) -> Option<Vec<Node>> {
    // SAFETY: AX functions are documented thread-safe; we own the
    // AXUIElementRefs created here and release them before returning.
    let app: AXUIElementRef = unsafe { AXUIElementCreateApplication(pid) };
    if app.is_null() {
        return None;
    }
    unsafe {
        let _ = AXUIElementSetMessagingTimeout(app, PER_ELEMENT_TIMEOUT.as_secs_f32());
    }

    let window = copy_attr_ref(app, kAXFocusedWindowAttribute);
    // SAFETY: AXUIElementCreateApplication returns +1; release.
    unsafe { CFRelease(app as CFTypeRef) };

    let Some(window) = window else { return None };
    unsafe {
        let _ = AXUIElementSetMessagingTimeout(window, PER_ELEMENT_TIMEOUT.as_secs_f32());
    }

    let mut state = WalkState {
        nodes: Vec::with_capacity(64),
        deadline: Instant::now() + TOTAL_BUDGET,
    };
    walk(window, 0, &mut state);

    // SAFETY: copy_attr_ref returned +1; release.
    unsafe { CFRelease(window as CFTypeRef) };

    if state.nodes.is_empty() {
        None
    } else {
        Some(state.nodes)
    }
}

struct WalkState {
    nodes: Vec<Node>,
    deadline: Instant,
}

impl WalkState {
    fn should_stop(&self) -> bool {
        self.nodes.len() >= MAX_NODES || Instant::now() >= self.deadline
    }
}

fn walk(elem: AXUIElementRef, depth: usize, state: &mut WalkState) {
    if depth >= MAX_DEPTH || state.should_stop() {
        return;
    }

    let role = match read_string_attr(elem, kAXRoleAttribute) {
        Some(r) => r,
        None => return, // not a real element — skip silently
    };

    // Cheap skip-list. Mirrors prior-art's should_skip_role minimum.
    if matches!(
        role.as_str(),
        "AXUnknown" | "AXImage" | "AXSeparator" | "AXBrowser"
    ) {
        return;
    }

    let name = read_string_attr(elem, kAXTitleAttribute);
    let value = read_string_attr(elem, kAXValueAttribute);

    state.nodes.push(Node {
        role: role.clone(),
        name,
        value,
        depth: depth as u8,
    });

    // Recurse.
    if let Some(children) = copy_children(elem) {
        for i in 0..children.len() {
            if state.should_stop() {
                break;
            }
            let child = match children.get(i) {
                Some(c) => *c as AXUIElementRef,
                None => continue,
            };
            // SAFETY: items inside the CFArray are +0; do NOT release.
            unsafe {
                let _ = AXUIElementSetMessagingTimeout(child, PER_ELEMENT_TIMEOUT.as_secs_f32());
            }
            walk(child, depth + 1, state);
        }
        // children goes out of scope here, releasing the CFArray (+1).
    }
}

/// Token stream for [`super::cache::simhash`] over a Node list.
pub fn tokens_for_simhash(nodes: &[Node]) -> Vec<String> {
    nodes
        .iter()
        .map(|n| {
            let text = n.name.as_deref().or(n.value.as_deref()).unwrap_or("");
            format!("{}:{}", n.role, text)
        })
        .collect()
}

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

fn copy_children(elem: AXUIElementRef) -> Option<CFArray<*const c_void>> {
    let attr = CFString::new(kAXChildrenAttribute);
    let mut value: CFTypeRef = ptr::null_mut() as *const c_void;
    let res =
        unsafe { AXUIElementCopyAttributeValue(elem, attr.as_concrete_TypeRef(), &mut value) };
    if res != kAXErrorSuccess as AXError || value.is_null() {
        return None;
    }
    Some(unsafe { CFArray::<*const c_void>::wrap_under_create_rule(value as CFArrayRef) })
}

fn read_string_attr(elem: AXUIElementRef, attr: &str) -> Option<String> {
    let attr = CFString::new(attr);
    let mut value: CFTypeRef = ptr::null_mut() as *const c_void;
    let res =
        unsafe { AXUIElementCopyAttributeValue(elem, attr.as_concrete_TypeRef(), &mut value) };
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
