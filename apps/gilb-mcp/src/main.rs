//! gilb-mcp — stdio MCP-server exposing recorded activity from `~/.gilb/db.sqlite`
//! to Claude Code / other MCP clients.
//!
//! Launched on demand by the client (not a daemon). Opens the DB **read-only**;
//! the Tauri-app keeps writing concurrently — SQLite WAL handles this.

mod queries;
mod range;
mod service;

use anyhow::Result;
use rmcp::{transport::stdio, ServiceExt};
use tracing_subscriber::EnvFilter;

use crate::service::GilbService;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let db_path = resolve_db_path()?;
    tracing::info!(?db_path, "gilb-mcp: opening db");
    let db = gilb_db::open_db_read_only(&db_path).await?;
    let service = GilbService::new(db);

    tracing::info!("gilb-mcp: serving stdio");
    let conn = service.serve(stdio()).await?;
    conn.waiting().await?;
    Ok(())
}

/// Log to stderr — stdout is reserved for JSON-RPC frames.
fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,gilb_mcp=debug")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

/// Env override `GILB_DB` wins over the default `$HOME/.gilb/db.sqlite`.
fn resolve_db_path() -> Result<std::path::PathBuf> {
    if let Ok(p) = std::env::var("GILB_DB") {
        return Ok(std::path::PathBuf::from(p));
    }
    Ok(gilb_config::db_path()?)
}
