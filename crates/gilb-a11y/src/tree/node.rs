//! Platform-neutral serialised a11y-tree node.
//!
//! Both the macOS (AX) and Windows (UIA) walkers emit `Vec<Node>` so the
//! persisted `root_json` has one shape regardless of OS, and the SimHash
//! token stream is computed the same way.

use serde::{Deserialize, Serialize};

/// One node in the serialised tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub role: String,
    /// Element title/name. Absent when empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Element value (text-field contents etc.), only when non-empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub depth: u8,
}

/// Token stream for [`crate::tree::cache::simhash`] over a Node list.
pub fn tokens_for_simhash(nodes: &[Node]) -> Vec<String> {
    nodes
        .iter()
        .map(|n| {
            let text = n.name.as_deref().or(n.value.as_deref()).unwrap_or("");
            format!("{}:{}", n.role, text)
        })
        .collect()
}
