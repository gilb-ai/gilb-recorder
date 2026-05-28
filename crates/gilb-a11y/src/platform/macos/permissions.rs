//! Permission probes and request flows for macOS.
//!
//! `*_granted` functions are cheap polling probes — they do not trigger
//! system prompts. `request_*` functions register the process with TCC
//! (so it appears in System Settings → Privacy & Security) and, on
//! macOS's discretion, show the native consent prompt.

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;

use super::ffi;

/// Returns `true` when the process is allowed to read AX information of other
/// processes. Equivalent to System Settings → Privacy → Accessibility being
/// checked for this app. Polling-safe — does not trigger the system prompt.
pub fn accessibility_granted() -> bool {
    // SAFETY: accessibility-sys exposes `AXIsProcessTrusted` (no args). It is
    // safe to call from any thread.
    unsafe { accessibility_sys::AXIsProcessTrusted() }
}

/// Returns `true` when System Settings → Privacy → Input Monitoring is checked
/// for this app. Without it, CGEventTap silently delivers no events.
/// Polling-safe — `CGPreflightListenEventAccess` does not trigger a prompt.
pub fn input_monitoring_granted() -> bool {
    // SAFETY: CoreGraphics access-check call, thread-safe.
    unsafe { ffi::CGPreflightListenEventAccess() }
}

/// Trigger the macOS Accessibility permission flow. Registers the process
/// with TCC (so it appears in System Settings) and shows the native
/// consent prompt on first call. Subsequent calls re-show the prompt only
/// while still untrusted. Returns the current trusted state — typically
/// `false` on first call, because the user has not yet acted on the prompt.
pub fn request_accessibility() -> bool {
    // SAFETY: `kAXTrustedCheckOptionPrompt` is a static `CFStringRef`
    // exported by ApplicationServices; we wrap it under +0 (get-rule) and
    // build a one-entry dictionary the framework copies internally.
    unsafe {
        let key = CFString::wrap_under_get_rule(accessibility_sys::kAXTrustedCheckOptionPrompt);
        let value = CFBoolean::true_value();
        let dict = CFDictionary::from_CFType_pairs(&[(key, value)]);
        accessibility_sys::AXIsProcessTrustedWithOptions(dict.as_concrete_TypeRef())
    }
}

/// Trigger the macOS Input Monitoring permission flow. Registers the
/// process with TCC (so it appears in System Settings) and shows the
/// native consent prompt on first call. Returns the resulting grant
/// status. Subsequent calls return the current status without re-prompting
/// — once the user denies, the only way back is via System Settings.
///
/// Calls both `IOHIDRequestAccess` (IOKit) and `CGRequestListenEventAccess`
/// (CoreGraphics). They target the same TCC entry, but in packaged builds
/// the CG call alone sometimes leaves the System Settings list empty — the
/// IOKit call has historically been the reliable way to get the process
/// listed. See the comment in `ffi.rs`.
pub fn request_input_monitoring() -> bool {
    // SAFETY: both calls take a single primitive argument and are
    // documented as thread-safe.
    unsafe {
        let _io = ffi::IOHIDRequestAccess(ffi::kIOHIDRequestTypeListenEvent);
        ffi::CGRequestListenEventAccess()
    }
}
