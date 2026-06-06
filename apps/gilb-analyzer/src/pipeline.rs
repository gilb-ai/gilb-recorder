//! Job orchestration: pull a job's prompt from config, run the LLM as
//! `claude -p` over gilb-mcp, parse the emitted findings (opaque JSON), POST
//! each to the job's endpoint (gilb-web validates + dedups per kind), and record
//! the run. Kind-agnostic — the recorder never inspects the finding's shape.
//!
//! `build_trigger` / `window_secs` are pure (unit-tested); `run_job` is the IO seam.

use anyhow::Result;
use gilb_config::Job;
use serde_json::Value;

use crate::claude::{ClaudeResult, ClaudeRunner};
use crate::db;
use crate::findings::parse_findings;
use crate::run::{RunInputs, RunRecord};
use crate::web::{PostOutcome, Web};

/// Append the emit-only trigger to the server-delivered prompt: bound the
/// window, read via gilb-mcp, emit a JSON array, push nothing (Rust owns egress).
pub fn build_trigger(prompt: &str, from: &str, to: &str) -> String {
    format!(
        "{prompt}\n\n---\n\nTime window: from {from} to {to} (RFC3339 UTC). Read the \
recorded activity for this window via the gilb-mcp tools. Output ONLY a JSON array of \
findings matching the schema above and nothing else. Do NOT POST anything, do NOT run \
curl, do NOT call any HTTP endpoint — emitting the JSON is your entire job.\n"
    )
}

/// Result of a job run, for the CLI to print.
pub struct FindSummary {
    pub run: RunRecord,
    /// The findings emitted (would be / were pushed) — for `--dry-run`.
    pub findings: Vec<Value>,
}

/// The RFC3339 `[from, to]` time window a run analyzes.
#[derive(Debug, Clone, Copy)]
pub struct Window<'a> {
    pub from: &'a str,
    pub to: &'a str,
}

fn window_secs(from: &str, to: &str) -> i64 {
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
    match (parse(from), parse(to)) {
        (Some(a), Some(b)) => (b - a).num_seconds().max(0),
        _ => 0,
    }
}

/// Run one job over the window. In `dry_run` no network is touched: pushes are
/// skipped and the run is not posted; the summary shows what would have happened.
pub async fn run_job(
    db: &sqlx::SqlitePool,
    config_version: i64,
    job: &Job,
    runner: &ClaudeRunner,
    web: &Web,
    window: Window<'_>,
    dry_run: bool,
) -> Result<FindSummary> {
    let Window { from, to } = window;
    let run_id = uuid::Uuid::new_v4().to_string();
    let started_at = chrono::Utc::now().to_rfc3339();

    // Volume available in the window (form A) — computed independently of the LLM.
    let rows = db::load_rows(db, from, to).await?;
    let trees = db::count_tree_snapshots(db, from, to).await.unwrap_or(0);
    let source = db::source_counts(&rows, trees);

    let full = build_trigger(&job.prompt, from, to);
    let claude_res = runner.run(&full).await;

    // Capture everything so a run record is posted on every outcome.
    let mut created = 0i64;
    let mut deduped = 0i64;
    let mut failed = 0i64;
    let mut error: Option<String> = None;
    let mut findings: Vec<Value> = Vec::new();
    let mut claude_for_record: Option<ClaudeResult> = None;

    match claude_res {
        Err(e) => error = Some(format!("{e:#}")),
        Ok(result) => {
            match parse_findings(&result.text) {
                Err(e) => error = Some(format!("parse failed: {e:#}")),
                Ok(items) => {
                    if dry_run {
                        created = items.len() as i64;
                    } else {
                        let (c, d, f, rate_limited) =
                            push_findings(web, &job.post_to, &items, &run_id).await;
                        created = c;
                        deduped = d;
                        failed = f;
                        if rate_limited {
                            error =
                                Some("rate limited (429): run aborted before pushing all".into());
                        }
                    }
                    findings = items;
                }
            }
            if result.is_error && error.is_none() {
                error = Some("claude reported is_error".into());
            }
            claude_for_record = Some(result);
        }
    }

    let finished_at = chrono::Utc::now().to_rfc3339();

    let run = RunRecord::assemble(RunInputs {
        run_id,
        job: job.name.clone(),
        started_at,
        finished_at,
        window_from: from,
        window_to: to,
        window_secs: window_secs(from, to),
        config_version,
        source,
        claude: claude_for_record.as_ref(),
        created,
        deduped,
        failed,
        error,
    });

    if !dry_run {
        if let Err(e) = web.post_run(&run).await {
            tracing::warn!("failed to post run record: {e:#}");
        }
    }

    Ok(FindSummary { run, findings })
}

/// POST each finding one at a time to `post_to`, stamping `run_id`. Returns
/// (created, deduped, failed, rate_limited). Stops on 429; counts 409 as
/// deduped; logs and continues on other errors (server owns validation/dedup).
async fn push_findings(
    web: &Web,
    post_to: &str,
    items: &[Value],
    run_id: &str,
) -> (i64, i64, i64, bool) {
    let mut created = 0i64;
    let mut deduped = 0i64;
    let mut failed = 0i64;
    for item in items {
        match web.post_finding(post_to, item, run_id).await {
            Ok(PostOutcome::Created) => created += 1,
            Ok(PostOutcome::Duplicate) => deduped += 1,
            Ok(PostOutcome::RateLimited) => return (created, deduped, failed, true),
            Ok(PostOutcome::Failed { status, body }) => {
                failed += 1;
                tracing::warn!("push failed (HTTP {status}) to {post_to}: {}", body.trim());
            }
            Err(e) => {
                failed += 1;
                tracing::warn!("push errored to {post_to}: {e:#}");
            }
        }
    }
    (created, deduped, failed, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_embeds_window_and_forbids_push() {
        let t = build_trigger("BASE PROMPT", "F", "T");
        assert!(t.starts_with("BASE PROMPT"));
        assert!(t.contains("from F to T"));
        assert!(t.contains("Do NOT POST"));
        assert!(t.contains("gilb-mcp"));
    }

    #[test]
    fn window_secs_computes_span() {
        assert_eq!(
            window_secs("2026-06-06T00:00:00Z", "2026-06-06T01:00:00Z"),
            3600
        );
        assert_eq!(window_secs("bad", "also bad"), 0);
    }
}
