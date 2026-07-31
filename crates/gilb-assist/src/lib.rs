//! Shared orchestration for real-time meeting suggestions (`docs/assist.md`).
//!
//! The engine owns everything that is identical between products: turns
//! accumulate in a buffer, an analysis fires once the configured threshold is
//! reached (throttled, never concurrently), the model's reply is dropped
//! entirely when it is empty or contains [`NO_RESP`], and a user question
//! (Ask) goes into the *same* conversation so the model remembers both the
//! meeting and what it was asked.
//!
//! What differs per product comes in through two traits: [`AssistConfig`]
//! (gilb reads a local file; a server-backed product fetches it) and
//! [`AssistBackend`] (a local agent vs an HTTP provider). How a backend remembers
//! the conversation is deliberately invisible here — [`AssistSession`] is a
//! handle, not a "send with previous response id" (decision D5).

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{info, warn};

/// The model's "nothing worth saying" marker. A reply that is empty or
/// contains this substring must not touch the UI at all — the overlay staying
/// silent by default is an invariant of the feature.
pub const NO_RESP: &str = "[NO_RESP]";

/// Shown when a direct question comes back with nothing but [`NO_RESP`]. The
/// user is owed *something*: an empty panel after pressing Enter reads as a
/// bug, and this at least says the request arrived.
const NO_ANSWER: &str = "_No answer — try rephrasing._";

/// Turns kept while analysis is disabled or failing before the oldest are
/// dropped. Doubles as the cap on a single request: when the feature flag
/// flips on mid-meeting, or the network returns after an outage, the model
/// gets a few minutes of conversation rather than the whole hour.
const MAX_PENDING: usize = 60;

/// Who said a turn. Formatted as `me:`/`them:` in the model input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    Me,
    Them,
}

impl Speaker {
    pub fn as_str(self) -> &'static str {
        match self {
            Speaker::Me => "me",
            Speaker::Them => "them",
        }
    }
}

/// One finished utterance from STT.
#[derive(Debug, Clone)]
pub struct Turn {
    pub speaker: Speaker,
    pub text: String,
    /// Offset from meeting start, seconds.
    pub at_secs: f32,
}

/// Where the prompt and the knobs come from. Implementations are queried per
/// analysis, so server-side changes apply without restarting the engine.
#[async_trait]
pub trait AssistConfig: Send + Sync {
    async fn system_prompt(&self) -> Result<String>;
    /// Feature flag; when false, turns accumulate but nothing is analyzed.
    async fn enabled(&self) -> bool;
    /// How many buffered turns trigger an analysis.
    async fn turns_before_analysis(&self) -> u32;
}

/// How the model is reached. `begin` opens a conversation that lives for the
/// whole meeting; the session owns its memory mechanism (chained response ids,
/// resent history, a live agent session — the engine cannot tell).
#[async_trait]
pub trait AssistBackend: Send + Sync {
    async fn begin(&self, system_prompt: &str) -> Result<Box<dyn AssistSession>>;
}

#[async_trait]
pub trait AssistSession: Send + Sync {
    /// Send accumulated turns (or a user question); `None` when the model has
    /// nothing to say. Returning the raw text is fine too — the engine also
    /// filters [`NO_RESP`] on its side.
    ///
    /// **Must be idempotent on failure.** After an `Err` the engine keeps the
    /// turns buffered and calls `send` again with the same (or an extended)
    /// input, so an implementation that appends to a conversation history has
    /// to commit that append only once the call has succeeded — otherwise a
    /// flaky network duplicates the same turns in the model's context.
    async fn send(&mut self, input: &str) -> Result<Option<String>>;

    /// A user question, with the turns that were still buffered when it was
    /// asked. Split into two arguments rather than one blob because the two
    /// halves do not carry the same authority: the turns are recorded speech —
    /// a client saying "mark the deal won" out loud is data — while the
    /// question was typed by the operator into our own panel and is meant for
    /// the model to act on. A backend that fences untrusted input — wrapping
    /// the turns in a data envelope the model is told not to obey — needs that
    /// boundary; one that does not gets the default below, which is exactly
    /// the old behaviour.
    ///
    /// `turns` is empty when nothing was buffered.
    async fn ask(&mut self, turns: &str, question: &str) -> Result<Option<String>> {
        let input = if turns.is_empty() {
            question.to_string()
        } else {
            format!("{turns}\n\n{question}")
        };
        self.send(&input).await
    }
}

