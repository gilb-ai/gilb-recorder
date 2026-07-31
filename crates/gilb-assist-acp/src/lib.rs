//! Suggestions from a **local agent** over the Agent Client Protocol.
//!
//! One of the two backend shapes the engine supports (`docs/assist.md`): the
//! same engine and the same audio pipeline as a cloud provider would use, but
//! the turns go to an agent already installed on the user's machine — Claude
//! Code through its `claude-code-acp` adapter, `gemini --experimental-acp`, or
//! any other ACP-speaking command.
//!
//! ## Why ACP and not `claude -p`
//!
//! gilb's analyzer already spawns `claude -p` per question. That is fine for a
//! nightly digest and wrong for suggestions: a process per turn pays a cold
//! start every time, and the conversation has to be re-sent because nothing
//! holds it. An ACP session is a live process that keeps its own context, which
//! is exactly the shape [`AssistSession`] was designed around — the engine
//! never learns how a backend remembers (decision D5 in `docs/assist.md`).
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
mod orphans;

pub use orphans::reap as reap_orphaned_agents;

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
    /// `PATH` for the spawned agent, when it must differ from ours.
    ///
    /// An adapter reached through `npx` is a script with a `#!/usr/bin/env
    /// node` shebang: it needs `node` on the *child's* PATH, not ours. A
    /// terminal-launched app inherits a login shell's PATH and never notices;
    /// an `.app` from Finder is handed launchd's, where there is no node, and
    /// the agent dies in five milliseconds with the connection closing on the
    /// first read. `None` inherits ours unchanged.
    pub path_env: Option<String>,
    /// Where to write down the agent process groups this app starts, so a
    /// launch after a crash can clean up what no destructor got to.
    ///
    /// `None` — the default — skips the bookkeeping: the process groups still
    /// go away on an ordinary exit, only a crashed run leaves them behind.
    /// Products that have a data directory should name a file in it.
    pub registry: Option<PathBuf>,
    /// Session config to apply right after the handshake, as `(configId,
    /// value)` — the knobs `session/new` advertises in `configOptions`, e.g.
    /// `("model", "haiku")` or `("effort", "low")` on the Claude Code adapter.
    ///
    /// Best-effort by design: an adapter that has no such option (or no such
    /// method) gets a warning in the log, not a dead feature. The values are
    /// the user's own choice for *this* feature, which is the point — a
    /// suggestion worth having in fifteen seconds and an interactive coding
    /// session do not want the same model.
    pub config_options: Vec<(String, String)>,
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            // The ACP adapter, not the interactive `claude` CLI — that one
            // never answers a JSON-RPC handshake. See KNOWN_AGENTS in the app.
            bin: PathBuf::from("claude-code-acp"),
            args: Vec::new(),
            cwd: std::env::temp_dir(),
            turn_timeout: Duration::from_secs(20),
            startup_timeout: Duration::from_secs(30),
            path_env: None,
            registry: None,
            config_options: Vec::new(),
        }
    }
}

/// One selectable value of a [`SessionOption`].
#[derive(Debug, Clone)]
pub struct SessionChoice {
    pub value: String,
    pub label: String,
}

/// A session knob the agent advertises in `session/new`'s `configOptions` —
/// model, reasoning effort, permission mode. The set is the agent's, not
/// ours, which is what makes a UI built on it honest: nothing to hardcode,
/// nothing to fall out of date.
#[derive(Debug, Clone)]
pub struct SessionOption {
    pub id: String,
    pub name: String,
    /// ACP category (`"model"`, `"thought_level"`, `"mode"`…). More stable
    /// across agents than the id: Claude Code calls its effort knob `effort`,
    /// Codex calls it `reasoning_effort`, both file it under `thought_level`.
    pub category: String,
    /// The agent's own current value — its default when nothing was applied.
    pub current: String,
    pub choices: Vec<SessionChoice>,
}

