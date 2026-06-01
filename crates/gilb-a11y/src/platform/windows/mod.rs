//! Windows capture backend: low-level keyboard/mouse hooks + UI Automation,
//! feeding the shared normalizer. See `platform.rs` for the wiring.

mod clipboard;
mod focus;
mod hooks;
mod keyboard;
mod platform;
mod uia;

pub use platform::WindowsPlatform;
