//! Low-level keyboard + mouse hooks on a dedicated message-pump thread.
//!
//! `WH_KEYBOARD_LL` / `WH_MOUSE_LL` callbacks are invoked by the OS on the
//! thread that installed them, and only while that thread pumps messages — so
//! the thread owns a `GetMessageW` loop. The callbacks must return fast, so
//! they only decode into a [`RawEvent`] and `try_send` it (lossy on a full
//! channel, exactly like the macOS event-tap). The shared normalizer does all
//! the buffering/debounce downstream.

use std::cell::RefCell;
use std::thread::{self, JoinHandle};

use anyhow::{anyhow, Result};
use tokio::sync::mpsc;
use tracing::{debug, info};

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PostThreadMessageW, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MOUSEWHEEL,
    WM_QUIT, WM_RBUTTONDOWN, WM_SYSKEYDOWN, WM_XBUTTONDOWN,
};

use super::keyboard::{special_key_from_vk, vk_to_text};
use crate::events::{MouseButton, RawEvent};

thread_local! {
    static HOOK_TX: RefCell<Option<mpsc::Sender<RawEvent>>> = const { RefCell::new(None) };
}

fn emit(ev: RawEvent) {
    HOOK_TX.with(|cell| {
        if let Ok(guard) = cell.try_borrow() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.try_send(ev);
            }
        }
    });
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let msg = wparam.0 as u32;
        if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let vk = kb.vkCode as u16;
            let special = special_key_from_vk(vk);
            // Only translate to text for non-special keys; special keys are
            // flush triggers handled by the normalizer.
            let text = if special.is_some() {
                None
            } else {
                vk_to_text(vk, kb.scanCode)
            };
            if special.is_some() || text.is_some() {
                emit(RawEvent::KeyDown { special, text });
            }
        }
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let msg = wparam.0 as u32;
        let ms = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        let x = ms.pt.x as f64;
        let y = ms.pt.y as f64;
        let ev = match msg {
            WM_LBUTTONDOWN => Some(RawEvent::MouseDown {
                button: MouseButton::Left,
                x,
                y,
            }),
            WM_RBUTTONDOWN => Some(RawEvent::MouseDown {
                button: MouseButton::Right,
                x,
                y,
            }),
            WM_MBUTTONDOWN | WM_XBUTTONDOWN => Some(RawEvent::MouseDown {
                button: MouseButton::Other,
                x,
                y,
            }),
            WM_MOUSEWHEEL => {
                // High word of mouseData is a signed wheel delta in units of
                // WHEEL_DELTA (120). Normalise to detents.
                let delta = ((ms.mouseData >> 16) as i16) as i64 / 120;
                Some(RawEvent::Scroll {
                    delta_y: delta,
                    delta_x: 0,
                })
            }
            _ => None,
        };
        if let Some(ev) = ev {
            emit(ev);
        }
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

/// Owns the hook thread. Drop is not sufficient to stop it; call [`stop`].
pub struct HookThread {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl HookThread {
    /// Spawn the hook thread. Returns once both hooks are installed, or an
    /// error if installation fails.
    pub fn spawn(raw_tx: mpsc::Sender<RawEvent>) -> Result<Self> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::result::Result<u32, String>>();
        let join = thread::Builder::new()
            .name("gilb-win-hooks".into())
            .spawn(move || run_hook_thread(raw_tx, ready_tx))
            .map_err(|e| anyhow!("failed to spawn hook thread: {e}"))?;

        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                join: Some(join),
            }),
            Ok(Err(msg)) => {
                let _ = join.join();
                Err(anyhow!(msg))
            }
            Err(e) => {
                let _ = join.join();
                Err(anyhow!("hook thread exited without signalling: {e}"))
            }
        }
    }

    /// Post `WM_QUIT` to the hook thread and join it.
    pub fn stop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn run_hook_thread(
    raw_tx: mpsc::Sender<RawEvent>,
    ready_tx: std::sync::mpsc::Sender<std::result::Result<u32, String>>,
) {
    HOOK_TX.with(|cell| *cell.borrow_mut() = Some(raw_tx));

    unsafe {
        let hinstance: HINSTANCE = match GetModuleHandleW(None) {
            Ok(h) => HINSTANCE(h.0),
            Err(e) => {
                let _ = ready_tx.send(Err(format!("GetModuleHandleW failed: {e}")));
                return;
            }
        };

        let kb_hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), hinstance, 0) {
            Ok(h) => h,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("install keyboard hook failed: {e}")));
                return;
            }
        };
        let mouse_hook = match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinstance, 0) {
            Ok(h) => h,
            Err(e) => {
                let _ = UnhookWindowsHookEx(kb_hook);
                let _ = ready_tx.send(Err(format!("install mouse hook failed: {e}")));
                return;
            }
        };

        let thread_id = GetCurrentThreadId();
        if ready_tx.send(Ok(thread_id)).is_err() {
            let _ = UnhookWindowsHookEx(kb_hook);
            let _ = UnhookWindowsHookEx(mouse_hook);
            return;
        }
        info!("hooks installed");

        let mut msg = MSG::default();
        loop {
            // hwnd = null → also delivers thread messages (our WM_QUIT).
            let ret = GetMessageW(&mut msg, HWND::default(), 0, 0);
            if ret.0 <= 0 {
                break; // 0 = WM_QUIT, -1 = error
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnhookWindowsHookEx(kb_hook);
        let _ = UnhookWindowsHookEx(mouse_hook);
        debug!("hooks removed");
    }

    HOOK_TX.with(|cell| *cell.borrow_mut() = None);
}
