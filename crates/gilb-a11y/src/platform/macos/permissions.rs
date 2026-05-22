//! Permission probes for macOS.
//!
//! Both checks are cheap and idempotent — call freely on every `status` poll.

use super::ffi;

/// Returns `true` when the process is allowed to read AX information of other
/// processes. Equivalent to System Settings → Privacy → Accessibility being
/// checked for this app.
pub fn accessibility_granted() -> bool {
    // SAFETY: accessibility-sys exposes `AXIsProcessTrusted` (no args). It is
    // safe to call from any thread.
    unsafe { accessibility_sys::AXIsProcessTrusted() }
}

/// Returns `true` when System Settings → Privacy → Input Monitoring is checked
/// for this app. Without it, CGEventTap silently delivers no events.
pub fn input_monitoring_granted() -> bool {
    // SAFETY: IOHIDCheckAccess is a thread-safe IOKit call.
    let access = unsafe { ffi::IOHIDCheckAccess(ffi::kIOHIDRequestTypeListenEvent) };
    access == ffi::kIOHIDAccessTypeGranted
}
