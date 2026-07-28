//! Shared orchestration for real-time meeting suggestions ([RDK]
//! `rodnik-app-tauri/docs/REALTIME_ASSIST.md` §7).
//!
//! The engine owns everything that is identical between products: turns
//! accumulate in a buffer, an analysis fires once the configured threshold is
//! reached (throttled, never concurrently), the model's reply is dropped
//! entirely when it is empty or contains [`NO_RESP`], and a user question
//! (Ask) goes into the *same* conversation so the model remembers both the
//! meeting and what it was asked.
//!
//! What differs per product comes in through two traits: [`AssistConfig`]
//! (Rodnik pulls it from `/api/v1/config`, gilb from a local file) and
//! [`AssistBackend`] (HTTP provider vs local agent). How a backend remembers
//! the conversation is deliberately invisible here — [`AssistSession`] is a
//! handle, not a "send with previous response id" (decision D5).

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::warn;

/// The model's "nothing worth saying" marker. A reply that is empty or
/// contains this substring must not touch the UI at all — the overlay staying
/// silent by default is an invariant of the feature.
pub const NO_RESP: &str = "[NO_RESP]";

/// Turns kept while analysis is disabled or failing before the oldest are
/// dropped; keeps an unattended meeting from growing the buffer forever.
const MAX_PENDING: usize = 200;

/// Who said a turn. Formatted as `me:`/`them:` in the model input, the same
/// shape the Electron prototype used.
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
    async fn send(&mut self, input: &str) -> Result<Option<String>>;
}

/// What the engine tells the UI. Mirrors the webview events in §4.4:
/// `Loading` drives the spinner, `Update` carries ready-to-render markdown,
/// silence stays silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistEvent {
    Loading(bool),
    Update(String),
    Error(String),
}

/// Engine knobs that are product-independent (§6.8 "параметры для подбора").
#[derive(Debug, Clone)]
pub struct EngineParams {
    /// Floor between two analyses; turns keep accumulating meanwhile.
    pub min_analysis_interval: Duration,
}

impl Default for EngineParams {
    fn default() -> Self {
        Self { min_analysis_interval: Duration::from_secs(5) }
    }
}

enum Cmd {
    Turn(Turn),
    Ask(String),
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
    tokio::spawn(Engine { config, backend, params, events: ev_tx }.run(rx));
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
                Some(Cmd::Ask(question)) => {
                    self.converse(&mut session, &question).await;
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

            self.maybe_analyze(&mut session, &mut pending, &mut last_analysis).await;
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

        let input = pending
            .iter()
            .map(|t| format!("{}: {}", t.speaker.as_str(), t.text))
            .collect::<Vec<_>>()
            .join("\n");
        *last_analysis = Some(Instant::now());
        if self.converse(session, &input).await {
            pending.clear();
        }
        // On failure turns stay buffered; the next trigger retries them.
    }

    /// One round-trip to the model: open the session if needed, send, apply
    /// the [`NO_RESP`] discipline. Returns whether the input was delivered.
    async fn converse(&self, session: &mut Option<Box<dyn AssistSession>>, input: &str) -> bool {
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
        input: &str,
    ) -> Result<()> {
        if session.is_none() {
            let prompt = self.config.system_prompt().await?;
            *session = Some(self.backend.begin(&prompt).await?);
        }
        let reply = session.as_mut().unwrap().send(input).await?;
        match reply {
            Some(text) if !text.trim().is_empty() && !text.contains(NO_RESP) => {
                let _ = self.events.send(AssistEvent::Update(text));
            }
            // Empty or [NO_RESP]: the UI is not touched at all.
            _ => {}
        }
        Ok(())
    }
}

pub mod echo;

#[cfg(test)]
mod tests;
