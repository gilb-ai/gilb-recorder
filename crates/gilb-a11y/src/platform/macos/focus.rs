//! Foreground-app introspection via `NSWorkspace.frontmostApplication`.
//!
//! Called from the normalizer on every event — cheap (microseconds).

use std::sync::Arc;

use arc_swap::ArcSwap;
use objc2::rc::Retained;
use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_foundation::NSString;

use gilb_core::AppInfo;

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

#[allow(dead_code)]
fn first_running_app() -> Option<Retained<NSRunningApplication>> {
    NSWorkspace::sharedWorkspace().frontmostApplication()
}

fn ns_string_to_rust(s: Option<&NSString>) -> Option<String> {
    s.map(|ns| ns.to_string())
}
