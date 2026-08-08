//! Suggestions from a **local agent** over the Agent Client Protocol.
//!
//! One of the two backend shapes the engine supports (`docs/assist.md`): the
//! same engine and the same audio pipeline as a cloud provider would use, but
//! the turns go to an agent already installed on the user's machine.
//!
//! ## What is here, and what moved
//!
//! Speaking ACP — the JSON-RPC framing, the reply routing, the process groups
//! a package runner leaves behind — is no longer gilb's problem: it lives in
//! [`acp_client`], shared with the other products that reach a local agent the
//! same way, together with the tests that pin every one of those lessons.
//!
//! What stays here is the half that is a *meeting assistant*, and it is the
//! half that would be wrong in a shared crate:
//!
//! * **A deadline of seconds, not minutes.** A suggestion that arrives after
//!   the topic moved on is worse than silence, so a late turn yields `None`
//!   rather than an error the panel would show as a red line.
//! * **No permission dialogs.** A coding agent may ask to run a tool mid-turn
//!   and nobody is watching the panel to answer, so every request is refused
//!   ([`PermissionPolicy::RefuseAll`]) rather than left to hang the turn.
//! * **Text only.** Thoughts and tool calls are dropped; only the answer
//!   becomes a suggestion.
//! * **A system prompt with nowhere to go.** ACP has no system-message slot:
//!   an agent arrives with its own instructions, and ours ride in as the
//!   opening turn — once, not on every suggestion.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use acp_client::{
    Agent, Config, Deadlines, Error as AcpError, Event, EventKind, PermissionPolicy, Session,
    SessionOpts,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use gilb_assist::{AssistBackend, AssistSession};
use tokio::sync::mpsc;
use tracing::warn;

/// Whether the machine has this agent. Re-exported so the product asks one
/// question of one place — the panel gates the whole feature on it.
pub use acp_agents::available as agent_available;
/// Kill agents a previous run left behind. Call once at startup, before
/// anything spawns an agent of its own.
pub use acp_client::reap as reap_orphaned_agents;
pub use acp_client::{SessionChoice, SessionOption};

/// How many events may queue up before the reader waits for us. Only ever
/// touched while a turn is in flight, and a turn is drained as it streams.
const EVENT_BUFFER: usize = 128;

#[derive(Debug, Clone)]
pub struct AcpConfig {
    /// Agent executable. Resolution — PATH, npm dirs, an override env var — is
    /// the product's job (`assist::harness`); this only spawns what it is
    /// handed.
    pub bin: PathBuf,
    /// Arguments that put the agent into ACP mode.
    pub args: Vec<String>,
    /// Working directory for the session. An agent scopes file access to it,
    /// so a meeting assistant gets something harmless.
    pub cwd: PathBuf,
    /// How long one suggestion may take before it is abandoned.
    pub turn_timeout: Duration,
    /// How long the agent has to answer the handshake. Separate from the turn
    /// deadline: a cold agent may be slow once — `npx` may be fetching it —
    /// and failing the handshake disables the feature rather than losing one
    /// suggestion.
    pub startup_timeout: Duration,
    /// `PATH` for the spawned agent, when it must differ from ours.
    pub path_env: Option<String>,
    /// Where to write down the agent process groups this app starts.
    pub registry: Option<PathBuf>,
    /// Session knobs to apply right after the handshake, as `(configId,
    /// value)` — the user's own choice for *this* feature, which is the point:
    /// a suggestion worth having in fifteen seconds and an interactive coding
    /// session do not want the same model.
    pub config_options: Vec<(String, String)>,
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            // The ACP adapter, not the interactive `claude` CLI — that one
            // never answers a JSON-RPC handshake. See HARNESSES in the app.
            bin: PathBuf::from("claude-agent-acp"),
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

impl AcpConfig {
    fn client(&self) -> Config {
        let mut config = Config::new(self.bin.clone())
            .args(self.args.clone())
            .cwd(self.cwd.clone())
            .client("gilb-assist", env!("CARGO_PKG_VERSION"))
            // Nobody is watching the overlay to grant anything.
            .permissions(PermissionPolicy::RefuseAll)
            .deadlines(Deadlines {
                startup: self.startup_timeout,
                ..Deadlines::interactive(self.turn_timeout)
            });
        if let Some(path) = &self.path_env {
            config = config.env("PATH", path.clone());
        }
        if let Some(registry) = &self.registry {
            config = config.registry(registry.clone());
        }
        config
    }

