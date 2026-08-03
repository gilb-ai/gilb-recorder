//! Tests drive a **fake agent** — a shell script that speaks just enough ACP —
//! so the whole client is exercised without an agent binary, a model or a
//! network. Same trick `gilb-analyzer` uses for `claude`.

#![cfg(unix)]

use super::*;
use std::os::unix::fs::PermissionsExt;

/// Writes an executable fake agent and returns its path (kept alive by `dir`).
fn fake_agent(dir: &tempfile::TempDir, body: &str) -> PathBuf {
    let path = dir.path().join("fake-agent");
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn config(bin: PathBuf) -> AcpConfig {
    AcpConfig {
        bin,
        args: Vec::new(),
        cwd: std::env::temp_dir(),
        turn_timeout: Duration::from_secs(5),
        startup_timeout: Duration::from_secs(5),
        ..AcpConfig::default()
    }
}

/// Reads a line per request and answers by method name. `$1` is the extra
/// behaviour injected per test between the handshake and the prompt reply.
const HANDSHAKE: &str = r#"
read_line() { IFS= read -r line; }
read_line   # initialize
printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}\n'
read_line   # session/new
printf '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s-1"}}\n'
read_line   # session/prompt
"#;

#[tokio::test]
async fn streams_the_answer_and_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        &format!(
            r#"{HANDSHAKE}
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"**Спроси"}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":" про бюджет**"}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}\n'
sleep 5
"#
        ),
    );

    let backend = AcpBackend::new(config(bin));
    let mut session = backend.begin("ты суфлёр").await.unwrap();

    let reply = session.send("them: дорого").await.unwrap();
    assert_eq!(reply.as_deref(), Some("**Спроси про бюджет**"));
}

/// Thoughts and tool calls are not suggestions: only agent_message_chunk text
/// reaches the panel, or the overlay fills with the agent thinking out loud.
#[tokio::test]
async fn ignores_thoughts_and_tool_calls() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        &format!(
            r#"{HANDSHAKE}
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"agent_thought_chunk","content":{{"type":"text","text":"надо посмотреть CRM"}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"tool_call","title":"crm_sql"}}}}}}\n'
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"уточни сроки"}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}\n'
sleep 5
"#
        ),
    );

    let backend = AcpBackend::new(config(bin));
    let mut session = backend.begin("").await.unwrap();

    assert_eq!(
        session.send("them: дорого").await.unwrap().as_deref(),
        Some("уточни сроки")
    );
}

/// A permission prompt with nobody watching would hang the turn forever, so the
/// client answers it itself and the agent finishes with what it has.
#[tokio::test]
async fn refuses_permission_requests_instead_of_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        &format!(
            r#"{HANDSHAKE}
printf '{{"jsonrpc":"2.0","id":99,"method":"session/request_permission","params":{{"sessionId":"s-1"}}}}\n'
read_line   # our answer to the permission request
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"без инструментов: спроси про бюджет"}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}\n'
sleep 5
"#
        ),
    );

    let backend = AcpBackend::new(config(bin));
    let mut session = backend.begin("").await.unwrap();

    let reply = session.send("them: дорого").await.unwrap();
    assert_eq!(
        reply.as_deref(),
        Some("без инструментов: спроси про бюджет")
    );
}

/// A late suggestion is worse than none: past the deadline the turn yields
/// silence, not an error the engine would surface as a red line in the panel.
#[tokio::test]
async fn a_slow_agent_yields_silence() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(&dir, &format!("{HANDSHAKE}\nsleep 5\n"));

    let mut config = config(bin);
    config.turn_timeout = Duration::from_millis(300);
    let backend = AcpBackend::new(config);
    let mut session = backend.begin("").await.unwrap();

    assert_eq!(session.send("them: дорого").await.unwrap(), None);
}

/// A dead agent must fail the turn, not wait forever: the engine keeps the
/// turns buffered and retries, which is the right behaviour only if it is told.
#[tokio::test]
async fn a_dead_agent_fails_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        r#"
IFS= read -r line
printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}\n'
IFS= read -r line
printf '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s-1"}}\n'
exit 0
"#,
    );

    let backend = AcpBackend::new(config(bin));
    let mut session = backend.begin("").await.unwrap();

    assert!(session.send("them: дорого").await.is_err());
}

