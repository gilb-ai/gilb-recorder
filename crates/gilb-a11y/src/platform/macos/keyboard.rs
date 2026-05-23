//! Classification of non-printable navigation/editing keys.
//!
//! Printable characters (RU/EN/composite/dead-key compositions) come straight
//! out of `CGEventKeyboardGetUnicodeString` in
//! [`super::event_tap::extract_unicode`] — the OS already composes them with
//! the current layout, so we don't need a layer-aware decoder here.

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
    /// `keyCode` → optional special-key classification.
    pub fn from_keycode(code: u16) -> Option<Self> {
        // Values from `Carbon/HIToolbox/Events.h`.
        Some(match code {
            0x24 => SpecialKey::Return, // kVK_Return
            0x4C => SpecialKey::Return, // kVK_ANSI_KeypadEnter
            0x30 => SpecialKey::Tab,
            0x35 => SpecialKey::Escape,
            0x33 => SpecialKey::Backspace,
            0x75 => SpecialKey::Delete,
            0x7B => SpecialKey::ArrowLeft,
            0x7C => SpecialKey::ArrowRight,
            0x7E => SpecialKey::ArrowUp,
            0x7D => SpecialKey::ArrowDown,
            0x73 => SpecialKey::Home,
            0x77 => SpecialKey::End,
            0x74 => SpecialKey::PageUp,
            0x79 => SpecialKey::PageDown,
            _ => return None,
        })
    }

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
    fn special_key_table_covers_basics() {
        assert_eq!(SpecialKey::from_keycode(0x24), Some(SpecialKey::Return));
        assert_eq!(SpecialKey::from_keycode(0x30), Some(SpecialKey::Tab));
        assert_eq!(SpecialKey::from_keycode(0x33), Some(SpecialKey::Backspace));
        assert_eq!(SpecialKey::from_keycode(0x7B), Some(SpecialKey::ArrowLeft));
        assert_eq!(SpecialKey::from_keycode(0xFF), None);
    }

    #[test]
    fn navigation_keys_flush_buffer() {
        assert!(SpecialKey::ArrowLeft.is_navigation());
        assert!(SpecialKey::Return.is_navigation());
        assert!(!SpecialKey::Backspace.is_navigation());
    }
}