    fn session(&self) -> SessionOpts {
        self.config_options.iter().fold(
            SessionOpts::default().cwd(self.cwd.clone()),
            |opts, (id, value)| opts.with_config(id, value),
        )
    }
}

/// Ask the agent which session knobs it has: start one, read the options off
/// `session/new`, drop everything.
///
/// Costs an agent start, so callers should cache the answer per agent — and it
/// doubles as the honest install check, because a package that downloaded but
/// cannot answer `initialize` is not installed in any sense the user cares
/// about.
///
/// Only `select` options come back: a control for a type nobody has produced
/// is a guess, and the list is meant to be read rather than assumed.
pub async fn probe_session_options(config: &AcpConfig) -> Result<Vec<SessionOption>> {
    let (tx, _events) = mpsc::channel(EVENT_BUFFER);
    let agent = Agent::launch(config.client(), tx).await?;
    let session = agent
        .new_session(SessionOpts::default().cwd(config.cwd.clone()))
        .await?;
    Ok(session
        .options()
        .iter()
        .filter(|o| o.kind == "select" && !o.choices.is_empty())
        .cloned()
        .collect())
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
        let (tx, events) = mpsc::channel(EVENT_BUFFER);
        let agent = Agent::launch(self.config.client(), tx)
            .await
            .with_context(|| format!("starting {}", self.config.bin.display()))?;
        let session = agent.new_session(self.config.session()).await?;

        Ok(Box::new(AcpSession {
            _agent: agent,
            session,
            events,
            pending_system_prompt: Some(system_prompt.to_string()).filter(|p| !p.trim().is_empty()),
        }))
    }
}

/// One live agent process, one ACP session, for the length of a meeting.
pub struct AcpSession {
    /// Held for its lifetime: dropping it stops the agent and everything it
    /// started.
    _agent: Arc<Agent>,
    session: Session,
    events: mpsc::Receiver<Event>,
    pending_system_prompt: Option<String>,
}

impl AcpSession {
    /// One turn, and the answer it streamed.
    ///
    /// The events are drained *while* the turn runs rather than after it: a
    /// long answer would otherwise fill the channel and stall the reader that
    /// is also carrying the reply we are waiting for.
    async fn turn(&mut self, text: String) -> Result<Option<String>> {
        let mut answer = String::new();
        let outcome = {
            let prompt = self.session.prompt(&text);
            tokio::pin!(prompt);
            loop {
                tokio::select! {
                    outcome = &mut prompt => break outcome,
                    Some(event) = self.events.recv() => Self::collect(&mut answer, event),
                }
            }
        };
        // Whatever arrived between the last chunk and the reply.
        while let Ok(event) = self.events.try_recv() {
            Self::collect(&mut answer, event);
        }

        match outcome {
            Ok(_) => Ok(Some(answer).filter(|a| !a.trim().is_empty())),
            // Silence beats a stale suggestion, and the engine keeps the turns
            // buffered for the next attempt either way. The client has already
            // cancelled the turn and quarantined whatever the agent is still
            // saying about it.
            Err(err @ AcpError::Timeout { .. }) => {
                warn!(%err, "assist: staying silent");
                Ok(None)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Only the answer reaches the panel. Thoughts and tool calls are the
    /// agent working, not something to say out loud in a meeting.
    fn collect(answer: &mut String, event: Event) {
        if let EventKind::Text(text) = event.kind {
            answer.push_str(&text);
        }
    }
}

#[async_trait]
impl AssistSession for AcpSession {
    async fn send(&mut self, input: &str) -> Result<Option<String>> {
        let text = match &self.pending_system_prompt {
            Some(prompt) => format!("{prompt}\n\n{input}"),
            None => input.to_string(),
        };
        let result = self.turn(text).await;
        // Cleared only once the turn was actually made. The engine retries a
        // failed `send` with the same input, and a system prompt dropped on
        // the way would leave the agent prompting a meeting with no
        // instructions at all — for the rest of the meeting.
        if result.is_ok() {
            self.pending_system_prompt = None;
        }
        result
    }
}

#[cfg(test)]
mod tests;
