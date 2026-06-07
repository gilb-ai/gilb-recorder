//! Per-run accounting. Every job tick — whether it produced findings, none, or
//! errored — is recorded to gilb-web (`POST /api/v1/analyzer/runs`) with the
//! token usage Claude Code reported and the volume that was available to
//! analyze. Token cost is per *run* (one agentic run yields 0..N findings
//! together); the server divides by the linked findings (same `run_id`) to get
//! cost-per-one. `job` tags which analysis kind this run was.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::claude::{ClaudeResult, ModelUsage, Usage};
use crate::db::SourceCounts;

// Token usage is reported with the same field names Claude Code emits, so
// [`crate::claude::Usage`] doubles as the wire shape — no separate type needed.

/// What a run produced — recorded so empty/error runs (their tokens "spent for
/// nothing") are visible too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Produced,
    Empty,
    Error,
}

/// Classify a run from what happened. `errored` wins (e.g. claude failed or the
/// output didn't parse); otherwise `produced` if any finding was created, else
/// `empty`.
pub fn classify_outcome(errored: bool, created: usize) -> Outcome {
    if errored {
        Outcome::Error
    } else if created > 0 {
        Outcome::Produced
    } else {
        Outcome::Empty
    }
}

/// The `input` block: how much data the run had / used. (Form C — MCP tool I/O
/// from stream-json — lands here later; omitted for now.) Token counts live in
/// `usage` / `model_usage`, not here.
#[derive(Debug, Clone, Serialize)]
pub struct InputBlock {
    pub window_secs: i64,
    pub source: SourceCounts,
}

/// One row of run accounting — serialized as `{"run": …}` for the endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct RunRecord {
    /// Client-generated id linking this run to the findings it produced (each is
    /// POSTed with the same `run_id`) — lets a finding show its token cost.
    pub run_id: String,
    /// Which job/kind this run was, e.g. `"therblig-finder"`.
    pub job: String,
    pub started_at: String,
    pub finished_at: String,
    pub window_from: String,
    pub window_to: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub config_version: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_turns: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    pub usage: Usage,
    /// Per-model token usage and cost — every model the run touched. Omitted
    /// when Claude Code reported none (e.g. an errored run with no result).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub model_usage: BTreeMap<String, ModelUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    pub outcome: Outcome,
    pub input: InputBlock,
    /// How many findings the server accepted as new (201).
    pub findings_created: i64,
    /// How many were duplicates the server already had (409).
    pub findings_deduped: i64,
    /// How many failed to POST (non-2xx, non-409).
    pub findings_failed: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Inputs to assemble a [`RunRecord`] — keeps the constructor call readable.
pub struct RunInputs<'a> {
    pub run_id: String,
    pub job: String,
    pub started_at: String,
    pub finished_at: String,
    pub window_from: &'a str,
    pub window_to: &'a str,
    pub window_secs: i64,
    pub config_version: i64,
    pub source: SourceCounts,
    pub claude: Option<&'a ClaudeResult>,
    /// Findings created (or, in a dry run, *would* be) — drives the outcome.
    pub created: i64,
    pub deduped: i64,
    pub failed: i64,
    pub error: Option<String>,
}

