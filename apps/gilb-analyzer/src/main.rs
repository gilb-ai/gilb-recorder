//! Shannon CLI.
//!
//! - `slice` — reduce + redact recent activity into a de-identified slice (the
//!   original Layer-1 dry-run; works fully local).
//! - `find` — Phase 1: run the therblig-finder prompt as `claude -p` over
//!   gilb-mcp, parse the emitted Therbligs, dedup, push to gilb-web, record the
//!   run. `--dry-run` does all of it except touch the network.
//! - `run` — loop `find` on the server-controlled cadence (in-process daemon).
//!
//! Everything but `slice` needs enterprise credentials
//! (`gilb_config::load_credentials`); without them only `slice` is available
//! (Tier-1, local-only).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use gilb_analyzer::claude::ClaudeRunner;
use gilb_analyzer::config::ensure_config;
use gilb_analyzer::pipeline::{run_find, FindSummary, Window};
use gilb_analyzer::web::Web;
use gilb_analyzer::{db, redact};

const FAR_FUTURE: &str = "9999-12-31T23:59:59Z";

#[derive(Parser, Debug)]
#[command(
    name = "gilb-analyzer",
    about = "Shannon — reduce/redact activity and find Therbligs"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Reduce + redact recent activity into a de-identified slice (local).
    Slice {
        /// DB path (default: ~/.gilb/db.sqlite).
        #[arg(long)]
        db: Option<PathBuf>,
        /// Only actions at/after this ISO8601 time (default: last 1h).
        #[arg(long)]
        since: Option<String>,
        /// Pretty-print the slice JSON.
        #[arg(long)]
        pretty: bool,
    },
    /// Phase 1: find Therbligs in a window and push them to gilb-web.
    Find {
        /// DB path (default: ~/.gilb/db.sqlite).
        #[arg(long)]
        db: Option<PathBuf>,
        /// Window start (default: now - cadence).
        #[arg(long)]
        since: Option<String>,
        /// Build + analyze + parse, but touch no network (no dedup, no push).
        #[arg(long)]
        dry_run: bool,
    },
    /// Loop `find` on the server-controlled cadence (in-process daemon).
    Run {
        /// DB path (default: ~/.gilb/db.sqlite).
        #[arg(long)]
        db: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Command::Slice { db, since, pretty } => cmd_slice(db, since, pretty).await,
        Command::Find { db, since, dry_run } => cmd_find(db, since, dry_run).await,
        Command::Run { db } => cmd_run(db).await,
    }
}

async fn open_db(db: Option<PathBuf>) -> Result<sqlx::SqlitePool> {
    let db_path = match db {
        Some(p) => p,
        None => gilb_config::db_path().context("resolve db path")?,
    };
    gilb_db::open_db_read_only(&db_path)
        .await
        .with_context(|| format!("open db at {}", db_path.display()))
}

fn default_since(secs: u64) -> String {
    (chrono::Utc::now() - chrono::Duration::seconds(secs as i64)).to_rfc3339()
}

async fn cmd_slice(db: Option<PathBuf>, since: Option<String>, pretty: bool) -> Result<()> {
    let pool = open_db(db).await?;
    let since = since.unwrap_or_else(|| default_since(3600));
    let rows = db::load_rows(&pool, &since, FAR_FUTURE).await?;
    let slice = redact(&rows);
    let json = if pretty {
        serde_json::to_string_pretty(&slice)?
    } else {
        serde_json::to_string(&slice)?
    };
    println!("{json}");
    Ok(())
}

/// Build a runner from env: `GILB_CLAUDE_BIN` (default `claude`),
/// `GILB_MCP_CONFIG` (path to the gilb-mcp MCP config), `GILB_CLAUDE_MODEL`.
fn runner_from_env() -> ClaudeRunner {
    let mut runner = ClaudeRunner::new();
    if let Ok(bin) = std::env::var("GILB_CLAUDE_BIN") {
        runner = runner.bin(bin);
    }
    if let Ok(path) = std::env::var("GILB_MCP_CONFIG") {
        runner = runner.mcp_config(PathBuf::from(path));
    }
    runner = runner.model(std::env::var("GILB_CLAUDE_MODEL").ok());
    runner
}

