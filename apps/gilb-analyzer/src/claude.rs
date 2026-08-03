//! Run the analysis LLM as a `claude -p` subprocess.
//!
//! For the PoC the LLM is Claude Code on the operator's subscription (not the
//! Anthropic API, not a proxy): we spawn `claude -p --output-format json`,
//! optionally attaching `gilb-mcp` via `--mcp-config` (inline JSON, plus
//! `--strict-mcp-config` so only our server is used) so the model can read the
//! live DB read-only. The prompt goes in on stdin; the single `result` JSON
//! object comes back on stdout, carrying both the model's text answer and the
//! token `usage` we record per run.
//!
//! `resolve_claude_bin` / `sibling_mcp_config` cope with a bundled `.app`: when
//! launched from Finder the process PATH is minimal and `gilb-mcp` lives next to
//! us in `Contents/MacOS/`, so we probe known install dirs for `claude` and
//! point it at the sibling `gilb-mcp` by absolute path.
//!
//! `parse_result` is pure (unit-tested with a canned object). `ClaudeRunner`
//! owns the spawn/timeout/IO and is exercised by an integration test that puts
//! a fake `claude` on disk — no network, no real model.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Default wall-clock cap for one agentic run.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Token usage for one run, normalized from Claude Code's `usage` block. Field
/// names double as the gilb-web run-record wire shape (see `run`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
}

/// Per-model usage Claude Code reports under `modelUsage` — one entry per model
/// a single run touched (e.g. the main model plus a small helper). Field names
/// double as the gilb-web wire shape (snake_case); deserialized from Claude
/// Code's camelCase via [`WireModelUsage`].
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ModelUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cost_usd: f64,
}

/// Parsed outcome of a `claude -p --output-format json` invocation.
#[derive(Debug)]
pub struct ClaudeResult {
    /// The model's text answer (the `result` field) — fed to `parse_therbligs`.
    pub text: String,
    pub usage: Usage,
    /// Per-model usage (`modelUsage`): every model the run used, with its own
    /// token counts and cost. Empty if Claude Code didn't report it.
    pub model_usage: BTreeMap<String, ModelUsage>,
    pub total_cost_usd: Option<f64>,
    pub num_turns: Option<i64>,
    pub duration_ms: Option<i64>,
    /// The run's primary model — the `modelUsage` entry with the most tokens
    /// (falls back to a top-level `model` field on older Claude Code). `None`
    /// when neither is present.
    pub model: Option<String>,
    /// Claude Code reported the run itself errored (we still keep `usage`).
    pub is_error: bool,
}

#[derive(Debug, Deserialize)]
struct WireResult {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    num_turns: Option<i64>,
    #[serde(default)]
    duration_ms: Option<i64>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<WireUsage>,
    /// Per-model usage map. Newer Claude Code reports this instead of a
    /// top-level `model`.
    #[serde(rename = "modelUsage", default)]
    model_usage: BTreeMap<String, WireModelUsage>,
}

#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    cache_read_input_tokens: i64,
    #[serde(default)]
    cache_creation_input_tokens: i64,
}

/// One `modelUsage` entry as Claude Code emits it (camelCase). Claude also
/// reports `webSearchRequests`, `contextWindow`, `maxOutputTokens` per model;
/// we don't track those (not token usage), so they're ignored on deserialize.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireModelUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    cache_read_input_tokens: i64,
    #[serde(default)]
    cache_creation_input_tokens: i64,
    #[serde(default, rename = "costUSD")]
    cost_usd: f64,
}

impl From<WireModelUsage> for ModelUsage {
    fn from(w: WireModelUsage) -> Self {
        ModelUsage {
            input_tokens: w.input_tokens,
            output_tokens: w.output_tokens,
            cache_read_tokens: w.cache_read_input_tokens,
            cache_creation_tokens: w.cache_creation_input_tokens,
            cost_usd: w.cost_usd,
        }
    }
}

impl ModelUsage {
    /// Total tokens attributed to this model — used to pick a run's primary one.
    fn total_tokens(&self) -> i64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_creation_tokens
    }
}

/// Parse the single `result` JSON object Claude Code prints under
/// `--output-format json`. Pure; tolerant of missing optional fields.
pub fn parse_result(stdout: &str) -> Result<ClaudeResult> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        bail!("claude produced no output");
    }
    let wire: WireResult = serde_json::from_str(trimmed).with_context(|| {
        format!(
            "claude output is not the expected result JSON: {}",
            crate::util::snippet(trimmed)
        )
    })?;
    let usage = wire.usage.unwrap_or_default();
    let model_usage: BTreeMap<String, ModelUsage> = wire
        .model_usage
        .into_iter()
        .map(|(k, v)| (k, v.into()))
        .collect();
    // Primary model: prefer a top-level `model` (older Claude Code), else the
    // `modelUsage` entry that consumed the most tokens.
    let model = wire.model.or_else(|| {
        model_usage
            .iter()
            .max_by_key(|(_, u)| u.total_tokens())
            .map(|(name, _)| name.clone())
    });
    Ok(ClaudeResult {
        text: wire.result.unwrap_or_default(),
        usage: Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_input_tokens,
            cache_creation_tokens: usage.cache_creation_input_tokens,
        },
        model_usage,
        total_cost_usd: wire.total_cost_usd,
        num_turns: wire.num_turns,
        duration_ms: wire.duration_ms,
        model,
        is_error: wire.is_error,
    })
}