impl RunRecord {
    /// Build a run record from the run's outcome. `claude` is `None` when the
    /// subprocess itself failed (no usage available).
    pub fn assemble(inp: RunInputs<'_>) -> Self {
        let errored = inp.error.is_some() || inp.claude.map(|c| c.is_error).unwrap_or(false);
        let usage = inp.claude.map(|c| c.usage.clone()).unwrap_or_default();
        RunRecord {
            run_id: inp.run_id,
            job: inp.job,
            started_at: inp.started_at,
            finished_at: inp.finished_at,
            window_from: inp.window_from.to_string(),
            window_to: inp.window_to.to_string(),
            model: inp.claude.and_then(|c| c.model.clone()),
            config_version: inp.config_version,
            num_turns: inp.claude.and_then(|c| c.num_turns),
            duration_ms: inp.claude.and_then(|c| c.duration_ms),
            usage,
            model_usage: inp
                .claude
                .map(|c| c.model_usage.clone())
                .unwrap_or_default(),
            total_cost_usd: inp.claude.and_then(|c| c.total_cost_usd),
            outcome: classify_outcome(errored, inp.created.max(0) as usize),
            input: InputBlock {
                window_secs: inp.window_secs,
                source: inp.source,
            },
            findings_created: inp.created,
            findings_deduped: inp.deduped,
            findings_failed: inp.failed,
            error: inp.error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_classification() {
        assert_eq!(classify_outcome(true, 3), Outcome::Error);
        assert_eq!(classify_outcome(false, 2), Outcome::Produced);
        assert_eq!(classify_outcome(false, 0), Outcome::Empty);
    }

    fn claude(input_tokens: i64, is_error: bool) -> ClaudeResult {
        let model = "claude-opus-4-8".to_string();
        let model_usage = BTreeMap::from([(
            model.clone(),
            ModelUsage {
                input_tokens,
                output_tokens: 10,
                ..ModelUsage::default()
            },
        )]);
        ClaudeResult {
            text: "[]".to_string(),
            usage: Usage {
                input_tokens,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            model_usage,
            total_cost_usd: Some(0.05),
            num_turns: Some(4),
            duration_ms: Some(1000),
            model: Some(model),
            is_error,
        }
    }

    fn inputs(c: Option<&ClaudeResult>, created: i64) -> RunInputs<'_> {
        RunInputs {
            run_id: "test-run-id".to_string(),
            job: "therblig-finder".to_string(),
            started_at: "2026-06-06T00:00:00Z".to_string(),
            finished_at: "2026-06-06T00:01:00Z".to_string(),
            window_from: "2026-06-06T00:00:00Z",
            window_to: "2026-06-06T01:00:00Z",
            window_secs: 3600,
            config_version: 7,
            source: SourceCounts::default(),
            claude: c,
            created,
            deduped: 0,
            failed: 0,
            error: None,
        }
    }

    #[test]
    fn assembles_produced_run_with_usage() {
        let c = claude(48211, false);
        let rec = RunRecord::assemble(inputs(Some(&c), 2));
        assert_eq!(rec.outcome, Outcome::Produced);
        assert_eq!(rec.usage.input_tokens, 48211);
        assert_eq!(rec.findings_created, 2);
        assert_eq!(rec.job, "therblig-finder");
        assert_eq!(rec.model.as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn empty_run_still_records_usage() {
        let c = claude(5000, false);
        let rec = RunRecord::assemble(inputs(Some(&c), 0));
        assert_eq!(rec.outcome, Outcome::Empty);
        assert_eq!(rec.usage.input_tokens, 5000);
    }

    #[test]
    fn errored_run_without_claude_has_zero_usage() {
        let mut inp = inputs(None, 0);
        inp.error = Some("claude timed out".to_string());
        let rec = RunRecord::assemble(inp);
        assert_eq!(rec.outcome, Outcome::Error);
        assert_eq!(rec.usage.input_tokens, 0);
        assert_eq!(rec.error.as_deref(), Some("claude timed out"));
    }

    #[test]
    fn serializes_under_run_key_shape() {
        let c = claude(100, false);
        let rec = RunRecord::assemble(inputs(Some(&c), 1));
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["run_id"], "test-run-id");
        assert_eq!(v["job"], "therblig-finder");
        assert_eq!(v["outcome"], "produced");
        assert_eq!(v["findings_created"], 1);
        assert_eq!(v["usage"]["input_tokens"], 100);
        assert_eq!(v["model_usage"]["claude-opus-4-8"]["input_tokens"], 100);
        assert_eq!(v["model_usage"]["claude-opus-4-8"]["output_tokens"], 10);
    }

    #[test]
    fn omits_empty_model_usage() {
        let mut inp = inputs(None, 0);
        inp.error = Some("claude timed out".to_string());
        let rec = RunRecord::assemble(inp);
        let v = serde_json::to_value(&rec).unwrap();
        assert!(v.get("model_usage").is_none());
    }
}