/// The system prompt has no slot in ACP, so it rides in as the opening turn —
/// once, not on every suggestion.
#[tokio::test]
async fn system_prompt_leads_the_first_turn_only() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("seen.txt");
    let bin = fake_agent(
        &dir,
        &format!(
            r#"
IFS= read -r line
printf '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1}}}}\n'
IFS= read -r line
printf '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"s-1"}}}}\n'
IFS= read -r line
printf '%s\n' "$line" >> {out}
printf '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}\n'
IFS= read -r line
printf '%s\n' "$line" >> {out}
printf '{{"jsonrpc":"2.0","id":4,"result":{{"stopReason":"end_turn"}}}}\n'
sleep 5
"#,
            out = out.display()
        ),
    );

    let backend = AcpBackend::new(config(bin));
    let mut session = backend.begin("ТЫ СУФЛЁР").await.unwrap();
    let _ = session.send("them: раз").await.unwrap();
    let _ = session.send("them: два").await.unwrap();

    let seen = std::fs::read_to_string(&out).unwrap();
    let mut lines = seen.lines();
    assert!(lines.next().unwrap().contains("ТЫ СУФЛЁР"));
    assert!(!lines.next().unwrap().contains("ТЫ СУФЛЁР"));
}

#[test]
fn agent_availability_follows_the_binary() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(&dir, "exit 0\n");

    assert!(agent_available(&bin));
    assert!(!agent_available(&dir.path().join("nope")));
}

/// The leak that produced the orphans, pinned end to end.
///
/// An adapter is reached through `npx`, so the process we spawn goes on to
/// start the agent itself. Killing our own child leaves that grandchild
/// running — reparented to init, holding its memory. Here a shell stands in
/// for `npx` and a `sleep` for the agent: neither speaks ACP, so the
/// handshake times out, which is exactly the path that used to leak (the
/// options probe against a binary that is not an adapter).
///
/// Passing means the grandchild is gone and the registry is clean, without
/// anyone calling the reaper — the failure path took the whole group.
#[tokio::test]
async fn a_failed_handshake_takes_the_whole_process_group() {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = dir.path().join("agents.json");
    let marker = dir.path().join("grandchild.pid");

    let config = AcpConfig {
        bin: PathBuf::from("/bin/sh"),
        // Start a "child agent", write down its pid, then sit there saying
        // nothing — the shape of a binary that does not speak the protocol.
        args: vec![
            "-c".into(),
            format!(
                "sleep 60 & echo $! > {}; sleep 60",
                marker.to_string_lossy()
            ),
        ],
        startup_timeout: Duration::from_millis(600),
        registry: Some(registry.clone()),
        ..AcpConfig::default()
    };

    let err = match bootstrap(&config).await {
        Err(err) => err,
        // Not `expect_err`: the success value owns a live agent, and printing
        // it is neither possible nor the point.
        Ok(_) => panic!("a shell that says nothing cannot pass an ACP handshake"),
    };
    assert!(
        err.to_string().contains("did not answer the ACP handshake"),
        "unexpected failure: {err}"
    );

    let pid: i32 = std::fs::read_to_string(&marker)
        .expect("the stand-in agent recorded its pid")
        .trim()
        .parse()
        .expect("a pid");

    // The signal travels the group; give the kernel a moment to deliver it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(
        !alive,
        "the grandchild outlived the failed handshake — this is the leak"
    );

    let left = std::fs::read_to_string(&registry).unwrap_or_default();
    assert!(
        !left.contains("/bin/sh"),
        "a group that was cleaned up must not stay on the reaper's list: {left}"
    );
}

