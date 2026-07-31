//! Shared data types used across `gilb-*` crates.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
        }
    }
}

/// Periodic AX-tree snapshot of a single focused window.
///
/// Emitted on focus transitions. Persisted to `tree_snapshots`; correlated
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
}

/// Anything the capture pipeline can hand to the engine writer.
///
/// One channel carries both actions and tree snapshots to the writer. Keep
/// this enum small — we route on the variant in the writer's hot loop.
#[derive(Debug, Clone)]
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
