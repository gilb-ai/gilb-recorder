//! macOS keycode → [`SpecialKey`] classification.
//!
//! Printable characters (RU/EN/composite/dead-key compositions) come straight
//! out of `CGEventKeyboardGetUnicodeString` in
//! [`super::event_tap::extract_unicode`] — the OS already composes them with
//! the current layout, so we don't need a layer-aware decoder here.

use crate::keyboard::SpecialKey;

/// Carbon `keyCode` → optional special-key classification.
/// Values from `Carbon/HIToolbox/Events.h`.
pub fn special_key_from_macos_keycode(code: u16) -> Option<SpecialKey> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_key_table_covers_basics() {
        assert_eq!(
            special_key_from_macos_keycode(0x24),
            Some(SpecialKey::Return)
        );
        assert_eq!(special_key_from_macos_keycode(0x30), Some(SpecialKey::Tab));
        assert_eq!(
            special_key_from_macos_keycode(0x33),
            Some(SpecialKey::Backspace)
        );
        assert_eq!(
            special_key_from_macos_keycode(0x7B),
            Some(SpecialKey::ArrowLeft)
        );
        assert_eq!(special_key_from_macos_keycode(0xFF), None);
    }
}
