//! Decide when to persist an AX-tree snapshot.
//!
//! Owned by the normalizer (one per capture session). The normalizer
//! calls [`Snapshotter::capture_on_focus`] from the focus tick after
//! [`AppInfo`] has been updated. The snapshotter:
//!   1. walks the focused window via the platform walker,
//!   2. SimHashes the resulting node bag,
//!   3. asks its [`SnapshotCache`] whether to persist, and
//!   4. returns a [`TreeSnapshot`] when (and only when) we should.
//!
//! The walker is platform-gated. Non-macOS builds always return `None`
//! today — that matches Phase 2 scope (macOS-only capture).

use chrono::Utc;

use gilb_core::{AppInfo, SessionId, TreeSnapshot};

use super::cache::{simhash, SnapshotCache};

#[cfg(target_os = "macos")]
use super::walker_macos::{tokens_for_simhash, walk_focused_window};

pub struct Snapshotter {
    session_id: SessionId,
    cache: SnapshotCache,
}

impl Snapshotter {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            cache: SnapshotCache::new(),
        }
    }

    /// Capture the focused window of `app`'s pid if we have one. Returns
    /// `Some` only when the SimHash dedup says we should persist.
    pub fn capture_on_focus(&mut self, app: &AppInfo) -> Option<TreeSnapshot> {
        let pid = app.pid?;
        let nodes = walk(pid)?;
        let tokens = tokens(&nodes);
        let hash = simhash(&tokens);
        let app_key = app.bundle_id.as_deref().unwrap_or("-");
        let window_key = app.window_title.as_deref().unwrap_or("-");

        if !self.cache.should_store(app_key, window_key, hash) {
            return None;
        }

        let root_json = match serde_json::to_string(&nodes) {
            Ok(s) => s,
            Err(_) => return None,
        };

        Some(TreeSnapshot {
            session_id: self.session_id,
            captured_at: Utc::now(),
            app: app.clone(),
            simhash: hash as i64,
            root_json,
        })
    }
}

#[cfg(target_os = "macos")]
fn walk(pid: i32) -> Option<Vec<super::walker_macos::Node>> {
    walk_focused_window(pid)
}

#[cfg(target_os = "macos")]
fn tokens(nodes: &[super::walker_macos::Node]) -> Vec<String> {
    tokens_for_simhash(nodes)
}

#[cfg(not(target_os = "macos"))]
fn walk(_pid: i32) -> Option<Vec<()>> {
    None
}

#[cfg(not(target_os = "macos"))]
fn tokens(_nodes: &[()]) -> Vec<String> {
    Vec::new()
}
