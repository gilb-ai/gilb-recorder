//! Platform-neutral input event types.
//!
//! Each OS capture source (macOS CGEventTap, Windows low-level hooks) decodes
//! native events into these and pushes them through an `mpsc` channel to the
//! shared [`crate::normalizer::Normalizer`]. Keeping the enum free of any
//! OS-specific fields (raw keycodes, modifier bitmasks) is what lets a single
//! normalizer serve every platform.

use crate::keyboard::SpecialKey;

/// Minimal representation of an input event handed to the normalizer.
///
/// Key→text translation happens in the platform source (the OS composes the
/// character with the active layout for us), as does keycode→[`SpecialKey`]
/// classification — so the normalizer never touches raw codes.
#[derive(Debug, Clone)]
pub enum RawEvent {
    KeyDown {
        /// `Some` for non-printable navigation/editing keys; `None` otherwise.
        special: Option<SpecialKey>,
        /// Composed printable text for this key, if any.
        text: Option<String>,
    },
    MouseDown {
        button: MouseButton,
        x: f64,
        y: f64,
    },
    Scroll {
        delta_y: i64,
        delta_x: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Other,
}

/// A clipboard contents change observed by a platform clipboard source.
#[derive(Debug, Clone)]
pub struct ClipboardChange {
    pub change_count: i64,
    pub text: Option<String>,
}
