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

// ---- CGEventTap probe (TCC registration fallback) -----------------------
//
// When IOKit + CG `RequestAccess` calls still leave the Input Monitoring
// list empty, the **only** thing TCC reliably reacts to is an actual
// attempt to create an event tap. Even a tap with an empty mask and a
// no-op callback triggers the OS to register the app with TCC; the call
// returns NULL when permission is not yet granted, which is fine — we
// just release whatever (if anything) came back.

pub type CGEventTapProxy = *mut c_void;
pub type CGEventTapCallBack = unsafe extern "C" fn(
    proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef;
pub type CFMachPortRef = *mut c_void;
pub type CFTypeRef = *const c_void;

pub const kCGHIDEventTap: u32 = 0;
pub const kCGHeadInsertEventTap: u32 = 0;
pub const kCGEventTapOptionListenOnly: u32 = 1;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    pub fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub fn CFRelease(cf: CFTypeRef);
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
