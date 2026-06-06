//! Phase 1 (`find`) orchestration: pull the prompt from config, run the LLM as
//! `claude -p` over gilb-mcp, parse the emitted Therbligs, dedup by title
//! against what gilb-web already has, push the new ones, and record the run.
//!
//! The trigger and dedup are pure (unit-tested); `run_find` is the IO seam.

use std::collections::HashSet;

use anyhow::{Context, Result};
use gilb_config::AnalyzerConfig;

use crate::claude::{ClaudeResult, ClaudeRunner};
use crate::db;
use crate::redact::redact;
use crate::run::{RunInputs, RunRecord};
use crate::therblig::{parse_therbligs, Therblig};
use crate::web::{dedup_key, PostOutcome, Web};

/// Job name looked up in the config bundle.
const FINDER_JOB: &str = "therblig-finder";

/// Append the emit-only trigger to the server-delivered prompt: bound the
/// window, read via gilb-mcp, emit a JSON array, push nothing (Rust owns the
/// egress).
pub fn build_trigger(prompt: &str, from: &str, to: &str) -> String {
    format!(
        "{prompt}\n\n---\n\nTime window: from {from} to {to} (RFC3339 UTC). Read the \
recorded activity for this window via the gilb-mcp tools. Output ONLY a JSON array of \
Therbligs matching the schema and nothing else. Do NOT POST anything, do NOT run curl, \
do NOT call any HTTP endpoint — emitting the JSON is your entire job.\n"
    )
}

/// Split parsed Therbligs into the ones not already seen (by case-insensitive
/// trimmed title) and a count of those dropped as duplicates. Also dedups
/// within the batch itself.
pub fn select_new(therbligs: Vec<Therblig>, existing: &HashSet<String>) -> (Vec<Therblig>, i64) {
    let mut seen = existing.clone();
    let mut new = Vec::new();
    let mut deduped = 0i64;
    for t in therbligs {
        let key = dedup_key(&t.title);
        if seen.insert(key) {
            new.push(t);
        } else {
            deduped += 1;
        }
    }
    (new, deduped)
}

/// Result of a `find` run, for the CLI to print.
pub struct FindSummary {
    pub run: RunRecord,
    /// The Therbligs that were new (would be / were pushed) — for `--dry-run`.
    pub new_therbligs: Vec<Therblig>,
}

/// The RFC3339 `[from, to]` time window a `find` run analyzes.
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

/// Run Phase 1 over the window. In `dry_run` no network is touched: dedup-fetch
/// and pushes are skipped and the run is not posted; the summary shows what would
/// have happened.
pub async fn run_find(
    db: &sqlx::SqlitePool,
    config: &AnalyzerConfig,
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
    let segments = redact(&rows).segments.len();
    let trees = db::count_tree_snapshots(db, from, to).await.unwrap_or(0);
    let source = db::source_counts(&rows, segments, trees);

    let job = config
        .job(FINDER_JOB)
        .with_context(|| format!("config bundle has no '{FINDER_JOB}' job"))?;
    let full = build_trigger(&job.prompt, from, to);

    let claude_res = runner.run(&full).await;

    // Capture everything so a run record is posted on every outcome.
    let mut created_ids: Vec<i64> = Vec::new();
    let mut created_count = 0usize;
    let mut deduped = 0i64;
    let mut failed = 0i64;
    let mut error: Option<String> = None;
    let mut new_therbligs: Vec<Therblig> = Vec::new();
    let mut claude_for_record: Option<ClaudeResult> = None;

    match claude_res {
        Err(e) => {
            error = Some(format!("{e:#}"));
        }
        Ok(result) => {
            match parse_therbligs(&result.text) {
                Err(e) => error = Some(format!("parse failed: {e:#}")),
                Ok(therbligs) => {
                    let existing = if dry_run {
                        HashSet::new()
                    } else {
                        fetch_existing_keys(web, from, to).await
                    };
                    let (new, d) = select_new(therbligs, &existing);
                    deduped = d;
                    if dry_run {
                        created_count = new.len();
                    } else {
                        let (ids, cnt, f, rate_limited) = push_all(web, &new, &run_id).await;
                        created_ids = ids;
                        created_count = cnt;
                        failed = f;
                        if rate_limited {
                            error =
                                Some("rate limited (429): run aborted before pushing all".into());
                        }
                    }
                    new_therbligs = new;
                }
            }
            if result.is_error && error.is_none() {
                error = Some("claude reported is_error".into());
            }
            claude_for_record = Some(result);
        }
    }

    let finished_at = chrono::Utc::now().to_rfc3339();
    // Dry-run pushes nothing, so reflect what *would* be created in the outcome.
    let created_count = if dry_run {
        new_therbligs.len()
    } else {
        created_count
    };

    let run = RunRecord::assemble(RunInputs {
        run_id,
        started_at,
        finished_at,
        window_from: from,
        window_to: to,
        window_secs: window_secs(from, to),
        config_version: config.version,
        source,
        claude: claude_for_record.as_ref(),
        created: created_ids,
        created_count,
        deduped,
        failed,
        error,
    });

    if !dry_run {
        if let Err(e) = web.post_run(&run).await {
            tracing::warn!("failed to post run record: {e:#}");
        }
    }

    Ok(FindSummary { run, new_therbligs })
}