async fn cmd_find(db: Option<PathBuf>, since: Option<String>, dry_run: bool) -> Result<()> {
    let Some(creds) = gilb_config::load_credentials()? else {
        eprintln!("not enterprise-configured (no credentials); only `slice` is available locally");
        return Ok(());
    };
    let pool = open_db(db).await?;
    // One-shot: no prior in-memory cache, so this always fetches the prompt
    // fresh (it is never read from / written to disk).
    let config = ensure_config(&creds, None).await?;
    let runner = runner_from_env();
    let web = Web::new(&creds.gilb_web_url, &creds.token);

    let interval = config
        .job("therblig-finder")
        .map(|j| j.interval_secs())
        .unwrap_or(gilb_config::DEFAULT_ANALYZE_INTERVAL_SECS);
    let since = since.unwrap_or_else(|| default_since(interval));
    let to = chrono::Utc::now().to_rfc3339();

    let window = Window {
        from: &since,
        to: &to,
    };
    let summary = run_find(&pool, &config, &runner, &web, window, dry_run).await?;
    print_summary(&summary, dry_run);
    Ok(())
}

async fn cmd_run(db: Option<PathBuf>) -> Result<()> {
    let Some(creds) = gilb_config::load_credentials()? else {
        eprintln!("not enterprise-configured (no credentials); `run` needs Tier-2");
        return Ok(());
    };
    let pool = open_db(db).await?;
    let runner = runner_from_env();
    let web = Web::new(&creds.gilb_web_url, &creds.token);

    // The config (incl. the private prompt) lives only here, in memory, for the
    // life of the daemon — never on disk. Reused across ticks for a cheap 304.
    let mut cached: Option<gilb_config::AnalyzerConfig> = None;

    loop {
        let config = match ensure_config(&creds, cached.as_ref()).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("config fetch failed, stopping: {e:#}");
                return Err(e);
            }
        };
        cached = Some(config.clone());
        let interval = config
            .job("therblig-finder")
            .map(|j| j.interval_secs())
            .unwrap_or(gilb_config::DEFAULT_ANALYZE_INTERVAL_SECS);
        let to = chrono::Utc::now().to_rfc3339();
        let from = default_since(interval);
        let window = Window {
            from: &from,
            to: &to,
        };

        match run_find(&pool, &config, &runner, &web, window, false).await {
            Ok(summary) => tracing::info!(
                "tick: outcome={:?} created={} deduped={} failed={}",
                summary.run.outcome,
                summary.run.therbligs_created.len(),
                summary.run.therbligs_deduped,
                summary.run.therbligs_failed,
            ),
            Err(e) => tracing::error!("tick failed: {e:#}"),
        }

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("interrupted; stopping");
                return Ok(());
            }
        }
    }
}

fn print_summary(summary: &FindSummary, dry_run: bool) {
    let run = &summary.run;
    if dry_run {
        // No network was touched; show what would be pushed.
        match serde_json::to_string_pretty(&summary.new_therbligs) {
            Ok(json) => println!("{json}"),
            Err(e) => eprintln!("failed to render therbligs: {e}"),
        }
        eprintln!(
            "[dry-run] outcome={:?} would_push={} deduped={} input_tokens={}",
            run.outcome,
            summary.new_therbligs.len(),
            run.therbligs_deduped,
            run.usage.input_tokens,
        );
    } else {
        eprintln!(
            "outcome={:?} created={} deduped={} failed={} input_tokens={} cost_usd={:?}",
            run.outcome,
            run.therbligs_created.len(),
            run.therbligs_deduped,
            run.therbligs_failed,
            run.usage.input_tokens,
            run.total_cost_usd,
        );
    }
}