/// Spawns `claude -p` and captures its result. Configurable so an integration
/// test can point `bin` at a fake; in production `bin` defaults to `claude` on
/// `PATH`.
#[derive(Debug)]
pub struct ClaudeRunner {
    bin: String,
    /// Inline MCP-config JSON (see `sibling_mcp_config`), passed as a string to
    /// `--mcp-config`; `None` runs the model without any MCP server.
    mcp_config: Option<String>,
    model: Option<String>,
    skip_permissions: bool,
    timeout: Duration,
}

impl Default for ClaudeRunner {
    fn default() -> Self {
        Self {
            bin: "claude".to_string(),
            mcp_config: None,
            model: None,
            // PoC: auto-approve the model's tool calls (read-only gilb-mcp).
            // Scoped permissions land with the daemon — see plan open items.
            skip_permissions: true,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl ClaudeRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the `claude` binary (tests point this at a fake script).
    pub fn bin(mut self, bin: impl Into<String>) -> Self {
        self.bin = bin.into();
        self
    }

    /// Attach the inline MCP-config JSON describing `gilb-mcp` (see
    /// `sibling_mcp_config`). Passed verbatim to `--mcp-config`.
    pub fn mcp_config(mut self, json: impl Into<String>) -> Self {
        self.mcp_config = Some(json.into());
        self
    }

    /// Pin a model id (otherwise Claude Code's own default is used).
    pub fn model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    pub fn skip_permissions(mut self, skip: bool) -> Self {
        self.skip_permissions = skip;
        self
    }

    /// Run the prompt and return the parsed result. The prompt is fed on stdin.
    pub async fn run(&self, prompt: &str) -> Result<ClaudeResult> {
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.arg("-p").arg("--output-format").arg("json");
        if self.skip_permissions {
            cmd.arg("--dangerously-skip-permissions");
        }
        if let Some(json) = &self.mcp_config {
            // Inline JSON + strict: use only our gilb-mcp, never the user's
            // globally-configured MCP servers (keeps the run deterministic).
            cmd.arg("--mcp-config").arg(json).arg("--strict-mcp-config");
        }
        if let Some(model) = &self.model {
            cmd.arg("--model").arg(model);
        }
        // A bundled .app inherits a minimal PATH; widen it so `claude` (and, for
        // the npm build, its `node`) resolve.
        cmd.env("PATH", augmented_path());
        // If the run future is dropped (daemon caught a shutdown signal mid-tick)
        // kill `claude` too — otherwise it keeps burning the subscription.
        cmd.kill_on_drop(true);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn `{}`", self.bin))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(prompt.as_bytes())
                .await
                .context("failed to write prompt to claude stdin")?;
            stdin.shutdown().await.ok();
        }

        let mut stdout = child.stdout.take().context("claude stdout missing")?;
        let mut stderr = child.stderr.take().context("claude stderr missing")?;
        let mut out_buf = Vec::new();
        let mut err_buf = Vec::new();

        // Drain both pipes concurrently (avoids a full-pipe deadlock), then
        // reap — all under the timeout. Kill the child if it overruns.
        let collect = async {
            tokio::try_join!(
                stdout.read_to_end(&mut out_buf),
                stderr.read_to_end(&mut err_buf),
            )
            .context("failed reading claude output")?;
            child.wait().await.context("failed to wait on claude")
        };

        let status = match tokio::time::timeout(self.timeout, collect).await {
            Ok(res) => res?,
            Err(_) => {
                let _ = child.start_kill();
                bail!("claude timed out after {:?}", self.timeout);
            }
        };

        if !status.success() {
            bail!(
                "claude exited with {}: {}",
                status,
                String::from_utf8_lossy(&err_buf).trim()
            );
        }

        parse_result(&String::from_utf8_lossy(&out_buf))
    }
}

/// Locate the `claude` binary. `GILB_CLAUDE_BIN` overrides everything; the
/// probe order and the reason for probing at all (a bundled `.app` has a
/// minimal PATH) live in gilb-config, shared with the ACP assist backend.
pub fn resolve_claude_bin() -> String {
    gilb_config::resolve_agent_bin("claude", "GILB_CLAUDE_BIN")
}

/// PATH for the spawned `claude`, so an npm-installed CLI finds its `node`
/// even from a bundle.
fn augmented_path() -> String {
    gilb_config::agent_path_env()
}

/// Build the inline MCP-config JSON pointing `claude` at the `gilb-mcp` binary
/// sitting next to our own executable (true in the bundle's `Contents/MacOS/`
/// and in `target/<profile>/` during dev). `None` if we can't resolve our path
/// or the sibling is missing — the caller then runs without MCP.
pub fn sibling_mcp_config() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let mcp = exe.parent()?.join("gilb-mcp");
    if !mcp.is_file() {
        return None;
    }
    let cfg = serde_json::json!({
        "mcpServers": { "gilb-mcp": { "command": mcp.to_string_lossy() } }
    });
    Some(cfg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"{
        "type": "result", "subtype": "success", "is_error": false,
        "num_turns": 7, "duration_ms": 42000, "total_cost_usd": 0.12,
        "model": "claude-opus-4-8",
        "result": "[]",
        "usage": {"input_tokens": 48211, "output_tokens": 900,
                  "cache_read_input_tokens": 12000, "cache_creation_input_tokens": 3000}
    }"#;

