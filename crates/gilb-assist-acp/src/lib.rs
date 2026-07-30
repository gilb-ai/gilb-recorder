//! Suggestions from a **local agent** over the Agent Client Protocol.
//!
//! This is gilb's half of the assist stack (stage 2 in Rodnik's
//! REALTIME_ASSIST.md §12): the same engine and the same audio pipeline, but
//! instead of a cloud provider the turns go to an agent already running on the
//! user's machine — `claude`, `gemini --experimental-acp`, a Hermes adapter.
//!
//! ## Why ACP and not `claude -p`
//!
//! gilb's analyzer already spawns `claude -p` per question. That is fine for a
//! nightly digest and wrong for suggestions: a process per turn pays a cold
//! start every time, and the conversation has to be re-sent because nothing
//! holds it. An ACP session is a live process that keeps its own context, which
//! is exactly the shape [`AssistSession`] was designed around — the engine
//! never learns how a backend remembers (decision D5).
//!
//! ## The three things a live meeting demands
//!
//! * **A deadline.** A suggestion that arrives after the topic moved on is
//!   worse than silence, so every prompt is capped ([`AcpConfig::turn_timeout`])
//!   and a late agent yields `None`, not an error.
//! * **No permission dialogs.** A coding agent may ask to run a tool mid-turn.
//!   Nobody is watching the panel to answer, so requests are auto-refused
//!   rather than left to hang the turn (`session/request_permission`).
//! * **Text only.** Everything the agent streams as thoughts or tool calls is
//!   dropped; only `agent_message_chunk` text becomes a suggestion.
//!
//! ## Wire shape
//!
//! JSON-RPC 2.0, one message per line, over the agent's stdin/stdout:
//!
//! ```text
//! → {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,…}}
//! → {"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"…","mcpServers":[]}}
//! → {"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"…","prompt":[…]}}
//! ← {"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk",…}}}
//! ← {"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use gilb_assist::{AssistBackend, AssistSession};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};

/// ACP revision this client speaks.
const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct AcpConfig {
    /// Agent executable. Resolution (PATH, npm dirs, an override env var) is
    /// the product's job — this crate only spawns what it is handed.
    pub bin: PathBuf,
    /// Arguments that put the agent into ACP mode, e.g. `["--experimental-acp"]`.
    pub args: Vec<String>,
    /// Working directory for the session. An agent scopes file access to it, so
    /// a meeting assistant should get something harmless.
    pub cwd: PathBuf,
    /// How long one suggestion may take before it is abandoned. Past this the
    /// conversation has moved on.
    pub turn_timeout: Duration,
    /// How long the agent has to answer `initialize`/`session/new`. Separate
    /// from the turn deadline: a cold agent may be slow once, and failing the
    /// handshake disables the feature rather than losing one suggestion.
    pub startup_timeout: Duration,
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            bin: PathBuf::from("claude"),
            args: Vec::new(),
            cwd: std::env::temp_dir(),
            turn_timeout: Duration::from_secs(20),
            startup_timeout: Duration::from_secs(30),
        }
    }
}

pub struct AcpBackend {
    config: AcpConfig,
}