/// Fetch existing titles for dedup; on failure log and proceed with none (per
/// the therblig-finder contract — a dedup-fetch blip must not abort the run).
async fn fetch_existing_keys(web: &Web, from: &str, to: &str) -> HashSet<String> {
    match web.list_therbligs(from, to).await {
        Ok(refs) => refs.iter().map(|r| dedup_key(&r.title)).collect(),
        Err(e) => {
            tracing::warn!("dedup fetch failed ({e:#}); proceeding without dedup context");
            HashSet::new()
        }
    }
}

/// Push each new Therblig one at a time, stamping `run_id` so each links to its
/// run. Returns (created ids, created count, failed count, rate_limited). Stops
/// on 429; logs and continues on other errors.
async fn push_all(web: &Web, new: &[Therblig], run_id: &str) -> (Vec<i64>, usize, i64, bool) {
    let mut ids = Vec::new();
    let mut created = 0usize;
    let mut failed = 0i64;
    for t in new {
        match web.post_therblig(t, run_id).await {
            Ok(PostOutcome::Created { id }) => {
                created += 1;
                if let Some(id) = id {
                    ids.push(id);
                }
            }
            Ok(PostOutcome::RateLimited) => return (ids, created, failed, true),
            Ok(PostOutcome::Failed { status, body }) => {
                failed += 1;
                tracing::warn!(
                    "push failed (HTTP {status}) for '{}': {}",
                    t.title,
                    body.trim()
                );
            }
            Err(e) => {
                failed += 1;
                tracing::warn!("push errored for '{}': {e:#}", t.title);
            }
        }
    }
    (ids, created, failed, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::therblig::{Delegation, Evidence, TherbligStep};

    fn therblig(title: &str) -> Therblig {
        Therblig {
            title: title.to_string(),
            intent_summary: "s".to_string(),
            time_window_from: "2026-06-06T00:00:00Z".to_string(),
            time_window_to: "2026-06-06T00:05:00Z".to_string(),
            steps: vec![TherbligStep {
                label: "a".to_string(),
                delegation: Delegation::Fully,
            }],
            evidence: vec![Evidence {
                captured_at: "2026-06-06T00:00:00Z".to_string(),
                app: "X".to_string(),
                summary: "y".to_string(),
            }],
        }
    }

    #[test]
    fn trigger_embeds_window_and_forbids_push() {
        let t = build_trigger("BASE PROMPT", "F", "T");
        assert!(t.starts_with("BASE PROMPT"));
        assert!(t.contains("from F to T"));
        assert!(t.contains("Do NOT POST"));
        assert!(t.contains("gilb-mcp"));
    }

    #[test]
    fn select_new_drops_known_titles() {
        let mut existing = HashSet::new();
        existing.insert("investor research".to_string());
        let (new, deduped) = select_new(
            vec![therblig("Investor Research"), therblig("New Task")],
            &existing,
        );
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].title, "New Task");
        assert_eq!(deduped, 1);
    }

    #[test]
    fn select_new_dedups_within_batch() {
        let (new, deduped) = select_new(
            vec![therblig("Same"), therblig("same"), therblig("Other")],
            &HashSet::new(),
        );
        assert_eq!(new.len(), 2);
        assert_eq!(deduped, 1);
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
