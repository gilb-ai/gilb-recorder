//! Direct FFI for Apple APIs not exposed by the ecosystem crates.

#![allow(non_upper_case_globals, non_snake_case)]

use std::ffi::c_void;

// CoreGraphics Input Monitoring access. `CGPreflightListenEventAccess`
// returns true when the process is approved for Accessibility OR Input
// Monitoring — which is exactly the signal our recorder needs (the
// underlying `CGEventTap` honours both grants).
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    pub fn CGPreflightListenEventAccess() -> bool;
    pub fn CGRequestListenEventAccess() -> bool;
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
