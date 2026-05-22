//! Platform-specific implementations behind `cfg`.

#[cfg(target_os = "macos")]
pub mod macos {
    pub mod ax_worker;
    pub mod event_tap;
    pub mod ffi;
    pub mod focus;
    pub mod keyboard;
    pub mod normalizer;
    pub mod pasteboard;
    pub mod permissions;
    mod platform;

    pub use platform::MacosPlatform;
}

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub mod unsupported;
