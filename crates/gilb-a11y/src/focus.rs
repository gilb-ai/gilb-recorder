//! Platform-neutral foreground-focus state.
//!
//! [`FocusState`] is a cheap-clone holder of the latest [`FocusSnapshot`],
//! written by a platform focus source (and, on click, by the element
//! resolver) and read on every event by the normalizer. The actual lookup of
//! the frontmost app lives behind the [`crate::FocusProvider`] trait.

use std::sync::Arc;

use arc_swap::ArcSwap;

use gilb_core::AppInfo;

#[derive(Debug, Default, Clone)]
pub struct FocusSnapshot {
    pub app: AppInfo,
    /// Accessibility role of the currently focused element, if known.
    pub focused_role: Option<String>,
    /// `true` when the focused element is a secure/password field. Set
    /// directly by sources that expose it (Windows UIA `IsPassword`); on
    /// macOS this stays `false` and secure-ness is derived from
    /// `focused_role` instead.
    pub focused_secure: bool,
}

/// Cheap-clone holder of the latest focus snapshot.
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
            focused_secure: prev.focused_secure,
        };
        self.inner.store(Arc::new(next));
    }

    pub fn set_focused_role(&self, role: Option<String>) {
        let prev = self.inner.load_full();
        let next = FocusSnapshot {
            app: prev.app.clone(),
            focused_role: role,
            focused_secure: prev.focused_secure,
        };
        self.inner.store(Arc::new(next));
    }

    pub fn set_focused_secure(&self, secure: bool) {
        let prev = self.inner.load_full();
        let next = FocusSnapshot {
            app: prev.app.clone(),
            focused_role: prev.focused_role.clone(),
            focused_secure: secure,
        };
        self.inner.store(Arc::new(next));
    }
}