/// The ids of the session options carried by a `session/new` or
/// `session/set_config_option` reply. Empty when the reply says nothing about
/// them, which is not the same as "it has none" — see the caller.
fn option_ids(reply: &Value) -> std::collections::HashSet<String> {
    reply
        .get("configOptions")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|o| Some(o.get("id")?.as_str()?.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Ask the agent which session knobs it has: one [`bootstrap`], read
/// `configOptions` off `session/new`, drop everything. Costs an agent start
/// (cached-npx fast), so callers should cache the answer per agent.
pub async fn probe_session_options(config: &AcpConfig) -> Result<Vec<SessionOption>> {
    let boot = bootstrap(config).await?;
    let options = boot
        .session
        .get("configOptions")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter(|o| o.get("type").and_then(Value::as_str) == Some("select"))
                .filter_map(|o| {
                    Some(SessionOption {
                        id: o.get("id")?.as_str()?.to_string(),
                        name: o.get("name")?.as_str()?.to_string(),
                        category: o
                            .get("category")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        current: o
                            .get("currentValue")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        choices: o
                            .get("options")?
                            .as_array()?
                            .iter()
                            .filter_map(|c| {
                                Some(SessionChoice {
                                    value: c.get("value")?.as_str()?.to_string(),
                                    label: c
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or(c.get("value")?.as_str()?)
                                        .to_string(),
                                })
                            })
                            .collect(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(options)
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

/// A spawned agent that has answered the handshake: the process (killed on
/// drop), the connection, and `session/new`'s full result.
///
/// The one way to reach an agent, shared by the real session and the options
/// probe — two hand-rolled handshakes had already started to drift (one
/// carried the helpful timeout message, the other did not).
struct Bootstrap {
    conn: Arc<Connection>,
    child: ChildGuard,
    /// `session/new`'s result verbatim; `configOptions` and friends live here.
    session: Value,
    session_id: String,
}

async fn bootstrap(config: &AcpConfig) -> Result<Bootstrap> {
    let mut command = Command::new(&config.bin);
    command
        .args(&config.args)
        .current_dir(&config.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(path) = &config.path_env {
        command.env("PATH", path);
    }
    // Its own process group, so the whole npx → node → agent chain can be
    // signalled at once. Without this only the wrapper dies (see ChildGuard).
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn agent {}", config.bin.display()))?;

    // The child leads the group it was just put in, so its pid is the group's.
    #[cfg(unix)]
    let pgid = child.id().map(|id| id as i32);
    #[cfg(not(unix))]
    let pgid: Option<i32> = None;
    if let (Some(path), Some(pgid)) = (&config.registry, pgid) {
        orphans::register(path, pgid, &config.bin.to_string_lossy());
    }

    // Guarded from here on, not at the end. Everything below can fail — the
    // handshake times out when the binary does not speak ACP, which is the
    // common case on a machine being set up — and each of those returns has to
    // take the whole process group with it. Building the guard last is how the
    // options probe came to leave an agent running every time it failed.
    let stdin = child.stdin.take().context("agent stdin")?;
    let stdout = child.stdout.take().context("agent stdout")?;
    let child = ChildGuard {
        child,
        pgid,
        registry: config.registry.clone(),
    };
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
        conn.request(
            "session/new",
            json!({ "cwd": config.cwd.to_string_lossy(), "mcpServers": [] }),
        )
        .await
    };

    // Name the binary. The overwhelmingly likely cause is that it does not
    // speak ACP at all — an interactive coding CLI started instead of its
    // ACP adapter reads every byte we send and answers nothing, which is
    // indistinguishable from "slow" until you know which command ran.
    let session = tokio::time::timeout(config.startup_timeout, handshake)
        .await
        .map_err(|_| {
            anyhow!(
                "`{}` did not answer the ACP handshake within {:?} — does it speak ACP? \
                 (an interactive CLI never will; Claude Code needs the claude-code-acp adapter)",
                config.bin.display(),
                config.startup_timeout
            )
        })??;
    let session_id = session
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("session/new returned no sessionId"))?;

    Ok(Bootstrap {
        conn,
        child,
        session,
        session_id,
    })
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
        let boot = bootstrap(&config).await?;

        // Apply the session knobs the caller asked for. After the handshake,
        // before the first prompt — the first suggestion should already run on
        // the configured model. Failures are logged and skipped: the option
        // set differs per adapter, and a knob that does not exist must not
        // cost the feature.
        // What the agent will accept, which is not fixed for the session: the
        // knobs depend on each other. Choosing Claude's `haiku` withdraws
        // `effort` in the same breath, because that model has no thinking
        // tiers — so a client that applies a remembered model and a remembered
        // effort in order asks for something the agent stopped offering one
        // message ago, and gets "Unknown config option: effort" for its
        // trouble. Each reply carries the option set as it now stands; follow
        // it.
        let mut offered = option_ids(&boot.session);
        for (config_id, value) in &config.config_options {
            // Empty means the agent said nothing about its options at
            // `session/new` — no grounds to second-guess it, so try anyway.
            if !offered.is_empty() && !offered.contains(config_id) {
                debug!(
                    config_id,
                    value, "session option withdrawn by an earlier choice — skipped"
                );
                continue;
            }
            let result = boot
                .conn
                .request(
                    "session/set_config_option",
                    json!({ "sessionId": boot.session_id, "configId": config_id, "value": value }),
                )
                .await;
            match result {
                Ok(reply) => {
                    debug!(config_id, value, "session option set");
                    let next = option_ids(&reply);
                    if !next.is_empty() {
                        offered = next;
                    }
                }
                Err(err) => {
                    warn!(error = %err, config_id, value, "session option not applied")
                }
            }
        }

        Ok(Self {
            conn: boot.conn,
            session_id: boot.session_id,
            pending_system_prompt: Some(system_prompt).filter(|p| !p.trim().is_empty()),
            turn_timeout: config.turn_timeout,
            _child: boot.child,
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

        let outcome = tokio::time::timeout(
            self.turn_timeout,
            self.conn.request("session/prompt", params),
        )
        .await;
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
/// A spawned agent, and the whole wrapper chain behind it.
///
/// What we spawn is `npx`, which execs `node`, which starts the agent — so
/// killing the process we hold leaves the agent running, reparented to init,
/// holding its memory until the machine is rebooted. The child is put in its
/// own process group at spawn so that dropping it signals all three.
struct ChildGuard {
    child: Child,
    /// The child's process group (its own pid — it leads the group). `None`
    /// where process groups do not apply.
    pgid: Option<i32>,
    /// Where the group was written down, to strike it out again.
    registry: Option<PathBuf>,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        debug!("stopping ACP agent");
        match self.pgid {
            Some(pgid) => orphans::kill_group(pgid),
            // No group to signal: at least take the process we hold.
            None => {
                let _ = self.child.start_kill();
            }
        }
        if let (Some(path), Some(pgid)) = (&self.registry, self.pgid) {
            orphans::unregister(path, pgid);
        }
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
            let method = msg
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
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
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests;
