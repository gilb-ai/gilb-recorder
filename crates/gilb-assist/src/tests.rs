use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::echo::StaticConfig;
use crate::{
    AssistBackend, AssistEvent, AssistSession, EngineParams, Speaker, Turn, NO_RESP,
};

/// Backend that records every input and replies from a scripted queue.
/// `None` in the script = reply with `[NO_RESP]`; an exhausted script echoes.
struct ScriptedBackend {
    begins: Arc<AtomicUsize>,
    inputs: Arc<Mutex<Vec<String>>>,
    script: Arc<Mutex<Vec<Option<String>>>>,
    fail_sends: Arc<AtomicUsize>,
}

impl ScriptedBackend {
    fn new() -> Self {
        Self {
            begins: Arc::new(AtomicUsize::new(0)),
            inputs: Arc::new(Mutex::new(Vec::new())),
            script: Arc::new(Mutex::new(Vec::new())),
            fail_sends: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl AssistBackend for ScriptedBackend {
    async fn begin(&self, _system_prompt: &str) -> Result<Box<dyn AssistSession>> {
        self.begins.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(ScriptedSession {
            inputs: self.inputs.clone(),
            script: self.script.clone(),
            fail_sends: self.fail_sends.clone(),
        }))
    }
}

struct ScriptedSession {
    inputs: Arc<Mutex<Vec<String>>>,
    script: Arc<Mutex<Vec<Option<String>>>>,
    fail_sends: Arc<AtomicUsize>,
}

#[async_trait]
impl AssistSession for ScriptedSession {
    async fn send(&mut self, input: &str) -> Result<Option<String>> {
        if self.fail_sends.load(Ordering::SeqCst) > 0 {
            self.fail_sends.fetch_sub(1, Ordering::SeqCst);
            anyhow::bail!("model unreachable");
        }
        self.inputs.lock().unwrap().push(input.to_string());
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            Ok(Some(format!("echo: {input}")))
        } else {
            Ok(script.remove(0).or_else(|| Some(NO_RESP.to_string())))
        }
    }
}

fn turn(speaker: Speaker, text: &str) -> Turn {
    Turn { speaker, text: text.into(), at_secs: 0.0 }
}

/// Drain everything currently queued, giving the engine task time to run.
async fn drain(rx: &mut UnboundedReceiver<AssistEvent>) -> Vec<AssistEvent> {
    tokio::task::yield_now().await;
    let mut got = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        got.push(ev);
    }
    got
}

fn params() -> EngineParams {
    EngineParams { min_analysis_interval: Duration::from_secs(30) }
}

#[tokio::test(start_paused = true)]
async fn analysis_waits_for_threshold_and_formats_turns() {
    let backend = ScriptedBackend::new();
    let inputs = backend.inputs.clone();
    let config = StaticConfig { turns_before_analysis: 2, ..Default::default() };
    let (handle, mut rx) = crate::spawn(config, backend, params());

    handle.push_turn(turn(Speaker::Me, "привет"));
    assert!(drain(&mut rx).await.is_empty(), "one turn is below the threshold");

    handle.push_turn(turn(Speaker::Them, "добрый день"));
    let events = drain(&mut rx).await;
    assert_eq!(
        events,
        vec![
            AssistEvent::Loading(true),
            AssistEvent::Update("echo: me: привет\nthem: добрый день".into()),
            AssistEvent::Loading(false),
        ]
    );
    assert_eq!(inputs.lock().unwrap().as_slice(), ["me: привет\nthem: добрый день"]);
}

#[tokio::test(start_paused = true)]
async fn no_resp_never_touches_the_ui() {
    let backend = ScriptedBackend::new();
    backend.script.lock().unwrap().push(None); // scripted [NO_RESP]
    let (handle, mut rx) = crate::spawn(StaticConfig::default(), backend, params());

    handle.push_turn(turn(Speaker::Them, "ну, посмотрим"));
    let events = drain(&mut rx).await;
    assert_eq!(events, vec![AssistEvent::Loading(true), AssistEvent::Loading(false)]);
}

#[tokio::test(start_paused = true)]
async fn ask_shares_the_session_with_analysis() {
    let backend = ScriptedBackend::new();
    let begins = backend.begins.clone();
    let inputs = backend.inputs.clone();
    let (handle, mut rx) = crate::spawn(StaticConfig::default(), backend, params());

    handle.push_turn(turn(Speaker::Them, "цена вопроса — миллион"));
    drain(&mut rx).await;
    handle.ask("что ответить про цену?".into());
    let events = drain(&mut rx).await;

    assert_eq!(begins.load(Ordering::SeqCst), 1, "ask must reuse the meeting session");
    assert_eq!(
        inputs.lock().unwrap().as_slice(),
        ["them: цена вопроса — миллион", "что ответить про цену?"]
    );
    assert!(events.contains(&AssistEvent::Update("echo: что ответить про цену?".into())));
}

#[tokio::test(start_paused = true)]
async fn throttle_batches_turns_until_the_interval_elapses() {
    let backend = ScriptedBackend::new();
    let inputs = backend.inputs.clone();
    let (handle, mut rx) = crate::spawn(StaticConfig::default(), backend, params());

    handle.push_turn(turn(Speaker::Me, "раз"));
    drain(&mut rx).await; // analysis #1 fires immediately

    handle.push_turn(turn(Speaker::Them, "два"));
    handle.push_turn(turn(Speaker::Them, "три"));
    assert!(drain(&mut rx).await.is_empty(), "throttle must hold the second analysis");

    tokio::time::advance(Duration::from_secs(31)).await;
    let events = drain(&mut rx).await;
    assert!(
        events.contains(&AssistEvent::Update("echo: them: два\nthem: три".into())),
        "queued turns must go out in one batch after the interval: {events:?}"
    );
    assert_eq!(inputs.lock().unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn disabled_flag_suppresses_analysis() {
    let backend = ScriptedBackend::new();
    let inputs = backend.inputs.clone();
    let config = StaticConfig { enabled: false, ..Default::default() };
    let (handle, mut rx) = crate::spawn(config, backend, params());

    for _ in 0..5 {
        handle.push_turn(turn(Speaker::Me, "..."));
    }
    assert!(drain(&mut rx).await.is_empty());
    assert!(inputs.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn failed_analysis_reports_and_retries_with_kept_turns() {
    let backend = ScriptedBackend::new();
    let inputs = backend.inputs.clone();
    backend.fail_sends.store(1, Ordering::SeqCst);
    let (handle, mut rx) = crate::spawn(StaticConfig::default(), backend, params());

    handle.push_turn(turn(Speaker::Me, "важная реплика"));
    let events = drain(&mut rx).await;
    assert!(
        events.iter().any(|e| matches!(e, AssistEvent::Error(_))),
        "failure must surface as an error event: {events:?}"
    );

    // The throttle deadline retries the kept turn on its own — no new turn
    // needed, nothing lost.
    tokio::time::advance(Duration::from_secs(31)).await;
    let events = drain(&mut rx).await;
    assert!(
        events.contains(&AssistEvent::Update("echo: me: важная реплика".into())),
        "kept turn must be retried after the interval: {events:?}"
    );
    assert_eq!(inputs.lock().unwrap().as_slice(), ["me: важная реплика"]);
}
