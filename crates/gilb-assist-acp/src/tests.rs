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
