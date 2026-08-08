//! Tests drive a **fake agent** — a shell script that speaks just enough ACP —
//! so the whole backend is exercised without an agent binary, a model or a
//! network. Same trick `gilb-analyzer` uses for `claude`.
//!
//! What is tested here is gilb's half: silence instead of a late suggestion,
//! text only, nobody to answer a permission dialog, and a system prompt that
//! rides in on the first turn. The wire itself — id spellings, process groups,
//! abandoned turns, a knob that withdraws another — belongs to `acp-client`
//! and is pinned by its suite, not duplicated here.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

use super::*;

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
        turn_timeout: Duration::from_secs(5),
        startup_timeout: Duration::from_secs(5),
        ..AcpConfig::default()
    }
}

/// Reads a line per request and answers by method name.
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
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"**Спроси"}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":" про бюджет**"}}}}}}}}\n'
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

/// Thoughts and tool calls are not suggestions: only the answer reaches the
/// panel, or the overlay fills with the agent thinking out loud.
#[tokio::test]
async fn ignores_thoughts_and_tool_calls() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        &format!(
            r#"{HANDSHAKE}
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-1","update":{{"sessionUpdate":"agent_thought_chunk","content":{{"type":"text","text":"надо посмотреть CRM"}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-1","update":{{"sessionUpdate":"tool_call","title":"crm_sql"}}}}}}\n'
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"уточни сроки"}}}}}}}}\n'
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

/// A permission prompt with nobody watching would hang the turn forever, so
/// the client answers it itself and the agent finishes with what it has.
#[tokio::test]
async fn refuses_permission_requests_instead_of_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        &format!(
            r#"{HANDSHAKE}
printf '{{"jsonrpc":"2.0","id":99,"method":"session/request_permission","params":{{"sessionId":"s-1","toolCall":{{"title":"Bash(ls)"}},"options":[{{"optionId":"y","kind":"allow_once"}}]}}}}\n'
read_line   # our answer to the permission request
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"без инструментов: спроси про бюджет"}}}}}}}}\n'
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
read_line() {{ IFS= read -r line; }}
read_line
printf '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1}}}}\n'
read_line
printf '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"s-1"}}}}\n'
IFS= read -r line; printf '%s\n' "$line" >> {out}
printf '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}\n'
IFS= read -r line; printf '%s\n' "$line" >> {out}
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

/// `AssistSession::send` must be idempotent on failure: the engine keeps the
/// turns buffered and calls again with the same input.
///
/// Cleared before the turn was made, a failed first `send` would take the
/// instructions with it — and the agent would prompt the whole meeting having
/// never been told what it is for.
#[tokio::test]
async fn a_system_prompt_survives_a_failed_first_turn() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("seen.txt");
    let bin = fake_agent(
        &dir,
        &format!(
            r#"
read_line() {{ IFS= read -r line; }}
read_line
printf '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1}}}}\n'
read_line
printf '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"s-1"}}}}\n'
IFS= read -r line; printf '%s\n' "$line" >> {out}
printf '{{"jsonrpc":"2.0","id":3,"error":{{"code":-32000,"message":"overloaded"}}}}\n'
IFS= read -r line; printf '%s\n' "$line" >> {out}
printf '{{"jsonrpc":"2.0","id":4,"result":{{"stopReason":"end_turn"}}}}\n'
sleep 5
"#,
            out = out.display()
        ),
    );

    let backend = AcpBackend::new(config(bin));
    let mut session = backend.begin("ТЫ СУФЛЁР").await.unwrap();

    assert!(
        session.send("them: раз").await.is_err(),
        "the agent refused"
    );
    let _ = session.send("them: раз").await.unwrap();

    let seen = std::fs::read_to_string(&out).unwrap();
    let mut lines = seen.lines();
    assert!(lines.next().unwrap().contains("ТЫ СУФЛЁР"));
    assert!(
        lines.next().unwrap().contains("ТЫ СУФЛЁР"),
        "a retry after a failed turn still carries the instructions"
    );
}

/// The knobs the user chose for this feature are applied before the first
/// suggestion, not after it.
#[tokio::test]
async fn the_chosen_model_is_applied_before_the_first_turn() {
    let dir = tempfile::tempdir().unwrap();
    let asked = dir.path().join("asked.log");
    let bin = fake_agent(
        &dir,
        &format!(
            r#"
read_line() {{ IFS= read -r line; printf '%s\n' "$line" >> {log}; }}
read_line   # initialize
printf '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1}}}}\n'
read_line   # session/new
printf '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"s-1","configOptions":[{{"id":"model"}}]}}}}\n'
read_line   # set_config_option(model)
printf '{{"jsonrpc":"2.0","id":3,"result":{{"configOptions":[{{"id":"model"}}]}}}}\n'
read_line   # and only then the prompt
printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"готово"}}}}}}}}\n'
printf '{{"jsonrpc":"2.0","id":4,"result":{{"stopReason":"end_turn"}}}}\n'
sleep 5
"#,
            log = asked.display()
        ),
    );

    let mut cfg = config(bin);
    cfg.config_options = vec![("model".to_string(), "haiku".to_string())];
    let backend = AcpBackend::new(cfg);
    let mut session = backend.begin("").await.unwrap();

    assert_eq!(
        session.send("them: привет").await.unwrap().as_deref(),
        Some("готово")
    );
    let log = std::fs::read_to_string(&asked).unwrap_or_default();
    let mut lines = log.lines().skip(2);
    assert!(
        lines
            .next()
            .unwrap_or_default()
            .contains(r#""value":"haiku""#),
        "the model is set before anything is asked: {log}"
    );
}

/// The agent is spawned with the PATH we hand it, not the one we inherited.
///
/// This is what broke the packaged app while dev worked fine: an `npx` adapter
/// is a `#!/usr/bin/env node` script, so it resolves `node` by name from its
/// own PATH, and an `.app` from Finder is handed launchd's, where no node
/// exists.
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

/// The panel offers controls only for shapes the agent actually sends: a
/// control for a type nobody has produced is a guess.
#[tokio::test]
async fn only_selectable_options_are_offered_to_the_panel() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(
        &dir,
        r#"
IFS= read -r line
printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}\n'
IFS= read -r line
printf '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s-1","configOptions":[{"id":"model","name":"Model","type":"select","category":"model","currentValue":"haiku","options":[{"value":"haiku","name":"Haiku"}]},{"id":"note","type":"string","currentValue":"x"},{"id":"empty","type":"select","options":[]}]}}\n'
sleep 5
"#,
    );

    let options = probe_session_options(&config(bin)).await.unwrap();

    assert_eq!(
        options.len(),
        1,
        "a string knob and an empty one are not controls"
    );
    assert_eq!(options[0].id, "model");
    assert_eq!(options[0].choices[0].value, "haiku");
}

#[test]
fn agent_availability_follows_the_binary() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_agent(&dir, "exit 0\n");

    assert!(agent_available(&bin));
    assert!(!agent_available(&dir.path().join("nope")));
}
