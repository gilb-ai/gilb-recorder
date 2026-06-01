//! Windows virtual-key → [`SpecialKey`] classification and layout-aware text
//! translation for the low-level keyboard hook.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, GetKeyboardLayout, ToUnicodeEx,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::keyboard::SpecialKey;

/// Virtual-key code → optional special-key classification.
/// Values are stable `VK_*` constants (`winuser.h`).
pub fn special_key_from_vk(vk: u16) -> Option<SpecialKey> {
    Some(match vk {
        0x0D => SpecialKey::Return,     // VK_RETURN
        0x09 => SpecialKey::Tab,        // VK_TAB
        0x1B => SpecialKey::Escape,     // VK_ESCAPE
        0x08 => SpecialKey::Backspace,  // VK_BACK
        0x2E => SpecialKey::Delete,     // VK_DELETE
        0x25 => SpecialKey::ArrowLeft,  // VK_LEFT
        0x26 => SpecialKey::ArrowUp,    // VK_UP
        0x27 => SpecialKey::ArrowRight, // VK_RIGHT
        0x28 => SpecialKey::ArrowDown,  // VK_DOWN
        0x24 => SpecialKey::Home,       // VK_HOME
        0x23 => SpecialKey::End,        // VK_END
        0x21 => SpecialKey::PageUp,     // VK_PRIOR
        0x22 => SpecialKey::PageDown,   // VK_NEXT
        _ => return None,
    })
}

/// Translate a key event to the printable text it produces, honouring the
/// foreground window's keyboard layout and the live modifier state
/// (Shift / AltGr / CapsLock). Returns `None` for keys that produce no
/// printable character (or only control characters — e.g. Ctrl+C, which we
/// treat as a shortcut, not typed text).
///
/// We pass `ToUnicodeEx` the "do not change keyboard state" flag (bit 2,
/// Windows 10 1607+) so this passive read does not disturb the user's
/// dead-key composition.
pub fn vk_to_text(vk: u16, scan: u32) -> Option<String> {
    // Build a keyboard-state array. The low-level hook thread is not the
    // foreground thread, so its own GetKeyboardState is unreliable; query the
    // async state of the modifier keys explicitly instead.
    let mut keystate = [0u8; 256];
    const VK_SHIFT: usize = 0x10;
    const VK_CONTROL: usize = 0x11;
    const VK_MENU: usize = 0x12; // Alt
    const VK_CAPITAL: usize = 0x14;
    const MODIFIERS: &[usize] = &[
        VK_SHIFT, VK_CONTROL, VK_MENU, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5,
    ];

    unsafe {
        for &m in MODIFIERS {
            if (GetAsyncKeyState(m as i32) as u16 & 0x8000) != 0 {
                keystate[m] = 0x80;
            }
        }
        if (GetKeyState(VK_CAPITAL as i32) & 0x0001) != 0 {
            keystate[VK_CAPITAL] = 0x01;
        }

        let hwnd = GetForegroundWindow();
        let tid = GetWindowThreadProcessId(hwnd, None);
        let hkl = GetKeyboardLayout(tid);

        let mut buf = [0u16; 8];
        let n = ToUnicodeEx(vk as u32, scan, &keystate, &mut buf, 0x0004, hkl);
        if n <= 0 {
            return None;
        }
        let s: String = String::from_utf16_lossy(&buf[..n as usize])
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}
