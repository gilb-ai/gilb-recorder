//! Direct FFI for Apple APIs not exposed by the ecosystem crates.

#![allow(non_upper_case_globals, non_snake_case)]

use std::ffi::c_void;

pub type IOHIDRequestType = u32;
pub type IOHIDAccessType = u32;

pub const kIOHIDRequestTypePostEvent: IOHIDRequestType = 0;
pub const kIOHIDRequestTypeListenEvent: IOHIDRequestType = 1;

pub const kIOHIDAccessTypeGranted: IOHIDAccessType = 0;
pub const kIOHIDAccessTypeDenied: IOHIDAccessType = 1;
pub const kIOHIDAccessTypeUnknown: IOHIDAccessType = 2;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    pub fn IOHIDCheckAccess(request: IOHIDRequestType) -> IOHIDAccessType;
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
