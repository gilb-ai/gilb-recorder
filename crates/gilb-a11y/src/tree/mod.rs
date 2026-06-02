//! AX tree snapshot pipeline.
//!
//! Wired in Phase 2: on focus_change events the normalizer asks the
//! [`Snapshotter`] to capture the focused window's a11y tree, dedup
//! against the last one via SimHash, and persist it.
//!
//! The walker itself is platform-gated — only macOS today; other
//! platforms return `None`. The simhash + cache logic is platform-
//! independent and lives in this crate's `cache` module.

pub mod cache;
pub mod node;
pub mod snapshotter;

#[cfg(target_os = "macos")]
pub mod walker_macos;

#[cfg(target_os = "windows")]
pub mod walker_windows;
