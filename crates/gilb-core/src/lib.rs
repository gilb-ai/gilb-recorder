//! Shared data types used across `gilb-*` crates.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifier for a recording session (one row in `sessions`).
pub type SessionId = i64;

/// Identifier for an a11y action row (one row in `actions`).
pub type ActionId = i64;

/// Identifier for a tree snapshot row (one row in `tree_snapshots`).
pub type TreeSnapshotId = i64;

/// Kinds of atomic actions a user can perform.
///
/// Stored as a lowercase string in `actions.kind` for human-readable queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Click,
    Text,
    Key,
    Scroll,
    Clipboard,
    FocusChange,
    /// Lifecycle marker (idle/alive/lock/unlock/recording). The specific
    /// signal rides in `extra_json.system` (see gilb-a11y `idle` + the macOS
    /// `session` source). Lets the server segment cases/sessions for mining.
    System,
    Debug,
}

impl ActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ActionKind::Click => "click",
            ActionKind::Text => "text",
            ActionKind::Key => "key",
            ActionKind::Scroll => "scroll",
            ActionKind::Clipboard => "clipboard",
            ActionKind::FocusChange => "focus_change",
            ActionKind::System => "system",
            ActionKind::Debug => "debug",
        }
    }
}

/// Periodic AX-tree snapshot of a single focused window.
///
/// Emitted on focus transitions (Phase 2 may also trigger on clipboard
/// and opaque-element clicks). Persisted to `tree_snapshots`; correlated
/// to nearby `actions` rows by `(session_id, captured_at)` within a
/// short time window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeSnapshot {
    pub session_id: SessionId,
    pub captured_at: DateTime<Utc>,
    pub app: AppInfo,
    /// 64-bit SimHash over 3-word shingles of element-role + element-text.
    /// Stored as i64 because SQLite has no native u64.
    pub simhash: i64,
    /// Serialized tree as JSON. Shape is intentionally not part of the
    /// schema — it's a blob the analyzer LLM parses on demand.
    pub root_json: String,
}

/// Foreground app context attached to every event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppInfo {
    pub bundle_id: Option<String>,
    pub name: Option<String>,
    pub pid: Option<i32>,
    pub window_title: Option<String>,
    /// Active URL of the focused tab when the app is a known browser
    /// (Chrome / Safari / Firefox / Edge / Brave / Arc / Vivaldi / Opera /
    /// Zen / Comet / Chromium). `None` for non-browser apps.
    pub browser_url: Option<String>,
}

/// Accessibility element context for the focused / clicked element.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ElementContext {
    pub role: Option<String>,
    pub name: Option<String>,
    pub value: Option<String>,
    pub help: Option<String>,
    pub identifier: Option<String>,
    pub frame: Option<Frame>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Frame {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionRange {
    pub start: usize,
    pub end: usize,
}

/// Modifier-key flags (SHIFT/CTRL/OPT/CMD/CAPS/FN) as a compact u8 bitfield.
/// Copied from screenpipe-a11y/events.rs — the cross-platform type that the
/// macOS CGEventFlags decoder (GILB-64) and the RawEvent (GILB-65) use.
/// `from_cg_flags` is macOS-only (the flag constants are CGEventFlags).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const SHIFT: u8 = 1 << 0;
    pub const CTRL: u8 = 1 << 1;
    pub const OPT: u8 = 1 << 2;
    pub const CMD: u8 = 1 << 3;
    pub const CAPS: u8 = 1 << 4;
    pub const FN: u8 = 1 << 5;

    pub fn new() -> Self {
        Self(0)
    }

    pub fn has_shift(&self) -> bool {
        self.0 & Self::SHIFT != 0
    }
    pub fn has_ctrl(&self) -> bool {
        self.0 & Self::CTRL != 0
    }
    pub fn has_opt(&self) -> bool {
        self.0 & Self::OPT != 0
    }
    pub fn has_cmd(&self) -> bool {
        self.0 & Self::CMD != 0
    }
    pub fn any_modifier(&self) -> bool {
        self.0 & (Self::CMD | Self::CTRL) != 0
    }

    #[cfg(target_os = "macos")]
    pub fn from_cg_flags(flags: u64) -> Self {
        let mut m = 0u8;
        if flags & 0x20000 != 0 {
            m |= Self::SHIFT;
        }
        if flags & 0x40000 != 0 {
            m |= Self::CTRL;
        }
        if flags & 0x80000 != 0 {
            m |= Self::OPT;
        }
        if flags & 0x100000 != 0 {
            m |= Self::CMD;
        }
        if flags & 0x10000 != 0 {
            m |= Self::CAPS;
        }
        if flags & 0x800000 != 0 {
            m |= Self::FN;
        }
        Self(m)
    }
}

/// A single captured action, before it has been persisted.
///
/// The `id` and `session_id` are assigned by the write queue / engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub session_id: SessionId,
    pub captured_at: DateTime<Utc>,
    pub kind: ActionKind,
    pub app: AppInfo,
    pub element: ElementContext,
    pub text_content: Option<String>,
    pub password_flag: bool,
    pub tree_snapshot_id: Option<TreeSnapshotId>,
    pub extra_json: Option<serde_json::Value>,
    /// Clipboard operation kind ("copy"/"cut"/"paste"); `Some` only for
    /// `Clipboard` actions. Step-1: always "copy" — NSPasteboard can't tell
    /// copy from cut; cut/paste detection is deferred.
    pub clipboard_op: Option<String>,
    /// sha256 (hex) of the raw clipboard text BEFORE PII redaction; `Some`
    /// only for `Clipboard` actions. Lets copy↔paste linking survive
    /// redaction of the shipped `text_content`.
    pub content_hash: Option<String>,
}

/// Anything the capture pipeline can hand to the engine writer.
///
/// Phase 2: the channel between [`crate::Action`] producers and the DB
/// writer fans in [`crate::TreeSnapshot`]s too. Keep this enum small —
/// we route on the variant in the writer's hot loop.
#[derive(Debug, Clone)]
// `Action` is ~408 B; the `clipboard_op`/`content_hash` fields pushed it past
// clippy's variant-size threshold. This enum is only ever moved through a
// tokio mpsc channel as a heap-managed slot, so the inline size carries no
// stack/perf cost — boxing the variant would just add a per-event allocation.
#[allow(clippy::large_enum_variant)]
pub enum WriterMessage {
    Action(Action),
    TreeSnapshot(TreeSnapshot),
}

impl From<Action> for WriterMessage {
    fn from(a: Action) -> Self {
        WriterMessage::Action(a)
    }
}

impl From<TreeSnapshot> for WriterMessage {
    fn from(s: TreeSnapshot) -> Self {
        WriterMessage::TreeSnapshot(s)
    }
}

impl Action {
    pub fn new_debug(session_id: SessionId, message: impl Into<String>) -> Self {
        Self {
            session_id,
            captured_at: Utc::now(),
            kind: ActionKind::Debug,
            app: AppInfo::default(),
            element: ElementContext::default(),
            text_content: Some(message.into()),
            password_flag: false,
            tree_snapshot_id: None,
            extra_json: None,
            clipboard_op: None,
            content_hash: None,
        }
    }
}

/// A unique correlation id we expose to logs / health events.
pub fn new_correlation_id() -> String {
    Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_range_serializes_to_start_end() {
        let range = SelectionRange { start: 0, end: 10 };
        let json = serde_json::to_string(&range).unwrap();
        assert_eq!(json, r#"{"start":0,"end":10}"#);

        let back: SelectionRange = serde_json::from_str(&json).unwrap();
        assert_eq!(back, range);
    }
}
