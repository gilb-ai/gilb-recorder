//! Run the analysis LLM as a `claude -p` subprocess.
//!
//! For the PoC the LLM is Claude Code on the operator's subscription (not the
//! Anthropic API, not a proxy): we spawn `claude -p --output-format json`,
//! optionally attaching `gilb-mcp` via `--mcp-config` so the model can read the
//! live DB read-only. The prompt goes in on stdin; the single `result` JSON
//! object comes back on stdout, carrying both the model's text answer and the
//! token `usage` we record per run.
//!
//! `parse_result` is pure (unit-tested with a canned object). `ClaudeRunner`
//! owns the spawn/timeout/IO and is exercised by an integration test that puts
//! a fake `claude` on disk — no network, no real model.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Default wall-clock cap for one agentic run.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Token usage for one run, normalized from Claude Code's `usage` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
}

/// Parsed outcome of a `claude -p --output-format json` invocation.
#[derive(Debug, Clone)]
pub struct ClaudeResult {
    /// The model's text answer (the `result` field) — fed to `parse_therbligs`.
    pub text: String,
    pub usage: Usage,
    pub total_cost_usd: Option<f64>,
    pub num_turns: Option<i64>,
    pub duration_ms: Option<i64>,
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
    Ok(ClaudeResult {
        text: wire.result.unwrap_or_default(),
        usage: Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_input_tokens,
            cache_creation_tokens: usage.cache_creation_input_tokens,
        },
        total_cost_usd: wire.total_cost_usd,
        num_turns: wire.num_turns,
        duration_ms: wire.duration_ms,
        model: wire.model,
        is_error: wire.is_error,
    })
}

/// Spawns `claude -p` and captures its result. Configurable so an integration
/// test can point `bin` at a fake; in production `bin` defaults to `claude` on
/// `PATH`.
#[derive(Debug, Clone)]
pub struct ClaudeRunner {
    bin: String,
    mcp_config: Option<PathBuf>,
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

    /// Attach an MCP config file (e.g. the one describing `gilb-mcp`).
    pub fn mcp_config(mut self, path: impl Into<PathBuf>) -> Self {
        self.mcp_config = Some(path.into());
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

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Run the prompt and return the parsed result. The prompt is fed on stdin.
    pub async fn run(&self, prompt: &str) -> Result<ClaudeResult> {
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.arg("-p").arg("--output-format").arg("json");
        if self.skip_permissions {
            cmd.arg("--dangerously-skip-permissions");
        }
        if let Some(path) = &self.mcp_config {
            cmd.arg("--mcp-config").arg(path);
        }
        if let Some(model) = &self.model {
            cmd.arg("--model").arg(model);
        }
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
