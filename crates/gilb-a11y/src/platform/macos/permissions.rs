//! Permission probes and request flows for macOS.
//!
//! `*_granted` functions are cheap polling probes — they do not trigger
//! system prompts. `request_*` functions register the process with TCC
//! (so it appears in System Settings → Privacy & Security) and, on
//! macOS's discretion, show the native consent prompt.
//!
//! Note on Input Monitoring: Apple documents `CGPreflightListenEventAccess`
//! as returning `true` when the process is approved for **Accessibility OR
//! Input Monitoring** — and `CGEventTapCreate` actually works through the
//! AX-granted channel on modern macOS. We embrace that: once the user
//! grants Accessibility, the recorder has everything it needs. We never
//! ask for Input Monitoring separately.

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

/// Returns `true` when the process is approved to listen for input events.
/// Polling-safe — does not trigger the system prompt.
///
/// Apple's `CGPreflightListenEventAccess` returns `true` if the process has
/// been granted **either** Accessibility or Input Monitoring, and the
/// underlying `CGEventTap` honours both. So in our model "input monitoring
/// granted" really means "we can listen for events at all" — which the AX
/// grant alone already satisfies.
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

/// Returns `true` when the process is approved to capture the screen. Needed
/// by the meeting recorder's ScreenCaptureKit stream. Polling-safe — does not
/// trigger the system prompt.
pub fn screen_recording_granted() -> bool {
    // SAFETY: CoreGraphics access-check call, thread-safe.
    unsafe { ffi::CGPreflightScreenCaptureAccess() }
}

/// Trigger the macOS Screen Recording permission flow. Registers the process
/// with TCC (so it appears in System Settings → Screen Recording) and shows
/// the native consent prompt while still ungranted. Returns the current
/// granted state — typically `false` on first call.
pub fn request_screen_recording() -> bool {
    // SAFETY: CoreGraphics request call, thread-safe.
    unsafe { ffi::CGRequestScreenCaptureAccess() }
}

/// Returns `true` when the process is authorized to capture microphone audio.
/// Mirrors `AVCaptureDevice.authorizationStatus(for: .audio) == .authorized`.
/// Polling-safe — does not trigger the system prompt.
pub fn microphone_granted() -> bool {
    use objc2::msg_send;
    use objc2::runtime::AnyClass;

    // `AVAuthorizationStatusAuthorized` is `3`; everything else (not
    // determined / restricted / denied) means "not granted".
    const AV_AUTHORIZED: isize = 3;

    let Some(cls) = AnyClass::get(c"AVCaptureDevice") else {
        return false;
    };
    // SAFETY: `+[AVCaptureDevice authorizationStatusForMediaType:]` takes an
    // `AVMediaType` (an `NSString *`) and returns `AVAuthorizationStatus`
    // (`NSInteger`). `AVMediaTypeAudio` is a framework-exported string; the
    // call is thread-safe and does not block.
    let status: isize =
        unsafe { msg_send![cls, authorizationStatusForMediaType: av_media_type_audio()] };
    status == AV_AUTHORIZED
}

/// Trigger the macOS Microphone permission flow. Registers the process with
/// TCC (so it appears in System Settings → Microphone) and shows the native
/// consent prompt while still undetermined. Fire-and-forget: the grant is
/// observed later by polling [`microphone_granted`].
pub fn request_microphone() {
    use block2::RcBlock;
    use objc2::msg_send;
    use objc2::runtime::AnyClass;

    let Some(cls) = AnyClass::get(c"AVCaptureDevice") else {
        return;
    };
    // `void (^)(BOOL granted)` — we don't act on the result here; the UI
    // re-polls `microphone_granted` once the user answers the prompt.
    let handler = RcBlock::new(|_granted: objc2::runtime::Bool| {});
    // SAFETY: `+[AVCaptureDevice requestAccessForMediaType:completionHandler:]`
    // takes an `AVMediaType` and a completion block; the block is retained by
    // the framework for the duration of the async prompt.
    let _: () = unsafe {
        msg_send![
            cls,
            requestAccessForMediaType: av_media_type_audio(),
            completionHandler: &*handler,
        ]
    };
}

/// The `AVMediaTypeAudio` constant — an `NSString *` exported by AVFoundation.
fn av_media_type_audio() -> *const objc2::runtime::AnyObject {
    #[link(name = "AVFoundation", kind = "framework")]
    extern "C" {
        static AVMediaTypeAudio: *const objc2::runtime::AnyObject;
    }
    // SAFETY: reading a framework-exported immortal string constant.
    unsafe { AVMediaTypeAudio }
}
