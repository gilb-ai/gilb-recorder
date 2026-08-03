//! Direct FFI for Apple APIs not exposed by the ecosystem crates.

#![allow(non_upper_case_globals, non_snake_case)]

use std::ffi::c_void;

// CoreGraphics Input Monitoring access. `CGPreflightListenEventAccess`
// returns true when the process is approved for Accessibility OR Input
// Monitoring — which is exactly the signal our recorder needs (the
// underlying `CGEventTap` honours both grants). There is no `CGRequest…`
// counterpart here on purpose: the splash screen sends the user to the
// System Settings pane instead, because the OS shows its own prompt at
// most once and a user who dismissed it would have no way back.
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    pub fn CGPreflightListenEventAccess() -> bool;

    // Screen Recording access (macOS 10.15+). `CGPreflight…` is a
    // polling-safe status probe; `CGRequest…` registers the process with
    // TCC (so it appears in System Settings → Screen Recording) and shows
    // the native consent prompt. The meeting recorder's ScreenCaptureKit
    // stream needs this grant.
    pub fn CGPreflightScreenCaptureAccess() -> bool;
    pub fn CGRequestScreenCaptureAccess() -> bool;
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
