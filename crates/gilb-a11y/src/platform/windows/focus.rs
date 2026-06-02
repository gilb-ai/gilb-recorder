//! Foreground-window introspection: app (process) name, window title, pid.
//!
//! Cheap Win32 calls usable from any thread, so — like the macOS NSWorkspace
//! path — they run directly on the normalizer's focus-poll tick rather than a
//! dedicated observer thread. Browser-URL extraction is deferred (it needs a
//! UIA walk); `browser_url` stays `None` for now.

use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
};

use gilb_core::AppInfo;

use crate::FocusProvider;

/// Windows [`FocusProvider`] backed by `GetForegroundWindow` + ToolHelp.
pub struct WindowsFocusProvider;

impl FocusProvider for WindowsFocusProvider {
    fn frontmost(&self) -> AppInfo {
        snapshot(false)
    }
    fn frontmost_with_window(&self) -> AppInfo {
        snapshot(true)
    }
}

fn snapshot(with_window: bool) -> AppInfo {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return AppInfo::default();
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let name = get_process_name(pid);
        let window_title = if with_window { window_text(hwnd) } else { None };
        AppInfo {
            // The exe name doubles as the stable app id — it's what
            // `password_masking::is_excluded_app` matches against on Windows.
            bundle_id: name.clone(),
            name,
            pid: Some(pid as i32),
            window_title,
            browser_url: None,
        }
    }
}

fn window_text(hwnd: HWND) -> Option<String> {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return None;
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        let n = GetWindowTextW(hwnd, &mut buf);
        if n <= 0 {
            return None;
        }
        let s = String::from_utf16_lossy(&buf[..n as usize]);
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

/// Resolve a pid to its executable file name (e.g. `chrome.exe`) by scanning
/// the process snapshot. Returns `None` if the process is gone.
fn get_process_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut result = None;
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                if entry.th32ProcessID == pid {
                    let end = entry
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(entry.szExeFile.len());
                    result = Some(String::from_utf16_lossy(&entry.szExeFile[..end]));
                    break;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        result
    }
}
