//! `sessions` table — start/stop a recording session.

use anyhow::Result;
use chrono::Utc;

use crate::Db;
use gilb_core::SessionId;

const GILB_VERSION: &str = env!("CARGO_PKG_VERSION");

fn host_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unsupported"
    }
}

/// Insert a new row in `sessions` and return its id.
pub async fn start_session(db: &Db) -> Result<SessionId> {
    let now = Utc::now().to_rfc3339();
    let res = sqlx::query(
        "INSERT INTO sessions (started_at, gilb_version, host_os) VALUES (?, ?, ?)",
    )
    .bind(&now)
    .bind(GILB_VERSION)
    .bind(host_os())
    .execute(db)
    .await?;
    Ok(res.last_insert_rowid())
}

/// Mark the session as stopped.
pub async fn stop_session(
    db: &Db,
    session_id: SessionId,
    reason: impl AsRef<str>,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query("UPDATE sessions SET stopped_at = ?, stop_reason = ? WHERE id = ?")
        .bind(&now)
        .bind(reason.as_ref())
        .bind(session_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Count of sessions that are still recording (no `stopped_at`).
pub async fn active_session_count(db: &Db) -> Result<i64> {
    let row: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM sessions WHERE stopped_at IS NULL")
            .fetch_one(db)
            .await?;
    Ok(row.0)
}