    #[test]
    fn parses_full_result() {
        let r = parse_result(FULL).unwrap();
        assert_eq!(r.text, "[]");
        assert_eq!(r.usage.input_tokens, 48211);
        assert_eq!(r.usage.cache_read_tokens, 12000);
        assert_eq!(r.usage.cache_creation_tokens, 3000);
        assert_eq!(r.total_cost_usd, Some(0.12));
        assert_eq!(r.num_turns, Some(7));
        assert_eq!(r.model.as_deref(), Some("claude-opus-4-8"));
        assert!(!r.is_error);
    }

    // Newer Claude Code (2.x): no top-level `model`, a `modelUsage` map instead.
    const MODEL_USAGE: &str = r#"{
        "is_error": false, "result": "[]",
        "usage": {"input_tokens": 2553, "output_tokens": 4},
        "modelUsage": {
            "claude-haiku-4-5-20251001": {
                "inputTokens": 462, "outputTokens": 20,
                "cacheReadInputTokens": 0, "cacheCreationInputTokens": 0,
                "webSearchRequests": 0, "costUSD": 0.000562,
                "contextWindow": 200000, "maxOutputTokens": 32000
            },
            "claude-opus-4-8[1m]": {
                "inputTokens": 2553, "outputTokens": 4,
                "cacheReadInputTokens": 15821, "cacheCreationInputTokens": 2275,
                "webSearchRequests": 0, "costUSD": 0.0349,
                "contextWindow": 1000000, "maxOutputTokens": 64000
            }
        }
    }"#;

    #[test]
    fn parses_model_usage_and_derives_primary_model() {
        let r = parse_result(MODEL_USAGE).unwrap();
        assert_eq!(r.model_usage.len(), 2);
        let opus = &r.model_usage["claude-opus-4-8[1m]"];
        assert_eq!(opus.input_tokens, 2553);
        assert_eq!(opus.cache_read_tokens, 15821);
        assert_eq!(opus.cache_creation_tokens, 2275);
        assert_eq!(opus.cost_usd, 0.0349);
        // Primary model = the entry with the most tokens (opus, via cache reads).
        assert_eq!(r.model.as_deref(), Some("claude-opus-4-8[1m]"));
    }

    #[test]
    fn parses_minimal_result() {
        let r = parse_result(r#"{"result": "[]"}"#).unwrap();
        assert_eq!(r.text, "[]");
        assert_eq!(r.usage, Usage::default());
        assert!(r.total_cost_usd.is_none());
    }

    #[test]
    fn surfaces_is_error() {
        let r =
            parse_result(r#"{"is_error": true, "result": "boom", "usage": {"input_tokens": 5}}"#)
                .unwrap();
        assert!(r.is_error);
        assert_eq!(r.usage.input_tokens, 5);
    }

    #[test]
    fn empty_output_errors() {
        assert!(parse_result("   ").is_err());
    }

    #[test]
    fn non_json_errors() {
        assert!(parse_result("not json at all").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_a_fake_claude_and_captures_usage() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("gilb-claude-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("claude");
        std::fs::write(
            &script,
            "#!/bin/sh\ncat > /dev/null\nprintf '%s' '{\"is_error\":false,\"num_turns\":2,\"result\":\"[]\",\"usage\":{\"input_tokens\":100,\"output_tokens\":10}}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let runner = ClaudeRunner::new()
            .bin(script.to_string_lossy().to_string())
            .skip_permissions(false);
        let r = runner.run("find therbligs in the last hour").await.unwrap();
        assert_eq!(r.text, "[]");
        assert_eq!(r.usage.input_tokens, 100);
        assert_eq!(r.num_turns, Some(2));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
