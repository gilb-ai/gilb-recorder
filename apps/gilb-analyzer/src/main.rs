//! Shannon CLI.
//!
//! - `find` — run the therblig-finder job's prompt as `claude -p` over gilb-mcp,
//!   parse the emitted findings, POST each to the job's endpoint, record the run.
//!   `--dry-run` does all of it except touch the network. (gilb-web validates +
//!   dedups per kind.)
//! - `run` — loop the job on its server-controlled cadence (in-process daemon).
//! - `backfill` — manually analyze a historical range in chunks (catch up on a
//!   backlog recorded before the analyzer shipped). One run per chunk; empty
//!   chunks are skipped.
//!
//! Both need enterprise credentials (`gilb_config::load_credentials`); without
//! them the analyzer is inert (Tier-1, local-only capture continues elsewhere).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

mod logging;

use gilb_analyzer::claude::ClaudeRunner;
use gilb_analyzer::config::ensure_config;
use gilb_analyzer::pipeline::{run_job, FindSummary, Window};
use gilb_analyzer::run::Outcome;
use gilb_analyzer::web::Web;

#[derive(Parser, Debug)]
#[command(
    name = "gilb-analyzer",
    about = "Shannon — find Therbligs via claude over gilb-mcp"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the analysis job over a window and POST findings to gilb-web.
    Find {
        /// DB path (default: <Documents>/gilb/db.sqlite).
        #[arg(long)]
        db: Option<PathBuf>,
        /// Window start (default: now - cadence).
        #[arg(long)]
        since: Option<String>,
        /// Build + analyze + parse, but touch no network (no push).
        #[arg(long)]
        dry_run: bool,
    },
    /// Loop `find` on the server-controlled cadence (in-process daemon).
    Run {
        /// DB path (default: <Documents>/gilb/db.sqlite).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Manually analyze recorded history in chunks (catch-up on a backlog
    /// recorded before the analyzer shipped). Walks up to now, one `claude` run
    /// per chunk; empty chunks are skipped (no cost). By default starts from the
    /// earliest recorded action; `--days N` limits it to the last N days.
    Backfill {
        /// DB path (default: <Documents>/gilb/db.sqlite).
        #[arg(long)]
        db: Option<PathBuf>,
        /// Only the last N days (default: from the earliest recorded action).
        #[arg(long)]
        days: Option<u64>,
        /// Window size per chunk in seconds (default: the job's cadence).
        #[arg(long)]
        chunk_secs: Option<u64>,
        /// Pause between chunks in seconds, to throttle (default: 0).
        #[arg(long, default_value_t = 0)]
        gap_secs: u64,
        /// Analyze + parse, but touch no network (no push).
        #[arg(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Held until the process exits so the file appender flushes on shutdown.
    let _log_guard = logging::init_tracing();

    let cli = Cli::parse();
    match cli.cmd {
        Command::Find { db, since, dry_run } => cmd_find(db, since, dry_run).await,
        Command::Run { db } => cmd_run(db).await,
        Command::Backfill {
            db,
            days,
            chunk_secs,
            gap_secs,
            dry_run,
        } => cmd_backfill(db, days, chunk_secs, gap_secs, dry_run).await,
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

/// Build a runner from the environment:
/// - `claude` binary: `GILB_CLAUDE_BIN`, else probed (see `resolve_claude_bin`).
/// - MCP config: the sibling `gilb-mcp` (see `sibling_mcp_config`), overridable
///   by `GILB_MCP_CONFIG` (an inline JSON string or a file path — `claude`
///   accepts either).
/// - Model: `GILB_CLAUDE_MODEL` (else Claude Code's own default).
fn runner_from_env() -> ClaudeRunner {
    let mut runner = ClaudeRunner::new().bin(gilb_analyzer::claude::resolve_claude_bin());
    if let Some(cfg) = gilb_analyzer::claude::sibling_mcp_config() {
        runner = runner.mcp_config(cfg);
    }
    if let Ok(cfg) = std::env::var("GILB_MCP_CONFIG") {
        runner = runner.mcp_config(cfg);
    }
    runner = runner.model(std::env::var("GILB_CLAUDE_MODEL").ok());
    runner
}

async fn cmd_find(db: Option<PathBuf>, since: Option<String>, dry_run: bool) -> Result<()> {
    let Some(creds) = gilb_config::load_credentials()? else {
        eprintln!("not enterprise-configured (no credentials); the analyzer is inactive");
        return Ok(());
    };
    let pool = open_db(db).await?;
    // One-shot: no prior in-memory cache, so this always fetches the prompt
    // fresh (it is never read from / written to disk).
    let config = ensure_config(&creds, None).await?;
    let runner = runner_from_env();
    let web = Web::new(&creds.gilb_web_url, &creds.token);

    let job = config
        .job("therblig-finder")
        .context("config bundle has no 'therblig-finder' job")?;
    let since = since.unwrap_or_else(|| default_since(job.interval_secs()));
    let to = chrono::Utc::now().to_rfc3339();

    let window = Window {
        from: &since,
        to: &to,
    };
    let summary = run_job(&pool, config.version, job, &runner, &web, window, dry_run).await?;
    print_summary(&summary, dry_run);
    Ok(())
}

async fn cmd_run(db: Option<PathBuf>) -> Result<()> {
    let Some(creds) = gilb_config::load_credentials()? else {
        eprintln!("not enterprise-configured (no credentials); `run` needs Tier-2");
        return Ok(());
    };
    // Backstop for a force-killed parent (the Tauri app): if it dies without
    // sending us SIGTERM, we're reparented — notice and shut down gracefully.
    spawn_parent_death_guard();
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
        let Some(job) = config.job("therblig-finder") else {
            tracing::error!("config has no 'therblig-finder' job; skipping tick");
            tokio::time::sleep(Duration::from_secs(
                gilb_config::DEFAULT_ANALYZE_INTERVAL_SECS,
            ))
            .await;
            continue;
        };
        let interval = job.interval_secs();
        let to = chrono::Utc::now().to_rfc3339();
        let from = default_since(interval);
        let window = Window {
            from: &from,
            to: &to,
        };

        // Race the tick against a shutdown signal so SIGTERM mid-run cancels the
        // in-flight `claude` (kill_on_drop) instead of waiting it out.
        tokio::select! {
            res = run_job(&pool, config.version, job, &runner, &web, window, false) => match res {
                Ok(summary) => tracing::info!(
                    "tick: job={} outcome={:?} created={} deduped={} failed={}",
                    summary.run.job,
                    summary.run.outcome,
                    summary.run.findings_created,
                    summary.run.findings_deduped,
                    summary.run.findings_failed,
                ),
                Err(e) => tracing::error!("tick failed: {e:#}"),
            },
            _ = wait_for_shutdown() => return Ok(()),
        }

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
            _ = wait_for_shutdown() => return Ok(()),
        }
    }
}

/// Resolve when the daemon should stop: SIGTERM (the Tauri supervisor's stop) or
/// SIGINT (Ctrl-C in a terminal). On non-unix, Ctrl-C only.
#[cfg(unix)]
async fn wait_for_shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("cannot install SIGTERM handler: {e}");
            std::future::pending::<()>().await;
            return;
        }
    };
    let mut int = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("cannot install SIGINT handler: {e}");
            std::future::pending::<()>().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => tracing::info!("received SIGTERM; stopping"),
        _ = int.recv() => tracing::info!("received SIGINT; stopping"),
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("received Ctrl-C; stopping");
}

