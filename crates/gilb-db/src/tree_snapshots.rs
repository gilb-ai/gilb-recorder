//! `tree_snapshots` table — periodic AX-tree dumps of the focused window.
//!
//! Schema is defined in `migrations/0001_init.sql`. The Phase 2
//! snapshotter (in `gilb-a11y::tree`) decides when to call us; we just
//! insert.

use anyhow::Result;

use crate::Db;
use gilb_core::{TreeSnapshot, TreeSnapshotId};

/// Insert one [`TreeSnapshot`]. Returns the new rowid.
pub async fn insert_tree_snapshot(db: &Db, snap: &TreeSnapshot) -> Result<TreeSnapshotId> {
    insert_tree_snapshot_with(db, snap).await
}

/// Insert one [`TreeSnapshot`] against any executor — see
/// [`crate::actions::insert_action_with`] for why the batched writer needs the
/// transaction-friendly variant.
pub(crate) async fn insert_tree_snapshot_with<'e, E>(
    executor: E,
    snap: &TreeSnapshot,
) -> Result<TreeSnapshotId>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let captured_at = snap.captured_at.to_rfc3339();
    let res = sqlx::query(
        r#"
        INSERT INTO tree_snapshots (
            session_id, captured_at, app_bundle_id, app_name, window_title,
            browser_url, simhash, root_json
        ) VALUES (?,?,?,?,?,?,?,?)
        "#,
    )
    .bind(snap.session_id)
    .bind(&captured_at)
    .bind(&snap.app.bundle_id)
    .bind(&snap.app.name)
    .bind(&snap.app.window_title)
    .bind(&snap.app.browser_url)
    .bind(snap.simhash)
    .bind(&snap.root_json)
    .execute(executor)
    .await?;

    Ok(res.last_insert_rowid())
}
