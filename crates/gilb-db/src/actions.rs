//! `actions` table — atomic captured events.

use anyhow::Result;

use crate::Db;
use gilb_core::{Action, ActionId, SessionId};

/// Insert one [`Action`]. Returns the new rowid.
pub async fn insert_action(db: &Db, action: &Action) -> Result<ActionId> {
    insert_action_with(db, action).await
}

/// Insert one [`Action`] against any executor — a pooled connection (`&Db`)
/// or a transaction (`&mut Transaction`). [`insert_action`] is the
/// single-row convenience wrapper; the batched writer in `gilb-db::write_batch`
/// passes a transaction so a whole batch commits with one fsync.
pub(crate) async fn insert_action_with<'e, E>(executor: E, action: &Action) -> Result<ActionId>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let captured_at = action.captured_at.to_rfc3339();
    let element_frame = action
        .element
        .frame
        .as_ref()
        .map(|f| serde_json::to_string(f).unwrap_or_default());
    let extra_json = action
        .extra_json
        .as_ref()
        .map(|j| serde_json::to_string(j).unwrap_or_default());

    let res = sqlx::query(
        r#"
        INSERT INTO actions (
            session_id, captured_at, kind,
            app_bundle_id, app_name, app_pid, window_title, browser_url,
            element_role, element_name, element_value, element_help, element_id, element_frame,
            text_content, password_flag, tree_snapshot_id, extra_json,
            clipboard_op, content_hash
        ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
        "#,
    )
    .bind(action.session_id)
    .bind(&captured_at)
    .bind(action.kind.as_str())
    .bind(&action.app.bundle_id)
    .bind(&action.app.name)
    .bind(action.app.pid)
    .bind(&action.app.window_title)
    .bind(&action.app.browser_url)
    .bind(&action.element.role)
    .bind(&action.element.name)
    .bind(&action.element.value)
    .bind(&action.element.help)
    .bind(&action.element.identifier)
    .bind(&element_frame)
    .bind(&action.text_content)
    .bind(if action.password_flag { 1_i64 } else { 0_i64 })
    .bind(action.tree_snapshot_id)
    .bind(&extra_json)
    .bind(&action.clipboard_op)
    .bind(&action.content_hash)
    .execute(executor)
    .await?;

    Ok(res.last_insert_rowid())
}

/// Count of actions in the given session.
pub async fn count_in_session(db: &Db, session_id: SessionId) -> Result<i64> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM actions WHERE session_id = ?")
        .bind(session_id)
        .fetch_one(db)
        .await?;
    Ok(row.0)
}