/// Choosing a model can withdraw the other knobs, and the client must notice.
///
/// The real case, from Claude Code's adapter: `model=haiku` succeeds and comes
/// back with an option set that no longer has `effort` in it — haiku has no
/// thinking tiers. Sending the remembered effort anyway earns an "Unknown
/// config option: effort" and a warning in the log on every single session.
///
/// The fake agent below withdraws the option the same way, and records what it
/// was asked for; passing means the client dropped the withdrawn knob and went
/// straight on with the meeting.
#[tokio::test]
async fn an_option_withdrawn_by_an_earlier_choice_is_not_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    let asked = dir.path().join("asked.log");
    let bin = fake_agent(
        &dir,
        &format!(
            r#"
read_line() {{ IFS= read -r line; printf '%s\n' "$line" >> {log}; }}
read_line   # initialize
printf '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1}}}}\n'
read_line   # session/new — both knobs on offer
printf '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"s-1","configOptions":[{{"id":"model"}},{{"id":"effort"}}]}}}}\n'
read_line   # set_config_option(model) — and effort goes away with the answer
printf '{{"jsonrpc":"2.0","id":3,"result":{{"configOptions":[{{"id":"model"}}]}}}}\n'
read_line   # must already be the prompt
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"готово"}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","id":4,"result":{{"stopReason":"end_turn"}}}}\n'
sleep 5
"#,
            log = asked.display()
        ),
    );

    let mut cfg = config(bin);
    cfg.config_options = vec![
        ("model".to_string(), "haiku".to_string()),
        ("effort".to_string(), "low".to_string()),
    ];

    let backend = AcpBackend::new(cfg);
    let mut session = backend.begin("ты суфлёр").await.unwrap();
    let reply = session.send("them: привет").await.unwrap();
    assert_eq!(reply.as_deref(), Some("готово"), "the meeting carried on");

    let log = std::fs::read_to_string(&asked).unwrap_or_default();
    assert!(
        log.contains(r#""configId":"model""#),
        "the model must still be applied: {log}"
    );
    assert!(
        !log.contains(r#""configId":"effort""#),
        "effort was withdrawn and must not be asked for: {log}"
    );
}

/// The agent is spawned with the PATH we hand it, not the one we inherited.
///
/// This is what broke the packaged app while dev worked fine: an `npx`
/// adapter is a `#!/usr/bin/env node` script, so it resolves `node` by name
/// from its own PATH. A terminal-launched build inherits a login shell's and
/// never notices; an `.app` from Finder gets launchd's, where no node exists,
/// and the agent dies before the handshake — "agent closed the connection".
#[tokio::test]
async fn the_agent_is_given_the_path_we_configured() {
    let dir = tempfile::tempdir().unwrap();
    let seen = dir.path().join("path.txt");
    let bin = fake_agent(
        &dir,
        &format!(
            r#"printf '%s' "$PATH" > {seen}
{HANDSHAKE}
printf '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}\n'
sleep 5
"#,
            seen = seen.display()
        ),
    );

    let mut cfg = config(bin);
    cfg.path_env = Some("/gilb-test-node-lives-here:/usr/bin:/bin".to_string());
    let backend = AcpBackend::new(cfg);
    let mut session = backend.begin("").await.unwrap();
    let _ = session.send("them: раз").await;

    let path = std::fs::read_to_string(&seen).expect("the agent recorded its PATH");
    assert!(
        path.starts_with("/gilb-test-node-lives-here"),
        "the agent must run with the PATH it was given, got {path}"
    );
}

/// An agent that never answers `session/set_config_option` must not wedge
/// the session: the knob is best-effort, the startup deadline applies to it
/// the same as to the handshake, and the meeting goes on without it.
#[tokio::test]
async fn a_silent_config_option_does_not_wedge_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        r#"
read_line() { IFS= read -r line; }
read_line   # initialize
printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}\n'
read_line   # session/new
printf '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s-1"}}\n'
read_line   # session/set_config_option — never answered
read_line   # session/prompt
printf '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"без настроек"}}}}\n'
printf '{"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}\n'
sleep 5
"#,
    );

    let mut cfg = config(bin);
    cfg.startup_timeout = Duration::from_secs(2);
    cfg.config_options = vec![("model".to_string(), "haiku".to_string())];
    let backend = AcpBackend::new(cfg);
    let mut session = tokio::time::timeout(Duration::from_secs(5), backend.begin(""))
        .await
        .expect("an unanswered config option must not wedge startup")
        .unwrap();

    let reply = session.send("them: дорого").await.unwrap();
    assert_eq!(
        reply.as_deref(),
        Some("без настроек"),
        "the meeting carried on"
    );
}

/// A request whose write fails must not leave its response slot behind —
/// otherwise every dead-agent turn piles a corpse into the pending map until
/// the connection dies.
#[tokio::test]
async fn a_failed_write_leaves_no_pending_request() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        r#"
IFS= read -r line
printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}\n'
IFS= read -r line
printf '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s-1"}}\n'
exit 0
"#,
    );

    let mut session = AcpSession::start(config(bin), String::new()).await.unwrap();
    // Let the agent's exit settle so the write below is what fails.
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(session.send("them: дорого").await.is_err());
    assert!(
        session.conn.pending.lock().await.is_empty(),
        "a request that could not be written must not stay pending"
    );
}

/// A turn that times out keeps streaming on the agent's side: its late chunks
/// must not land in the NEXT turn's suggestion. The fake agent answers the
/// first prompt a full second past the deadline; passing means the second
/// turn contains only its own words.
#[tokio::test]
async fn late_chunks_do_not_leak_into_the_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        &format!(
            r#"{HANDSHAKE}
sleep 1
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"поздняя"}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}\n'
read_line   # session/cancel for the timed-out turn
read_line   # session/prompt #2
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"свежая"}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","id":4,"result":{{"stopReason":"end_turn"}}}}\n'
sleep 5
"#
        ),
    );

    let mut cfg = config(bin);
    // Long enough that turn #2 (answered as soon as the agent's sleep ends)
    // still fits, short enough that turn #1 is abandoned mid-sleep.
    cfg.turn_timeout = Duration::from_millis(700);
    let backend = AcpBackend::new(cfg);
    let mut session = backend.begin("").await.unwrap();

    assert_eq!(
        session.send("them: раз").await.unwrap(),
        None,
        "the slow turn is abandoned"
    );
    let reply = session.send("them: два").await.unwrap();
    assert_eq!(
        reply.as_deref(),
        Some("свежая"),
        "no words from the dead turn may leak in"
    );
}
