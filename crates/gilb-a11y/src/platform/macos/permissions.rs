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
/// Three layered nudges:
///   1. `IOHIDRequestAccess` (IOKit) — the long-standing reliable path.
///   2. `CGRequestListenEventAccess` (CoreGraphics) — Apple's modern,
///      documented call; still emits the native consent prompt for users
///      who already have the toggle exposed.
///   3. A throwaway `CGEventTapCreate` with an empty event mask — the
///      single operation TCC *always* reacts to. Even when 1+2 silently
///      no-op (most painful in packaged builds where the System Settings
///      list comes up empty), step 3 forces TCC to add the app. The tap
///      returns NULL when permission isn't granted yet, in which case
///      nothing needs releasing.
pub fn request_input_monitoring() -> bool {
    unsafe {
        // SAFETY: primitive arg, thread-safe per Apple docs.
        let _io = ffi::IOHIDRequestAccess(ffi::kIOHIDRequestTypeListenEvent);
        // SAFETY: no args, thread-safe per Apple docs.
        let cg = ffi::CGRequestListenEventAccess();

        // SAFETY: empty event mask (no events of interest) + a no-op
        // callback that returns the event unmodified. Creating the tap
        // either fails (returns NULL — no resource leak) or succeeds and
        // we immediately release the returned CFMachPortRef. Either way
        // TCC registers the process.
        let port = ffi::CGEventTapCreate(
            ffi::kCGHIDEventTap,
            ffi::kCGHeadInsertEventTap,
            ffi::kCGEventTapOptionListenOnly,
            0, // no events of interest — we never want callbacks
            tap_probe_noop,
            std::ptr::null_mut(),
        );
        if !port.is_null() {
            ffi::CFRelease(port as ffi::CFTypeRef);
        }

        cg
    }
}

/// No-op tap callback used only by the `CGEventTapCreate` probe in
/// [`request_input_monitoring`]. Apple requires a non-null callback even
/// when the event mask is empty; this one returns the event unchanged
/// and is never actually invoked because no events match.
unsafe extern "C" fn tap_probe_noop(
    _proxy: ffi::CGEventTapProxy,
    _event_type: u32,
    event: ffi::CGEventRef,
    _user_info: *mut std::ffi::c_void,
) -> ffi::CGEventRef {
    event
}
