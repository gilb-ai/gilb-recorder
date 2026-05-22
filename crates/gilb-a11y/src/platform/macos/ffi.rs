//! Direct FFI for Apple APIs not exposed by the ecosystem crates.

#![allow(non_upper_case_globals, non_snake_case)]

use core_foundation::base::CFTypeRef;

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

// ---- Text Input Source / UCKeyTranslate ---------------------------------

pub type TISInputSourceRef = CFTypeRef;
pub type UCKeyboardLayoutPtr = *const u8;

pub const kUCKeyTranslateNoDeadKeysBit: u32 = 0;

pub const kUCKeyActionDown: u16 = 0;
pub const kUCKeyActionUp: u16 = 1;
pub const kUCKeyActionAutoKey: u16 = 2;

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    pub fn TISCopyCurrentKeyboardLayoutInputSource() -> TISInputSourceRef;
    pub fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> TISInputSourceRef;

    pub fn TISGetInputSourceProperty(
        source: TISInputSourceRef,
        property: CFTypeRef,
    ) -> CFTypeRef;

    pub fn LMGetKbdType() -> u8;

    pub fn UCKeyTranslate(
        keyLayoutPtr: UCKeyboardLayoutPtr,
        virtualKeyCode: u16,
        keyAction: u16,
        modifierKeyState: u32,
        keyboardType: u32,
        keyTranslateOptions: u32,
        deadKeyState: *mut u32,
        maxStringLength: libc::c_ulong,
        actualStringLength: *mut libc::c_ulong,
        unicodeString: *mut u16,
    ) -> i32;

    pub static kTISPropertyUnicodeKeyLayoutData: CFTypeRef;
}

// `CFDataGetBytePtr` is in core-foundation already, but we sometimes need to
// take it as a raw pointer for UCKeyTranslate; expose a typed name.
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub fn CFDataGetBytePtr(data: CFTypeRef) -> UCKeyboardLayoutPtr;
}
