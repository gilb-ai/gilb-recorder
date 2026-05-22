//! SQLite-backed storage for gilb.
//!
//! v0 surface is intentionally small:
//! - [`open_db`] / [`migrate`] — open + apply migrations + PRAGMA tuning.
//! - [`sessions`] — start/stop a recording session.
//! - [`actions`] — insert a captured action.
//!
//! Batched write-queue (Phase 3) and FTS5 search (Phase 5) live in
//! separate modules added by later phases.

pub mod actions;
pub mod sessions;

use anyhow::{Context, Result};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::SqlitePool;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;
use tracing::info;

/// Returned by [`open_db`]. The pool is configured for one writer + multiple
/// readers; concurrent writers serialize through SQLite's WAL lock.
pub type Db = SqlitePool;

/// Open `path` and apply embedded migrations.
///
/// PRAGMA values are chosen for steady-state low-latency capture; rationale in
/// `spec.md §4`.
pub async fn open_db(path: impl AsRef<Path>) -> Result<Db> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create db parent dir {}", parent.display()))?;
    }

    let url = format!("sqlite://{}", path.display());
    let connect_opts = SqliteConnectOptions::from_str(&url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5))
        .pragma("cache_size", "-65536")
        .pragma("mmap_size", "268435456")
        .pragma("temp_store", "MEMORY")
        .pragma("wal_autocheckpoint", "4000");

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(connect_opts)
        .await
        .with_context(|| format!("failed to open sqlite db at {}", path.display()))?;

    migrate(&pool).await?;
    info!(?path, "gilb-db open");
    Ok(pool)
}

/// Apply embedded migrations.
pub async fn migrate(db: &Db) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(db)
        .await
        .context("running gilb-db migrations")?;
    Ok(())
}
