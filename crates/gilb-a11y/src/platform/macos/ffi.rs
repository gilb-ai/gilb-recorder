//! Direct FFI for Apple APIs not exposed by the ecosystem crates.

#![allow(non_upper_case_globals, non_snake_case)]

use std::ffi::c_void;

// Input Monitoring access — two parallel API families that target the same
// TCC entry. We call **both** from `request_input_monitoring`:
//
// * `CGRequestListenEventAccess` is the modern documented call, but in
//   practice it does not always add the app to System Settings → Privacy &
//   Security → Input Monitoring on first invocation in a packaged build.
// * `IOHIDRequestAccess(kIOHIDRequestTypeListenEvent)` is the older IOKit
//   path; it registers the process with TCC reliably (this is what
//   long-standing utilities like Karabiner-Elements use).
//
// Calling both is harmless — TCC dedupes the registration — and the IOKit
// call closes the "empty list" failure mode where the user has no toggle
// to flip.
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    pub fn CGPreflightListenEventAccess() -> bool;
    pub fn CGRequestListenEventAccess() -> bool;
}

/// `IOHIDRequestType` is a `uint32_t`. `kIOHIDRequestTypeListenEvent = 1`
/// (the Input Monitoring TCC entry); `kIOHIDRequestTypePostEvent = 0` is
/// the separate "post events" entry we don't need.
pub const kIOHIDRequestTypeListenEvent: u32 = 1;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    pub fn IOHIDRequestAccess(requestType: u32) -> bool;
    pub fn IOHIDCheckAccess(requestType: u32) -> u32;
}

// ---- CGEvent keyboard text extraction -----------------------------------
//
// Used to be `TISCopyCurrentKeyboardLayoutInputSource` + `UCKeyTranslate`,
// but macOS 26+ enforces a `dispatch_assert_queue` on TSM calls — they must
// run on the main thread. Our normalizer runs on a tokio worker, so we'd
// trip the assertion and die with SIGTRAP.
//
// `CGEventKeyboardGetUnicodeString` reads characters straight out of the
// CGEvent's payload — no TSM round-trip, safe from any thread that holds
// the event ref.

/// Opaque `CGEventRef` (Carbon `typedef struct __CGEvent *`). We get one of
/// these by casting `core_graphics::event::CGEvent::as_ptr()`.
pub type CGEventRef = *mut c_void;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    pub fn CGEventKeyboardGetUnicodeString(
        event: CGEventRef,
        maxStringLength: libc::c_ulong,
        actualStringLength: *mut libc::c_ulong,
        unicodeString: *mut u16,
    );
}
