//! Standalone smoke binary: opens DB, starts a session, runs the platform
//! capture for N seconds, prints the row counts, exits cleanly.
//!
//! Used to validate Layer 1 outside of the Tauri shell.

use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tokio::sync::mpsc;
use tracing::{info, warn};

use gilb_a11y::{current_platform, StartContext};
use gilb_config::RecordingSettings;
use gilb_core::WriterMessage;
use gilb_db::{actions, open_db, sessions, tree_snapshots};
use gilb_events::EventBus;

#[derive(Parser, Debug)]
#[command(name = "gilb-a11y-cli", about = "gilb capture smoke runner")]
struct Cli {
    /// How long to run before stopping, in seconds.
    #[arg(long, default_value_t = 3)]
    seconds: u64,

    /// Optional override for the DB path.
    #[arg(long)]
    db: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let db_path = match cli.db {
        Some(p) => p,
        None => {
            gilb_config::ensure_data_dir()?;
            gilb_config::db_path()?
        }
    };
    info!(?db_path, "opening gilb db");
    let db = open_db(&db_path).await?;
    let session_id = sessions::start_session(&db).await?;

    let (writer_tx, mut writer_rx) = mpsc::channel::<WriterMessage>(1024);
    let event_bus = EventBus::new();

    let platform = current_platform();
    info!(name = platform.name(), "starting platform capture");
    let perms = platform.permissions().await;
    info!(?perms, "permissions snapshot");

    let handle = platform
        .start(StartContext {
            session_id,
            writer_tx,
            event_bus: event_bus.clone(),
            settings: RecordingSettings::from_env(),
        })
        .await?;

    let db_for_writer = db.clone();
    let writer = tokio::spawn(async move {
        while let Some(msg) = writer_rx.recv().await {
            match msg {
                WriterMessage::Action(action) => {
                    if let Err(err) = actions::insert_action(&db_for_writer, &action).await {
                        warn!(?err, "failed to insert action");
                    }
                }
                WriterMessage::TreeSnapshot(snap) => {
                    if let Err(err) =
                        tree_snapshots::insert_tree_snapshot(&db_for_writer, &snap).await
                    {
                        warn!(?err, "failed to insert tree_snapshot");
                    }
                }
            }
        }
    });

    tokio::time::sleep(Duration::from_secs(cli.seconds)).await;
    handle.stop().await?;
    drop(writer);

    sessions::stop_session(&db, session_id, "cli-smoke").await?;
    let total = actions::count_in_session(&db, session_id).await?;
    println!("session_id={session_id} actions={total}");
    Ok(())
}
