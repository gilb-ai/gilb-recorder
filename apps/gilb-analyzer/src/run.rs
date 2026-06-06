//! Per-run accounting. Every `find` tick — whether it produced Therbligs, none,
//! or errored — is recorded to gilb-web (`POST /api/v1/analyzer/runs`) with the
//! token usage Claude Code reported and the volume that was available to
//! analyze. Token cost is per *run* (one agentic run yields 0..N Therbligs
//! together); the server divides by the linked Therbligs to get cost-per-one.

use serde::Serialize;

use crate::claude::{ClaudeResult, Usage};
use crate::db::SourceCounts;

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
/// output didn't parse); otherwise `produced` if any Therblig was created, else
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

#[derive(Debug, Clone, Serialize)]
pub struct UsageOut {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
}

impl From<&Usage> for UsageOut {
    fn from(u: &Usage) -> Self {
        Self {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_tokens: u.cache_read_tokens,
            cache_creation_tokens: u.cache_creation_tokens,
        }
    }
}

/// MCP tool I/O (volume form C) — populated later from stream-json. `None` for
/// the PoC.
#[derive(Debug, Clone, Serialize)]
pub struct McpCounts {
    pub tool_calls: i64,
    pub rows_returned: i64,
    pub approx_bytes_returned: i64,
}

/// The `input` block: how much data the run had / used.
#[derive(Debug, Clone, Serialize)]
pub struct InputBlock {
    pub window_secs: i64,
    pub source: SourceCounts,
    pub llm_input_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpCounts>,
}

/// One row of run accounting — serialized as `{"run": …}` for the endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct RunRecord {
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
    pub usage: UsageOut,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    pub outcome: Outcome,
    pub input: InputBlock,
    pub therbligs_created: Vec<i64>,
    pub therbligs_deduped: i64,
    pub therbligs_failed: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Inputs to assemble a [`RunRecord`] — keeps the constructor call readable.
pub struct RunInputs<'a> {
    pub started_at: String,
    pub finished_at: String,
    pub window_from: &'a str,
    pub window_to: &'a str,
    pub window_secs: i64,
    pub config_version: i64,
    pub source: SourceCounts,
    pub claude: Option<&'a ClaudeResult>,
    /// Server-assigned ids of created Therbligs (those whose id we could read).
    pub created: Vec<i64>,
    /// How many Therbligs were created (or, in a dry run, *would* be) — drives
    /// the outcome. May exceed `created.len()` when the server omits an id.
    pub created_count: usize,
    pub deduped: i64,
    pub failed: i64,
    pub error: Option<String>,
}

impl RunRecord {
    /// Build a run record from the run's outcome. `claude` is `None` when the
    /// subprocess itself failed (no usage available).
    pub fn assemble(inp: RunInputs<'_>) -> Self {
        let errored = inp.error.is_some() || inp.claude.map(|c| c.is_error).unwrap_or(false);
        let usage = inp
            .claude
            .map(|c| UsageOut::from(&c.usage))
            .unwrap_or(UsageOut {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            });
        let llm_input_tokens = usage.input_tokens;
        RunRecord {
            started_at: inp.started_at,
            finished_at: inp.finished_at,
            window_from: inp.window_from.to_string(),
            window_to: inp.window_to.to_string(),
            model: inp.claude.and_then(|c| c.model.clone()),
            config_version: inp.config_version,
            num_turns: inp.claude.and_then(|c| c.num_turns),
            duration_ms: inp.claude.and_then(|c| c.duration_ms),
            usage,
            total_cost_usd: inp.claude.and_then(|c| c.total_cost_usd),
            outcome: classify_outcome(errored, inp.created_count),
            input: InputBlock {
                window_secs: inp.window_secs,
                source: inp.source,
                llm_input_tokens,
                mcp: None,
            },
            therbligs_created: inp.created,
            therbligs_deduped: inp.deduped,
            therbligs_failed: inp.failed,
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
        ClaudeResult {
            text: "[]".to_string(),
            usage: Usage {
                input_tokens,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            total_cost_usd: Some(0.05),
            num_turns: Some(4),
            duration_ms: Some(1000),
            model: Some("claude-opus-4-8".to_string()),
            is_error,
        }
    }

    fn inputs<'a>(c: Option<&'a ClaudeResult>, created: Vec<i64>) -> RunInputs<'a> {
        RunInputs {
            started_at: "2026-06-06T00:00:00Z".to_string(),
            finished_at: "2026-06-06T00:01:00Z".to_string(),
            window_from: "2026-06-06T00:00:00Z",
            window_to: "2026-06-06T01:00:00Z",
            window_secs: 3600,
            config_version: 7,
            source: SourceCounts::default(),
            claude: c,
            created_count: created.len(),
            created,
            deduped: 0,
            failed: 0,
            error: None,
        }
    }

    #[test]
    fn assembles_produced_run_with_usage() {
        let c = claude(48211, false);
        let rec = RunRecord::assemble(inputs(Some(&c), vec![12, 13]));
        assert_eq!(rec.outcome, Outcome::Produced);
        assert_eq!(rec.usage.input_tokens, 48211);
        assert_eq!(rec.input.llm_input_tokens, 48211);
        assert_eq!(rec.therbligs_created, vec![12, 13]);
        assert_eq!(rec.model.as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn empty_run_still_records_usage() {
        let c = claude(5000, false);
        let rec = RunRecord::assemble(inputs(Some(&c), vec![]));
        assert_eq!(rec.outcome, Outcome::Empty);
        assert_eq!(rec.usage.input_tokens, 5000);
    }

    #[test]
    fn errored_run_without_claude_has_zero_usage() {
        let mut inp = inputs(None, vec![]);
        inp.error = Some("claude timed out".to_string());
        let rec = RunRecord::assemble(inp);
        assert_eq!(rec.outcome, Outcome::Error);
        assert_eq!(rec.usage.input_tokens, 0);
        assert_eq!(rec.error.as_deref(), Some("claude timed out"));
    }

    #[test]
    fn serializes_under_run_key_shape() {
        let c = claude(100, false);
        let rec = RunRecord::assemble(inputs(Some(&c), vec![1]));
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["outcome"], "produced");
        assert_eq!(v["input"]["llm_input_tokens"], 100);
        assert_eq!(v["usage"]["input_tokens"], 100);
        assert!(v["input"].get("mcp").is_none());
    }
}