/// Watch for our parent (the app) disappearing. If it's force-killed without
/// sending SIGTERM, we get reparented (ppid changes); raise SIGTERM on ourselves
/// so we exit through the normal graceful path. No-op on non-unix.
#[cfg(unix)]
fn spawn_parent_death_guard() {
    let original = unsafe { libc::getppid() };
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let now = unsafe { libc::getppid() };
            if now != original {
                tracing::warn!("parent gone (ppid {original} -> {now}); shutting down");
                unsafe { libc::raise(libc::SIGTERM) };
                return;
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_parent_death_guard() {}

fn parse_rfc3339(s: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    let dt = chrono::DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("invalid RFC3339 timestamp {s:?} (e.g. 2026-06-01T00:00:00Z)"))?;
    Ok(dt.with_timezone(&chrono::Utc))
}

/// Walk recorded history up to now in `chunk_secs` windows, running one job per
/// chunk. The start is `now - days` if given, else the earliest recorded action.
/// Empty chunks short-circuit inside `run_job` (no claude call), so a backlog
/// spanning idle nights stays cheap. Sequential on purpose — one agentic run at
/// a time keeps the subscription's rate limits happy.
async fn cmd_backfill(
    db: Option<PathBuf>,
    days: Option<u64>,
    chunk_secs: Option<u64>,
    gap_secs: u64,
    dry_run: bool,
) -> Result<()> {
    let Some(creds) = gilb_config::load_credentials()? else {
        eprintln!("not enterprise-configured (no credentials); the analyzer is inactive");
        return Ok(());
    };
    let pool = open_db(db).await?;
    let config = ensure_config(&creds, None).await?;
    let runner = runner_from_env();
    let web = Web::new(&creds.gilb_web_url, &creds.token);
    let job = config
        .job("therblig-finder")
        .context("config bundle has no 'therblig-finder' job")?;

    let chunk = chunk_secs.unwrap_or_else(|| job.interval_secs()).max(1);
    let until_dt = chrono::Utc::now();
    let from0 = match days {
        Some(d) => until_dt - chrono::Duration::days(d as i64),
        None => match gilb_analyzer::db::earliest_action(&pool).await? {
            Some(ts) => parse_rfc3339(&ts)?,
            None => {
                eprintln!("[backfill] no recorded activity; nothing to do");
                return Ok(());
            }
        },
    };
    if until_dt <= from0 {
        eprintln!("[backfill] range is empty; nothing to do");
        return Ok(());
    }
    let step = chrono::Duration::seconds(chunk as i64);
    let span_days = (until_dt - from0).num_seconds() as f64 / 86_400.0;
    let total = ((until_dt - from0).num_seconds() + chunk as i64 - 1) / chunk as i64;
    eprintln!(
        "[backfill] {} .. {} (~{span_days:.1} days, {total} chunks of {chunk}s){}",
        from0.to_rfc3339(),
        until_dt.to_rfc3339(),
        if dry_run { " [dry-run]" } else { "" },
    );

    let (mut produced, mut empty, mut errored) = (0u64, 0u64, 0u64);
    let (mut created, mut deduped, mut failed) = (0i64, 0i64, 0i64);
    let mut input_tokens = 0i64;
    let mut cost = 0f64;
    let mut chunks = 0u64;

    let mut from = from0;
    while from < until_dt {
        let to = (from + step).min(until_dt);
        let (from_s, to_s) = (from.to_rfc3339(), to.to_rfc3339());
        let window = Window {
            from: &from_s,
            to: &to_s,
        };
        let summary = run_job(&pool, config.version, job, &runner, &web, window, dry_run).await?;
        let run = &summary.run;
        match run.outcome {
            Outcome::Produced => produced += 1,
            Outcome::Empty => empty += 1,
            Outcome::Error => errored += 1,
        }
        created += run.findings_created;
        deduped += run.findings_deduped;
        failed += run.findings_failed;
        input_tokens += run.usage.input_tokens;
        cost += run.total_cost_usd.unwrap_or(0.0);
        chunks += 1;
        eprintln!(
            "[backfill] {from_s} .. {to_s} outcome={:?} created={} deduped={} input_tokens={}",
            run.outcome, run.findings_created, run.findings_deduped, run.usage.input_tokens,
        );

        from = to;
        if gap_secs > 0 && from < until_dt {
            tokio::time::sleep(Duration::from_secs(gap_secs)).await;
        }
    }

    eprintln!(
        "[backfill] done: {chunks} chunks (produced={produced} empty={empty} error={errored}) \
created={created} deduped={deduped} failed={failed} input_tokens={input_tokens} \
est_cost_usd={cost:.4}{}",
        if dry_run { " [dry-run, no push]" } else { "" },
    );
    Ok(())
}

fn print_summary(summary: &FindSummary, dry_run: bool) {
    let run = &summary.run;
    if dry_run {
        // No network was touched; show what would be pushed.
        match serde_json::to_string_pretty(&summary.findings) {
            Ok(json) => println!("{json}"),
            Err(e) => eprintln!("failed to render findings: {e}"),
        }
        eprintln!(
            "[dry-run] job={} outcome={:?} would_push={} input_tokens={}",
            run.job,
            run.outcome,
            summary.findings.len(),
            run.usage.input_tokens,
        );
    } else {
        eprintln!(
            "job={} outcome={:?} created={} deduped={} failed={} input_tokens={} cost_usd={:?}",
            run.job,
            run.outcome,
            run.findings_created,
            run.findings_deduped,
            run.findings_failed,
            run.usage.input_tokens,
            run.total_cost_usd,
        );
    }
}
