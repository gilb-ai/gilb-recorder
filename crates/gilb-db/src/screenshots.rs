//! `screenshots` table — sample screenshot metadata (GILB-80).
//!
//! Schema in `migrations/0008_screenshots.sql`. The image bytes live on disk
//! (`image_path`); we persist only metadata. The screenshot worker in
//! `gilb-a11y` decides when to call us. Shipping/pruning lives in
//! `gilb-shipper` (over `shipped_at`, GILB-93).

use anyhow::Result;

use crate::Db;
use gilb_core::Screenshot;

/// Insert one [`Screenshot`] row. Returns the new rowid.
pub async fn insert_screenshot(db: &Db, shot: &Screenshot) -> Result<i64> {
    insert_screenshot_with(db, shot).await
}

/// Insert against any executor (pooled conn or transaction) — the batched
/// writer passes a transaction, per [`crate::actions::insert_action_with`].
pub(crate) async fn insert_screenshot_with<'e, E>(executor: E, shot: &Screenshot) -> Result<i64>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let captured_at = shot.captured_at.to_rfc3339();
    let res = sqlx::query(
        r#"
        INSERT INTO screenshots (
            session_id, captured_at, app_bundle_id, app_name, window_title,
            screenshot_id, image_path, width, height
        ) VALUES (?,?,?,?,?,?,?,?,?)
        "#,
    )
    .bind(shot.session_id)
    .bind(&captured_at)
    .bind(&shot.app.bundle_id)
    .bind(&shot.app.name)
    .bind(&shot.app.window_title)
    .bind(&shot.screenshot_id)
    .bind(&shot.image_path)
    .bind(shot.width)
    .bind(shot.height)
    .execute(executor)
    .await?;

    Ok(res.last_insert_rowid())
}
