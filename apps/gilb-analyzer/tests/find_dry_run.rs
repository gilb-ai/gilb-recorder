//! End-to-end `find --dry-run`: seed a temp DB, point the runner at a fake
//! `claude` that emits a Therbligs array, and assert the pipeline parses it,
//! computes window volume, and reports a Produced run — all without network.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;

use gilb_analyzer::claude::ClaudeRunner;
use gilb_analyzer::pipeline::{run_find, Window};
use gilb_analyzer::run::Outcome;
use gilb_analyzer::web::Web;
use gilb_config::AnalyzerConfig;

const FROM: &str = "2026-06-06T00:00:00Z";
const TO: &str = "2026-06-06T01:00:00Z";

async fn seed_db(path: &std::path::Path) -> sqlx::SqlitePool {
    let pool = gilb_db::open_db(path).await.expect("open+migrate db");
    sqlx::query(
        "INSERT INTO sessions (started_at, gilb_version, host_os) VALUES (?1, '0', 'test')",
    )
    .bind(FROM)
    .execute(&pool)
    .await
    .unwrap();
    for (kind, app, text) in [
        ("focus_change", Some("Google Chrome"), None),
        ("click", Some("Google Chrome"), None),
        ("text", Some("Google Chrome"), Some("hello there")),
    ] {
        sqlx::query(
            "INSERT INTO actions (session_id, captured_at, kind, app_name, text_content, password_flag)
             VALUES (1, ?1, ?2, ?3, ?4, 0)",
        )
        .bind("2026-06-06T00:10:00Z")
        .bind(kind)
        .bind(app)
        .bind(text)
        .execute(&pool)
        .await
        .unwrap();
    }
    pool
}

fn fake_claude(dir: &std::path::Path) -> String {
    let arr = r#"[{"title":"Investor research","intent_summary":"looked up rounds","time_window_from":"2026-06-06T00:00:00Z","time_window_to":"2026-06-06T00:30:00Z","steps":[{"label":"Open search","delegation":"fully"},{"label":"Judge relevance","delegation":"human"}],"evidence":[{"captured_at":"2026-06-06T00:10:00Z","app":"Google Chrome","summary":"opened advanced search"}]}]"#;
    let result = serde_json::json!({
        "is_error": false, "num_turns": 2, "duration_ms": 1000,
        "total_cost_usd": 0.02, "model": "claude-opus-4-8",
        "result": arr,
        "usage": {"input_tokens": 1234, "output_tokens": 56}
    })
    .to_string();
    let script = dir.join("claude");
    // `result` is a JSON string with only double quotes, so single-quoting it
    // for the shell is safe.
    std::fs::write(
        &script,
        format!("#!/bin/sh\ncat > /dev/null\nprintf '%s' '{result}'\n"),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script.to_string_lossy().to_string()
}

fn config() -> AnalyzerConfig {
    let mut prompts = BTreeMap::new();
    prompts.insert("therblig-finder".to_string(), "FIND THERBLIGS".to_string());
    AnalyzerConfig {
        version: 9,
        prompts,
        analyze_interval_secs: Some(3600),
        etag: None,
    }
}

#[tokio::test]
async fn find_dry_run_parses_and_accounts_without_network() {
    let dir = std::env::temp_dir().join(format!("gilb-find-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pool = seed_db(&dir.join("db.sqlite")).await;
    let runner = ClaudeRunner::new()
        .bin(fake_claude(&dir))
        .skip_permissions(false);
    // Unreachable on purpose; dry-run must not touch it.
    let web = Web::new("http://127.0.0.1:0", "token");

    let window = Window { from: FROM, to: TO };
    let summary = run_find(&pool, &config(), &runner, &web, window, true)
        .await
        .expect("find dry-run");

    assert_eq!(summary.new_therbligs.len(), 1);
    assert_eq!(summary.new_therbligs[0].title, "Investor research");
    assert_eq!(summary.run.outcome, Outcome::Produced);

    // Run accounting: usage from claude, volume from the DB window.
    assert_eq!(summary.run.usage.input_tokens, 1234);
    assert_eq!(summary.run.input.llm_input_tokens, 1234);
    assert_eq!(summary.run.config_version, 9);
    assert_eq!(summary.run.input.window_secs, 3600);
    assert_eq!(summary.run.input.source.actions_total, 3);
    assert_eq!(summary.run.input.source.segments, 1); // one focus_change segment
                                                      // dry-run pushed nothing.
    assert!(summary.run.therbligs_created.is_empty());
    // a run_id is always generated (links Therbligs ↔ run for cost).
    assert!(!summary.run.run_id.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}
