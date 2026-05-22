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
            ActionKind::Debug => "debug",
        }
    }
}

/// Foreground app context attached to every event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppInfo {
    pub bundle_id: Option<String>,
    pub name: Option<String>,
    pub pid: Option<i32>,
    pub window_title: Option<String>,
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
        }
    }
}

/// A unique correlation id we expose to logs / health events.
pub fn new_correlation_id() -> String {
    Uuid::new_v4().to_string()
}