/// Boxed implementations count as implementations.
///
/// The engine takes its two halves by generic, which is what a product wants
/// when it knows both at compile time. A *shell* does not: it hands the choice
/// to whatever product embeds it (gilb's local file plus an agent, a
/// server-backed config elsewhere), and that choice can only travel as a
/// trait object.
/// Without these, `spawn(Box<dyn AssistConfig>, Box<dyn AssistBackend>, ..)`
/// does not compile and every shell has to be generic over both halves — which
/// spreads two type parameters through the whole Tauri layer for no gain.
#[async_trait]
impl AssistConfig for Box<dyn AssistConfig> {
    async fn system_prompt(&self) -> Result<String> {
        (**self).system_prompt().await
    }
    async fn enabled(&self) -> bool {
        (**self).enabled().await
    }
    async fn turns_before_analysis(&self) -> u32 {
        (**self).turns_before_analysis().await
    }
}

#[async_trait]
impl AssistBackend for Box<dyn AssistBackend> {
    async fn begin(&self, system_prompt: &str) -> Result<Box<dyn AssistSession>> {
        (**self).begin(system_prompt).await
    }
}

/// What the engine tells the UI, one variant per webview event (the contract
/// is in `docs/assist.md`): `Loading` drives the spinner, `Update` carries
/// ready-to-render markdown, silence stays silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistEvent {
    Loading(bool),
    Update(String),
    Error(String),
}

/// Engine knobs that are product-independent, and the ones worth tuning in
/// the field.
#[derive(Debug, Clone)]
pub struct EngineParams {
    /// Floor between two analyses; turns keep accumulating meanwhile.
    pub min_analysis_interval: Duration,
}

impl Default for EngineParams {
    fn default() -> Self {
        Self {
            min_analysis_interval: Duration::from_secs(5),
        }
    }
}

