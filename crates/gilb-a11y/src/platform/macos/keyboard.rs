//! Translate raw `keyCode`+`modifiers` events into Unicode strings using
//! `UCKeyTranslate` with persistent dead-key state.
//!
//! This is a *best-effort* layer-aware decoder. It honors the user's current
//! keyboard input source (RU, EN, etc.) and survives dead-key composition.

use core_foundation::base::TCFType;
use core_foundation::data::{CFData, CFDataRef};
use parking_lot::Mutex;

use super::ffi;

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
            0x24 => SpecialKey::Return,            // kVK_Return
            0x4C => SpecialKey::Return,            // kVK_ANSI_KeypadEnter
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

/// Lazily-initialised `UCKeyTranslate` decoder with per-call dead-key state.
pub struct KeyboardDecoder {
    dead_state: Mutex<u32>,
}

impl KeyboardDecoder {
    pub fn new() -> Self {
        Self {
            dead_state: Mutex::new(0),
        }
    }

    /// Translate `keyCode` + modifier flags into a string of characters that
    /// the user actually typed (may be empty for dead keys awaiting a follow-up
    /// keystroke).
    pub fn translate(&self, keycode: u16, modifier_flags: u32) -> String {
        // SAFETY: All FFI calls below take owned/static CF objects. We always
        // release the input source via `CFRelease` after use.
        unsafe {
            let source = ffi::TISCopyCurrentKeyboardLayoutInputSource();
            if source.is_null() {
                return String::new();
            }
            // `kTISPropertyUnicodeKeyLayoutData` is a global CFStringRef.
            let layout_data_ref = ffi::TISGetInputSourceProperty(
                source,
                ffi::kTISPropertyUnicodeKeyLayoutData,
            ) as CFDataRef;
            let result = if layout_data_ref.is_null() {
                // Some sources (e.g. handwriting) don't ship Unicode layouts —
                // fall back to ASCII-capable source.
                let ascii_source = ffi::TISCopyCurrentASCIICapableKeyboardLayoutInputSource();
                if ascii_source.is_null() {
                    String::new()
                } else {
                    let ascii_layout_ref = ffi::TISGetInputSourceProperty(
                        ascii_source,
                        ffi::kTISPropertyUnicodeKeyLayoutData,
                    ) as CFDataRef;
                    let s = if ascii_layout_ref.is_null() {
                        String::new()
                    } else {
                        self.translate_with_layout(ascii_layout_ref, keycode, modifier_flags)
                    };
                    core_foundation::base::CFRelease(ascii_source as _);
                    s
                }
            } else {
                self.translate_with_layout(layout_data_ref, keycode, modifier_flags)
            };

            core_foundation::base::CFRelease(source as _);
            result
        }
    }

    unsafe fn translate_with_layout(
        &self,
        layout_data_ref: CFDataRef,
        keycode: u16,
        modifier_flags: u32,
    ) -> String {
        let layout = CFData::wrap_under_get_rule(layout_data_ref);
        let layout_ptr = layout.bytes().as_ptr();

        let kbd_type = ffi::LMGetKbdType() as u32;
        // `UCKeyTranslate` wants Carbon-style modifier bits in the high byte.
        // Mac CGEventFlags use bits 16..23 for shift/ctrl/option/cmd. Convert.
        // Bit definitions (NSEventModifierFlags):
        //   shift  = 1<<17,  ctrl = 1<<18,  option = 1<<19,  cmd = 1<<20.
        // Carbon modifiers expected by UCKeyTranslate (shifted right by 8):
        //   shift  = 1<<9,   ctrl = 1<<12, option = 1<<11, cmd = 1<<8.
        let mut carbon_mods: u32 = 0;
        if modifier_flags & (1 << 17) != 0 {
            carbon_mods |= 1 << 9;
        }
        if modifier_flags & (1 << 18) != 0 {
            carbon_mods |= 1 << 12;
        }
        if modifier_flags & (1 << 19) != 0 {
            carbon_mods |= 1 << 11;
        }
        if modifier_flags & (1 << 20) != 0 {
            carbon_mods |= 1 << 8;
        }
        let carbon_mod_state = (carbon_mods >> 8) & 0xFF;

        let mut unicode: [u16; 8] = [0; 8];
        let mut actual_len: libc::c_ulong = 0;
        let mut dead_state = self.dead_state.lock();

        let status = ffi::UCKeyTranslate(
            layout_ptr,
            keycode,
            ffi::kUCKeyActionDown,
            carbon_mod_state,
            kbd_type,
            ffi::kUCKeyTranslateNoDeadKeysBit,
            &mut *dead_state,
            unicode.len() as libc::c_ulong,
            &mut actual_len,
            unicode.as_mut_ptr(),
        );
        if status != 0 || actual_len == 0 {
            return String::new();
        }
        String::from_utf16_lossy(&unicode[..actual_len as usize])
    }

    pub fn reset_dead_state(&self) {
        *self.dead_state.lock() = 0;
    }
}

impl Default for KeyboardDecoder {
    fn default() -> Self {
        Self::new()
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
