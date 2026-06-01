//! Platform-neutral classification of non-printable navigation/editing keys.
//!
//! Printable characters come straight out of the platform source already
//! composed with the active layout, so there is no layout-aware decoder here.
//! The mapping from a native keycode to [`SpecialKey`] is platform-specific
//! and lives next to each capture source (e.g.
//! `platform::macos::keyboard::special_key_from_macos_keycode`).

/// Special, non-printable navigation/editing keys that the TextBuffer treats
/// as flush triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialKey {
    Return,
    Tab,
    Escape,
    Backspace,
    Delete,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Home,
    End,
    PageUp,
    PageDown,
    Other,
}

impl SpecialKey {
    pub fn is_navigation(self) -> bool {
        matches!(
            self,
            SpecialKey::Return
                | SpecialKey::Tab
                | SpecialKey::ArrowLeft
                | SpecialKey::ArrowRight
                | SpecialKey::ArrowUp
                | SpecialKey::ArrowDown
                | SpecialKey::Home
                | SpecialKey::End
                | SpecialKey::PageUp
                | SpecialKey::PageDown
                | SpecialKey::Escape
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_keys_flush_buffer() {
        assert!(SpecialKey::ArrowLeft.is_navigation());
        assert!(SpecialKey::Return.is_navigation());
        assert!(!SpecialKey::Backspace.is_navigation());
    }
}
