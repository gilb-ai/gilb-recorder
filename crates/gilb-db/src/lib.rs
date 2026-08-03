//! SQLite-backed storage for gilb.
//!
//! v0 surface is intentionally small:
//! - [`open_db`] / [`migrate`] — open + apply migrations + PRAGMA tuning.
//! - [`sessions`] — start/stop a recording session.
//! - [`actions`] — insert a captured action.
//! - [`write_batch`] — commit a batch of [`WriterMessage`]s in one transaction
//!   (the engine writer's hot path).
//!
//! Reads live in `gilb-mcp`, not here: this crate is the capture side's write
//! path, and keeping query surface out of it is what stops a slow analytics
//! query from being written into the hot loop by accident.

pub mod actions;
pub mod meetings;
pub mod sessions;
pub mod transcripts;
pub mod tree_snapshots;

use anyhow::{Context, Result};
use gilb_core::WriterMessage;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;
use tracing::info;

/// Returned by [`open_db`]. The pool is configured for one writer + multiple
/// readers; concurrent writers serialize through SQLite's WAL lock.
pub type Db = SqlitePool;

/// Open `path` and apply embedded migrations.
///
/// PRAGMA values are chosen for steady-state low-latency capture — a write
/// arrives on every click and keystroke, and none of them may block the
/// capture thread. Each one is justified inline below.
pub async fn open_db(path: impl AsRef<Path>) -> Result<Db> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create db parent dir {}", parent.display()))?;
    }

    let url = format!("sqlite://{}", path.display());
    let connect_opts = SqliteConnectOptions::from_str(&url)?
        .create_if_missing(true)
        // WAL: concurrent reads alongside one writer without blocking.
        .journal_mode(SqliteJournalMode::Wal)
        // NORMAL: fsync only at checkpoint, not per commit — required for the
        // batched write queue's throughput target.
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        // 5s busy_timeout — survives transient WAL lock contention without
        // surfacing SQLITE_BUSY to callers.
        .busy_timeout(Duration::from_secs(5))
        // 64 MB page cache (negative = KiB). Sized to hold the hot tail of
        // recent actions for read queries from gilb-mcp / UI.
        .pragma("cache_size", "-65536")
        // 256 MB mmap window — reads skip the user-space copy and go straight
        // through the page cache, keeping read latency low while the writer is
        // appending.
        .pragma("mmap_size", "268435456")
        // Spill temp tables / sorters to RAM rather than disk — avoids fsync
        // pressure during ad-hoc analytics queries.
        .pragma("temp_store", "MEMORY")
        // Auto-checkpoint at ~16 MB of WAL (4000 pages × 4 KiB). Keeps WAL
        // bounded so it doesn't grow into tens of GB on long-running sessions.
        .pragma("wal_autocheckpoint", "4000");

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(connect_opts)
        .await
        .with_context(|| format!("failed to open sqlite db at {}", path.display()))?;

    migrate(&pool).await?;
    info!(?path, "opened");
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

/// True when `err`'s chain contains a sqlx migration error — the database on
/// disk carries a migration history incompatible with this binary (modified
/// checksum, a version this binary doesn't know, or a migration that failed
/// to apply). Callers use this to tell "archive the DB and start fresh" apart
/// from environment errors (permissions, disk full) where recreating the
/// database wouldn't help.
pub fn is_migrate_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<sqlx::migrate::MigrateError>()
            .is_some()
            || matches!(
                cause.downcast_ref::<sqlx::Error>(),
                Some(sqlx::Error::Migrate(_))
            )
    })
}

/// Move an incompatible database out of the way so a fresh one can be created
/// at `path`. The main file becomes `<name>.old-<UTC timestamp>` in the same
/// directory, and any `-wal` / `-shm` sidecars are renamed to match so the
/// archived copy stays openable as-is. Returns the archived main-file path.
/// Nothing is deleted — the user can recover the data manually.
pub fn archive_incompatible_db(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("db path {} has no usable file name", path.display()))?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));

    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let mut archived_name = format!("{file_name}.old-{stamp}");
    let mut counter = 1;
    while dir.join(&archived_name).exists() {
        counter += 1;
        archived_name = format!("{file_name}.old-{stamp}.{counter}");
    }

    let archived = dir.join(&archived_name);
    std::fs::rename(path, &archived).with_context(|| {
        format!(
            "failed to archive {} to {}",
            path.display(),
            archived.display()
        )
    })?;
    // SQLite derives sidecar names from the main file's, so they must be
    // renamed in lockstep: a stray old `-wal` next to a fresh db would be
    // rejected (or worse, replayed) on the next open.
    for suffix in ["-wal", "-shm"] {
        let sidecar = dir.join(format!("{file_name}{suffix}"));
        if sidecar.exists() {
            let sidecar_archived = dir.join(format!("{archived_name}{suffix}"));
            std::fs::rename(&sidecar, &sidecar_archived).with_context(|| {
                format!(
                    "failed to archive {} to {}",
                    sidecar.display(),
                    sidecar_archived.display()
                )
            })?;
        }
    }
    info!(from = ?path, to = ?archived, "archived incompatible db");
    Ok(archived)
}

/// Insert a batch of [`WriterMessage`]s in a single transaction.
///
/// One `BEGIN … COMMIT` per batch collapses what used to be N independent
/// commits (each taking the WAL write-lock and fsync'ing at the next
/// checkpoint) into one, which is the whole point of the engine's batched
/// writer. Messages are inserted in arrival order so any future
/// `actions.tree_snapshot_id` correlation stays valid.
///
/// The batch is atomic: if any insert fails the transaction rolls back and the
/// error is returned without persisting any row. The caller (the engine
/// writer) handles that by falling back to per-row inserts, so a single
/// malformed message can't sink the whole batch.
pub async fn write_batch(db: &Db, batch: &[WriterMessage]) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let mut tx = db.begin().await.context("begin write-batch transaction")?;
    for msg in batch {
        match msg {
            WriterMessage::Action(action) => {
                actions::insert_action_with(&mut *tx, action).await?;
            }
            WriterMessage::TreeSnapshot(snap) => {
                tree_snapshots::insert_tree_snapshot_with(&mut *tx, snap).await?;
            }
        }
    }
    tx.commit()
        .await
        .context("commit write-batch transaction")?;
    Ok(())
}

/// Open `path` in **read-only mode** — for processes (like `gilb-mcp`) that
/// must not modify the database. Does **not** create the file or run
/// migrations; the caller is expected to point at an already-initialised DB
/// (typically the same `<Documents>/Gilb/db.sqlite` that the Tauri app writes to).
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

    info!(?path, "opened (read-only)");
    Ok(pool)
}
