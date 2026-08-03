use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::echo::StaticConfig;
use crate::{AssistBackend, AssistEvent, AssistSession, EngineParams, Speaker, Turn, NO_RESP};

/// Backend that records every input and replies from a scripted queue.
/// `None` in the script = reply with `[NO_RESP]`; an exhausted script echoes.
struct ScriptedBackend {
    begins: Arc<AtomicUsize>,
    sends: Arc<AtomicUsize>,
    inputs: Arc<Mutex<Vec<String>>>,
    script: Arc<Mutex<Vec<Option<String>>>>,
    fail_sends: Arc<AtomicUsize>,
}

impl ScriptedBackend {
    fn new() -> Self {
        Self {
            begins: Arc::new(AtomicUsize::new(0)),
            sends: Arc::new(AtomicUsize::new(0)),
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
            sends: self.sends.clone(),
            inputs: self.inputs.clone(),
            script: self.script.clone(),
            fail_sends: self.fail_sends.clone(),
        }))
    }
}

struct ScriptedSession {
    sends: Arc<AtomicUsize>,
    inputs: Arc<Mutex<Vec<String>>>,
    script: Arc<Mutex<Vec<Option<String>>>>,
    fail_sends: Arc<AtomicUsize>,
}

#[async_trait]
impl AssistSession for ScriptedSession {
    async fn send(&mut self, input: &str) -> Result<Option<String>> {
        self.sends.fetch_add(1, Ordering::SeqCst);
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
    Turn {
        speaker,
        text: text.into(),
        at_secs: 0.0,
    }
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
    EngineParams {
        min_analysis_interval: Duration::from_secs(30),
    }
}

#[tokio::test(start_paused = true)]
async fn analysis_waits_for_threshold_and_formats_turns() {
    let backend = ScriptedBackend::new();
    let inputs = backend.inputs.clone();
    let config = StaticConfig {
        turns_before_analysis: 2,
        ..Default::default()
    };
    let (handle, mut rx) = crate::spawn(config, backend, params());

    handle.push_turn(turn(Speaker::Me, "привет"));
    assert!(
        drain(&mut rx).await.is_empty(),
        "one turn is below the threshold"
    );

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
    assert_eq!(
        inputs.lock().unwrap().as_slice(),
        ["me: привет\nthem: добрый день"]
    );
}

#[tokio::test(start_paused = true)]
async fn no_resp_never_touches_the_ui_when_nobody_asked() {
    let backend = ScriptedBackend::new();
    backend.script.lock().unwrap().push(None); // scripted [NO_RESP]
    let (handle, mut rx) = crate::spawn(StaticConfig::default(), backend, params());

    handle.push_turn(turn(Speaker::Them, "ну, посмотрим"));
    let events = drain(&mut rx).await;
    assert_eq!(
        events,
        vec![AssistEvent::Loading(true), AssistEvent::Loading(false)]
    );
}

/// The mirror image of the test above. `[NO_RESP]` is the model's licence to
/// stay out of a conversation it was only watching — it is not a licence to
/// ignore a question the user typed and is sitting there waiting on. Silence
/// after pressing Enter is indistinguishable from a broken feature.
#[tokio::test(start_paused = true)]
async fn a_typed_question_is_always_answered() {
    let backend = ScriptedBackend::new();
    backend.script.lock().unwrap().push(None); // the model reaches for [NO_RESP]
    let (handle, mut rx) = crate::spawn(StaticConfig::default(), backend, params());

    handle.ask("сколько стоит?".into());
    let events = drain(&mut rx).await;

    let update = events.iter().find_map(|e| match e {
        AssistEvent::Update(text) => Some(text.clone()),
        _ => None,
    });
    let update = update.expect("a question must produce an answer in the panel");
    assert!(
        !update.contains(NO_RESP),
        "the marker itself must never be shown: {update}"
    );
    assert!(!update.trim().is_empty());
}

/// And when the model answers *and* tacks the marker on — which they do — the
/// answer survives and the marker does not.
#[tokio::test(start_paused = true)]
async fn an_answer_carrying_the_marker_keeps_the_answer() {
    let backend = ScriptedBackend::new();
    backend
        .script
        .lock()
        .unwrap()
        .push(Some(format!("**Около 200 тысяч** {NO_RESP}")));
    let (handle, mut rx) = crate::spawn(StaticConfig::default(), backend, params());

    handle.ask("сколько стоит?".into());
    let events = drain(&mut rx).await;

    assert!(events.contains(&AssistEvent::Update("**Около 200 тысяч**".into())));
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

    assert_eq!(
        begins.load(Ordering::SeqCst),
        1,
        "ask must reuse the meeting session"
    );
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
    assert!(
        drain(&mut rx).await.is_empty(),
        "throttle must hold the second analysis"
    );

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
    let config = StaticConfig {
        enabled: false,
        ..Default::default()
    };
    let (handle, mut rx) = crate::spawn(config, backend, params());

    for _ in 0..5 {
        handle.push_turn(turn(Speaker::Me, "..."));
    }
    assert!(drain(&mut rx).await.is_empty());
    assert!(inputs.lock().unwrap().is_empty());
}

/// A new meeting must not inherit the previous conversation: the backend is
/// asked to `begin` again, and turns left over from the old meeting are gone.
#[tokio::test(start_paused = true)]
async fn reset_starts_a_fresh_conversation() {
    let backend = ScriptedBackend::new();
    let begins = backend.begins.clone();
    let inputs = backend.inputs.clone();
    let config = StaticConfig {
        turns_before_analysis: 2,
        ..Default::default()
    };
    let (handle, mut rx) = crate::spawn(config, backend, params());

    handle.push_turn(turn(Speaker::Me, "первая встреча"));
    handle.push_turn(turn(Speaker::Them, "ответ клиента"));
    drain(&mut rx).await;
    assert_eq!(begins.load(Ordering::SeqCst), 1);

    handle.reset();
    // A turn that arrived before the boundary must not surface afterwards.
    handle.push_turn(turn(Speaker::Me, "вторая встреча"));
    handle.push_turn(turn(Speaker::Them, "другой клиент"));
    drain(&mut rx).await;

    assert_eq!(
        begins.load(Ordering::SeqCst),
        2,
        "reset must open a new session"
    );
    assert_eq!(
        inputs.lock().unwrap().as_slice(),
        [
            "me: первая встреча\nthem: ответ клиента",
            "me: вторая встреча\nthem: другой клиент"
        ]
    );
}

/// Turns still below the analysis threshold ride along with the question —
/// "what do I answer?" is about what was just said.
#[tokio::test(start_paused = true)]
async fn ask_carries_the_pending_turns() {
    let backend = ScriptedBackend::new();
    let inputs = backend.inputs.clone();
    let config = StaticConfig {
        turns_before_analysis: 5,
        ..Default::default()
    };
    let (handle, mut rx) = crate::spawn(config, backend, params());

    handle.push_turn(turn(Speaker::Them, "это дорого"));
    assert!(
        drain(&mut rx).await.is_empty(),
        "below the threshold, nothing sent"
    );

    handle.ask("что ответить?".into());
    drain(&mut rx).await;
    assert_eq!(
        inputs.lock().unwrap().as_slice(),
        ["them: это дорого\n\nчто ответить?"]
    );

    // Delivered with the question — the next analysis must not repeat them.
    tokio::time::advance(Duration::from_secs(31)).await;
    for _ in 0..5 {
        handle.push_turn(turn(Speaker::Me, "новое"));
    }
    drain(&mut rx).await;
    let sent = inputs.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert!(
        !sent[1].contains("это дорого"),
        "pending must be cleared: {:?}",
        sent[1]
    );
}

/// With the feature flag off a question is refused visibly, not swallowed and
/// not sent to the model.
#[tokio::test(start_paused = true)]
async fn ask_is_refused_when_disabled() {
    let backend = ScriptedBackend::new();
    let inputs = backend.inputs.clone();
    let config = StaticConfig {
        enabled: false,
        ..Default::default()
    };
    let (handle, mut rx) = crate::spawn(config, backend, params());

    handle.ask("вопрос".into());
    let events = drain(&mut rx).await;

    assert!(
        inputs.lock().unwrap().is_empty(),
        "nothing may reach the model"
    );
    assert!(
        events.iter().any(|e| matches!(e, AssistEvent::Error(_))),
        "the user must see the refusal: {events:?}"
    );
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

    // The retry backs off (here: double the 30 s throttle), then the deadline
    // retries the kept turn on its own — no new turn needed, nothing lost.
    tokio::time::advance(Duration::from_secs(61)).await;
    let events = drain(&mut rx).await;
    assert!(
        events.contains(&AssistEvent::Update("echo: me: важная реплика".into())),
        "kept turn must be retried after the backoff: {events:?}"
    );
    assert_eq!(inputs.lock().unwrap().as_slice(), ["me: важная реплика"]);
}

/// A backend that stays down used to be retried every throttle interval, the
/// same red line pushed to the panel every time — and each retry of a failing
/// `begin` respawns the agent behind it. Retries now back off exponentially,
/// and an unchanged failure is reported once.
#[tokio::test(start_paused = true)]
async fn repeated_failures_back_off_and_report_once() {
    let backend = ScriptedBackend::new();
    let sends = backend.sends.clone();
    backend.fail_sends.store(2, Ordering::SeqCst);
    let (handle, mut rx) = crate::spawn(StaticConfig::default(), backend, params());

    handle.push_turn(turn(Speaker::Me, "важная реплика"));
    let events = drain(&mut rx).await;
    assert_eq!(sends.load(Ordering::SeqCst), 1);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AssistEvent::Error(_)))
            .count(),
        1,
        "the failure is reported exactly once: {events:?}"
    );

    // Backoff doubled past the plain interval: 31 s in, no retry yet.
    tokio::time::advance(Duration::from_secs(31)).await;
    let events = drain(&mut rx).await;
    assert!(
        events.is_empty(),
        "backoff must hold the retry past the plain interval: {events:?}"
    );
    assert_eq!(sends.load(Ordering::SeqCst), 1);

    // Past the doubled wait the retry fires and fails the same way — the
    // panel is not told again.
    tokio::time::advance(Duration::from_secs(30)).await;
    let events = drain(&mut rx).await;
    assert_eq!(sends.load(Ordering::SeqCst), 2, "the retry must fire");
    assert!(
        events.iter().all(|e| !matches!(e, AssistEvent::Error(_))),
        "an unchanged failure must not be re-reported: {events:?}"
    );

    // The backend recovers: the next retry delivers the kept turn, and the
    // backoff resets to the plain interval.
    tokio::time::advance(Duration::from_secs(61)).await;
    let events = drain(&mut rx).await;
    assert!(
        events.contains(&AssistEvent::Update("echo: me: важная реплика".into())),
        "recovery must deliver the kept turn: {events:?}"
    );

    handle.push_turn(turn(Speaker::Them, "ещё реплика"));
    tokio::time::advance(Duration::from_secs(31)).await;
    let events = drain(&mut rx).await;
    assert!(
        events.contains(&AssistEvent::Update("echo: them: ещё реплика".into())),
        "after a success the plain throttle applies again: {events:?}"
    );
}