impl AcpBackend {
    pub fn new(config: AcpConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl AssistBackend for AcpBackend {
    async fn begin(&self, system_prompt: &str) -> Result<Box<dyn AssistSession>> {
        let session = AcpSession::start(self.config.clone(), system_prompt.to_string()).await?;
        Ok(Box::new(session))
    }
}

/// One live agent process, one ACP session.
pub struct AcpSession {
    conn: Arc<Connection>,
    session_id: String,
    /// Prepended to the first prompt. ACP has no system-message slot: an agent
    /// arrives with its own instructions, and ours ride in as the opening turn.
    pending_system_prompt: Option<String>,
    turn_timeout: Duration,
    _child: ChildGuard,
}

impl AcpSession {
    async fn start(config: AcpConfig, system_prompt: String) -> Result<Self> {
        let mut child = Command::new(&config.bin)
            .args(&config.args)
            .current_dir(&config.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn agent {}", config.bin.display()))?;

        let stdin = child.stdin.take().context("agent stdin")?;
        let stdout = child.stdout.take().context("agent stdout")?;
        let conn = Connection::spawn(stdin, stdout);

        let handshake = async {
            conn.request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false } },
                    "clientInfo": { "name": "gilb-assist", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await?;

            let session = conn
                .request(
                    "session/new",
                    json!({ "cwd": config.cwd.to_string_lossy(), "mcpServers": [] }),
                )
                .await?;
            session
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow!("session/new returned no sessionId"))
        };

        let session_id = tokio::time::timeout(config.startup_timeout, handshake)
            .await
            .map_err(|_| anyhow!("agent did not complete the ACP handshake in time"))??;

        Ok(Self {
            conn,
            session_id,
            pending_system_prompt: Some(system_prompt).filter(|p| !p.trim().is_empty()),
            turn_timeout: config.turn_timeout,
            _child: ChildGuard(child),
        })
    }

    async fn prompt(&mut self, text: String) -> Result<Option<String>> {
        let params = json!({
            "sessionId": self.session_id,
            "prompt": [{ "type": "text", "text": text }],
        });

        // Collect what the agent streams for THIS turn. The connection hands
        // chunks over as they arrive; the request future finishes when the
        // agent declares the turn over.
        let (chunks_tx, mut chunks_rx) = mpsc::unbounded_channel();
        self.conn.set_chunk_sink(Some(chunks_tx)).await;

        let outcome = tokio::time::timeout(self.turn_timeout, self.conn.request("session/prompt", params)).await;
        self.conn.set_chunk_sink(None).await;

        let mut text = String::new();
        while let Ok(chunk) = chunks_rx.try_recv() {
            text.push_str(&chunk);
        }

        match outcome {
            Ok(Ok(_)) => Ok(Some(text).filter(|t| !t.trim().is_empty())),
            Ok(Err(err)) => Err(err),
            // Silence beats a stale suggestion, and the engine keeps the turns
            // buffered for the next attempt either way.
            Err(_) => {
                warn!(timeout = ?self.turn_timeout, "agent turn timed out; staying silent");
                let _ = self
                    .conn
                    .notify("session/cancel", json!({ "sessionId": self.session_id }))
                    .await;
                Ok(None)
            }
        }
    }
}

#[async_trait]
impl AssistSession for AcpSession {
    async fn send(&mut self, input: &str) -> Result<Option<String>> {
        let text = match self.pending_system_prompt.take() {
            Some(prompt) => format!("{prompt}\n\n{input}"),
            None => input.to_string(),
        };
        self.prompt(text).await
    }
}

/// Kills the agent when the session is dropped. `kill_on_drop` covers a normal
/// drop; this makes the intent explicit and gives the process a name in logs.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        debug!("stopping ACP agent");
        let _ = self.0.start_kill();
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC over the agent's stdio
// ---------------------------------------------------------------------------

struct Connection {
    stdin: Mutex<ChildStdin>,
    next_id: std::sync::atomic::AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>,
    /// Where streamed text goes while a turn is in flight.
    chunks: Mutex<Option<mpsc::UnboundedSender<String>>>,
}

impl Connection {
    fn spawn(stdin: ChildStdin, stdout: tokio::process::ChildStdout) -> Arc<Self> {
        let conn = Arc::new(Self {
            stdin: Mutex::new(stdin),
            next_id: std::sync::atomic::AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            chunks: Mutex::new(None),
        });

        let reader_conn = conn.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(msg) => reader_conn.dispatch(msg).await,
                    Err(err) => warn!(error = %err, "unparsable ACP line"),
                }
            }
            // The agent died or closed stdout: fail everything still waiting so
            // no turn hangs on a process that is never going to answer.
            let mut pending = reader_conn.pending.lock().await;
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err(anyhow!("agent closed the connection")));
            }
        });

        conn
    }

    async fn dispatch(&self, msg: Value) {
        // A response to something we asked.
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            if msg.get("method").is_none() {
                let result = match msg.get("error") {
                    Some(err) => Err(anyhow!("agent error: {err}")),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                if let Some(tx) = self.pending.lock().await.remove(&id) {
                    let _ = tx.send(result);
                }
                return;
            }
            // A request FROM the agent. The only one that matters here is a
            // permission prompt: nobody is watching the overlay to grant it, so
            // refuse and let the agent finish the turn with what it has.
            let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
            let outcome = if method == "session/request_permission" {
                json!({ "outcome": { "outcome": "cancelled" } })
            } else {
                json!({})
            };
            let _ = self.respond(id, outcome).await;
            return;
        }

        // A notification: the streamed answer.
        if msg.get("method").and_then(Value::as_str) == Some("session/update") {
            let update = msg.pointer("/params/update");
            let kind = update
                .and_then(|u| u.get("sessionUpdate"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            // Thoughts and tool calls are noise for a suggestion panel.
            if kind != "agent_message_chunk" {
                return;
            }
            let text = update
                .and_then(|u| u.pointer("/content/text"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if text.is_empty() {
                return;
            }
            if let Some(sink) = self.chunks.lock().await.as_ref() {
                let _ = sink.send(text.to_string());
            }
        }
    }

    async fn set_chunk_sink(&self, sink: Option<mpsc::UnboundedSender<String>>) {
        *self.chunks.lock().await = sink;
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.write(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        rx.await.map_err(|_| anyhow!("agent dropped the request"))?
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    async fn respond(&self, id: u64, result: Value) -> Result<()> {
        self.write(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
            .await
    }

    async fn write(&self, msg: Value) -> Result<()> {
        let mut line = serde_json::to_string(&msg).context("encode ACP message")?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .context("write to agent")?;
        stdin.flush().await.context("flush agent stdin")?;
        Ok(())
    }
}

/// Fail loudly rather than silently returning nothing when the agent binary is
/// missing — the product gates the feature on this.
pub fn agent_available(bin: &std::path::Path) -> bool {
    if bin.is_absolute() {
        return bin.is_file();
    }
    std::env::var("PATH")
        .map(|path| {
            std::env::split_paths(&path).any(|dir| dir.join(bin).is_file())
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests;
