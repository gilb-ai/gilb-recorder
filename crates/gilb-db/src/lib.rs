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
pub mod tree_snapshots;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
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
/// `spec.md §4` and `research/06-layer1-capture-quality.md §2.5`. Each pragma
/// below has an inline citation to the research note that justifies it.
pub async fn open_db(path: impl AsRef<Path>) -> Result<Db> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create db parent dir {}", parent.display()))?;
    }

    let url = format!("sqlite://{}", path.display());
    let connect_opts = SqliteConnectOptions::from_str(&url)?
        .create_if_missing(true)
        // WAL: concurrent reads alongside one writer without blocking (§2.5).
        .journal_mode(SqliteJournalMode::Wal)
        // NORMAL: fsync only at checkpoint, not per commit — required for the
        // batched write queue's throughput target (§2.5, §2.6).
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        // 5s busy_timeout — survives transient WAL lock contention without
        // surfacing SQLITE_BUSY to callers (§3.5).
        .busy_timeout(Duration::from_secs(5))
        // 64 MB page cache (negative = KiB). Sized to hold the hot tail of
        // recent actions for read queries from gilb-mcp / UI (§2.5).
        .pragma("cache_size", "-65536")
        // 256 MB mmap window — reads skip the user-space copy and go straight
        // through the page cache, keeping read latency low while the writer is
        // appending (§2.5).
        .pragma("mmap_size", "268435456")
        // Spill temp tables / sorters to RAM rather than disk — avoids fsync
        // pressure during ad-hoc analytics queries (§2.5).
        .pragma("temp_store", "MEMORY")
        // Auto-checkpoint at ~16 MB of WAL (4000 pages × 4 KiB). Keeps WAL
        // bounded so it doesn't grow into tens of GB on long-running sessions
        // (§4.5).
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

/// Open `path` in **read-only mode** — for processes (like `gilb-mcp`) that
/// must not modify the database. Does **not** create the file or run
/// migrations; the caller is expected to point at an already-initialised DB
/// (typically the same `~/.gilb/db.sqlite` that the Tauri app writes to).
///
/// Concurrent reads alongside the Tauri-app's writes are safe — `gilb-db.rs`
/// already enables WAL journaling.
pub async fn open_db_read_only(path: impl AsRef<Path>) -> Result<Db> {
    let path = path.as_ref();
    let url = format!("sqlite://{}", path.display());
    let connect_opts = SqliteConnectOptions::from_str(&url)?
        .read_only(true)
        .create_if_missing(false)
        .immutable(false)
        .busy_timeout(Duration::from_secs(5))
        .pragma("cache_size", "-32768")
        .pragma("temp_store", "MEMORY");

    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(connect_opts)
        .await
        .with_context(|| format!("failed to open sqlite db read-only at {}", path.display()))?;

    info!(?path, "gilb-db open (read-only)");
    Ok(pool)
}