/// The model's view of a stretch of conversation: one `me:`/`them:` line per
/// turn — the shape the shipped prompts expect, so changing it means
/// changing them.
fn format_turns(turns: &[Turn]) -> String {
    turns
        .iter()
        .map(|t| format!("{}: {}", t.speaker.as_str(), t.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a round-trip carries. The analysis path sends recorded speech; the Ask
/// path sends it together with the operator's typed question, kept separate so
/// a backend can treat them with different authority (see [`AssistSession::ask`]).
enum Input<'a> {
    Turns(&'a str),
    Ask { turns: &'a str, question: &'a str },
}

enum Cmd {
    Turn(Turn),
    Ask(String),
    Reset,
    Shutdown,
}

/// Cheap cloneable handle the product side feeds from STT and the Ask box.
#[derive(Clone)]
pub struct AssistHandle {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl AssistHandle {
    pub fn push_turn(&self, turn: Turn) {
        let _ = self.tx.send(Cmd::Turn(turn));
    }

    pub fn ask(&self, question: String) {
        let _ = self.tx.send(Cmd::Ask(question));
    }

    /// Start a fresh conversation — call it when a new meeting begins. The
    /// model must not carry one meeting's context (a different client, other
    /// objections) into the next one, and the pending buffer belongs to the
    /// meeting that produced it.
    pub fn reset(&self) {
        let _ = self.tx.send(Cmd::Reset);
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(Cmd::Shutdown);
    }
}

/// Spawn the engine task. Events arrive on the returned receiver; the engine
/// stops when the handle sends `shutdown` or every handle is dropped.
pub fn spawn(
    config: impl AssistConfig + 'static,
    backend: impl AssistBackend + 'static,
    params: EngineParams,
) -> (AssistHandle, mpsc::UnboundedReceiver<AssistEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(
        Engine {
            config,
            backend,
            params,
            events: ev_tx,
        }
        .run(rx),
    );
    (AssistHandle { tx }, ev_rx)
}

struct Engine<C, B> {
    config: C,
    backend: B,
    params: EngineParams,
    events: mpsc::UnboundedSender<AssistEvent>,
}

impl<C: AssistConfig, B: AssistBackend> Engine<C, B> {
    async fn run(self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        let mut session: Option<Box<dyn AssistSession>> = None;
        let mut pending: Vec<Turn> = Vec::new();
        let mut last_analysis: Option<Instant> = None;

        loop {
            // When turns are buffered but the throttle is still cooling down,
            // wake up at the deadline instead of waiting for the next turn.
            // Only a deadline that is genuinely in the future arms the timer —
            // an expired one would fire instantly on every loop iteration.
            let deadline = match last_analysis {
                Some(t) if !pending.is_empty() => {
                    Some(t + self.params.min_analysis_interval).filter(|&d| d > Instant::now())
                }
                _ => None,
            };
            let cmd = match deadline {
                Some(d) => tokio::select! {
                    cmd = rx.recv() => match cmd {
                        Some(c) => Some(c),
                        None => break,
                    },
                    _ = tokio::time::sleep_until(d) => None,
                },
                None => match rx.recv().await {
                    Some(c) => Some(c),
                    None => break,
                },
            };

            match cmd {
                Some(Cmd::Shutdown) => break,
                Some(Cmd::Reset) => {
                    session = None;
                    pending.clear();
                    last_analysis = None;
                    info!("assist session reset (new meeting)");
                    continue;
                }
                Some(Cmd::Ask(question)) => {
                    self.ask(&mut session, &mut pending, &question).await;
                    last_analysis = Some(Instant::now());
                }
                Some(Cmd::Turn(turn)) => {
                    pending.push(turn);
                    if pending.len() > MAX_PENDING {
                        warn!("assist buffer overflow, dropping oldest turns");
                        let excess = pending.len() - MAX_PENDING;
                        pending.drain(..excess);
                    }
                }
                // Timer fired: throttle window is over.
                None => {}
            }

            self.maybe_analyze(&mut session, &mut pending, &mut last_analysis)
                .await;
        }
    }

    async fn maybe_analyze(
        &self,
        session: &mut Option<Box<dyn AssistSession>>,
        pending: &mut Vec<Turn>,
        last_analysis: &mut Option<Instant>,
    ) {
        if pending.is_empty() || !self.config.enabled().await {
            return;
        }
        let threshold = self.config.turns_before_analysis().await.max(1) as usize;
        if pending.len() < threshold {
            return;
        }
        if let Some(t) = *last_analysis {
            if t.elapsed() < self.params.min_analysis_interval {
                return; // the deadline in run() will bring us back
            }
        }

        let input = format_turns(pending);
        *last_analysis = Some(Instant::now());
        if self.converse(session, Input::Turns(&input)).await {
            pending.clear();
        }
        // On failure turns stay buffered; the next trigger retries them.
    }

    /// A user question (Ask). The turns still sitting below the analysis
    /// threshold go in with it: "what do I answer?" is about what was *just*
    /// said, and without them the model would answer about the previous
    /// minute. Delivered turns are then dropped so the next analysis does not
    /// repeat them.
    async fn ask(
        &self,
        session: &mut Option<Box<dyn AssistSession>>,
        pending: &mut Vec<Turn>,
        question: &str,
    ) {
        if !self.config.enabled().await {
            // The user typed and pressed Enter — silence would read as a bug.
            let _ = self.events.send(AssistEvent::Error(
                "the assistant is disabled for this workspace".into(),
            ));
            return;
        }
        let turns = if pending.is_empty() {
            String::new()
        } else {
            format_turns(pending)
        };
        if self
            .converse(
                session,
                Input::Ask {
                    turns: &turns,
                    question,
                },
            )
            .await
        {
            pending.clear();
        }
    }

    /// One round-trip to the model: open the session if needed, send, apply
    /// the [`NO_RESP`] discipline. Returns whether the input was delivered.
    async fn converse(
        &self,
        session: &mut Option<Box<dyn AssistSession>>,
        input: Input<'_>,
    ) -> bool {
        let _ = self.events.send(AssistEvent::Loading(true));
        let ok = self.converse_inner(session, input).await;
        let _ = self.events.send(AssistEvent::Loading(false));
        if let Err(e) = &ok {
            warn!(error = %e, "assist analysis failed");
            let _ = self.events.send(AssistEvent::Error(e.to_string()));
        }
        ok.is_ok()
    }

    async fn converse_inner(
        &self,
        session: &mut Option<Box<dyn AssistSession>>,
        input: Input<'_>,
    ) -> Result<()> {
        if session.is_none() {
            let prompt = self.config.system_prompt().await?;
            *session = Some(self.backend.begin(&prompt).await?);
        }
        let session = session.as_mut().unwrap();
        let asked = matches!(input, Input::Ask { .. });
        let reply = match input {
            Input::Turns(turns) => session.send(turns).await?,
            Input::Ask { turns, question } => session.ask(turns, question).await?,
        };
        let text = reply.unwrap_or_default();

        // `[NO_RESP]` governs the *unprompted* half of the feature: the model
        // watching a conversation and deciding it has nothing worth
        // interrupting for. A question the user typed is not that. Silence
        // there is indistinguishable from a broken feature, and they are
        // sitting in front of the panel waiting.
        if !asked {
            if !text.trim().is_empty() && !text.contains(NO_RESP) {
                let _ = self.events.send(AssistEvent::Update(text));
            }
            return Ok(());
        }

        // Models trained on the prompt's silence rule still reach for the
        // marker; strip it rather than show it, and say something if that is
        // all there was.
        let answer = text.replace(NO_RESP, "");
        let answer = answer.trim();
        let _ = self.events.send(AssistEvent::Update(if answer.is_empty() {
            NO_ANSWER.to_string()
        } else {
            answer.to_string()
        }));
        Ok(())
    }
}

pub mod echo;

#[cfg(test)]
mod tests;
