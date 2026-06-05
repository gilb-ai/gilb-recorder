//! Tests for gilb-transcribe: pure helpers (`backoff_delays`, `parse_response`)
//! and the `transcribe_meeting` retry/persist flow driven by a mock
//! [`Transcriber`] over an in-memory-style temp sqlite — no network, no key.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use gilb_db::meetings::insert_meeting;
use gilb_db::open_db;
use gilb_db::transcripts::get_transcript;
use gilb_transcribe::{
    backoff_delays, parse_response, transcribe_meeting, Transcriber, WhisperResponse, MODEL,
};
use uuid::Uuid;

fn temp_db_path() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("gilb-transcribe-test-{}.sqlite", Uuid::new_v4()));
    p
}

fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}

// ----- pure helpers -------------------------------------------------------

#[test]
fn backoff_delays_are_exponential() {
    let d = backoff_delays();
    assert_eq!(d[0], Duration::from_millis(250));
    assert_eq!(d[1], Duration::from_millis(500));
    assert_eq!(d[2], Duration::from_millis(1000));
}

#[test]
fn parse_response_extracts_text_and_segments() {
    let body = r#"{"text":"hello there","segments":[{"id":0,"text":"hello there"}]}"#;
    let (text, segments_json) = parse_response(body).expect("parse");
    assert_eq!(text, "hello there");
    assert!(segments_json.contains("\"id\":0"));
    assert!(segments_json.starts_with('['));
}

#[test]
fn parse_response_defaults_missing_segments_to_empty() {
    let (text, segments_json) = parse_response(r#"{"text":"hi"}"#).expect("parse");
    assert_eq!(text, "hi");
    assert_eq!(segments_json, "[]");
}

#[test]
fn parse_response_rejects_garbage() {
    assert!(parse_response("not json").is_err());
}

#[test]
fn key_validity_maps_status() {
    use gilb_transcribe::{key_validity_from_status, KeyValidity};
    use reqwest::StatusCode;

    assert_eq!(key_validity_from_status(StatusCode::OK), KeyValidity::Valid);
    assert_eq!(
        key_validity_from_status(StatusCode::UNAUTHORIZED),
        KeyValidity::Invalid
    );
    assert_eq!(
        key_validity_from_status(StatusCode::FORBIDDEN),
        KeyValidity::Invalid
    );
    assert_eq!(
        key_validity_from_status(StatusCode::TOO_MANY_REQUESTS),
        KeyValidity::Unknown
    );
    assert_eq!(
        key_validity_from_status(StatusCode::INTERNAL_SERVER_ERROR),
        KeyValidity::Unknown
    );
}

// ----- mocks --------------------------------------------------------------

/// Always succeeds with a fixed transcript.
struct OkMock;

#[async_trait]
impl Transcriber for OkMock {
    async fn transcribe(&self, _audio: &Path, _key: &str) -> anyhow::Result<WhisperResponse> {
        Ok(WhisperResponse {
            text: "transcribed text".to_string(),
            segments_json: r#"[{"id":0}]"#.to_string(),
        })
    }
}

/// Always fails, counting how many times it was called.
struct FailMock {
    calls: AtomicUsize,
}

#[async_trait]
impl Transcriber for FailMock {
    async fn transcribe(&self, _audio: &Path, _key: &str) -> anyhow::Result<WhisperResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(anyhow::anyhow!("simulated upload failure"))
    }
}

// ----- transcribe_meeting flow -------------------------------------------

#[tokio::test]
async fn success_populates_transcript_row() {
    let path = temp_db_path();
    let db = open_db(&path).await.expect("open_db");
    let mid = insert_meeting(&db, 0, "us.zoom.xos").await.expect("insert");

    transcribe_meeting(&db, "sk-test", mid, Path::new("/tmp/x.wav"), &OkMock)
        .await
        .expect("transcribe_meeting");

    let t = get_transcript(&db, mid).await.expect("get").expect("row");
    assert_eq!(t.text.as_deref(), Some("transcribed text"));
    assert_eq!(t.segments_json.as_deref(), Some(r#"[{"id":0}]"#));
    assert_eq!(t.model.as_deref(), Some(MODEL));
    assert!(t.error.is_none());

    db.close().await;
    cleanup(&path);
}

#[tokio::test]
async fn persistent_failure_records_error_after_three_tries() {
    let path = temp_db_path();
    let db = open_db(&path).await.expect("open_db");
    let mid = insert_meeting(&db, 0, "us.zoom.xos").await.expect("insert");

    let mock = FailMock {
        calls: AtomicUsize::new(0),
    };
    transcribe_meeting(&db, "sk-test", mid, Path::new("/tmp/x.wav"), &mock)
        .await
        .expect("transcribe_meeting persists error, not Err");

    assert_eq!(mock.calls.load(Ordering::SeqCst), 3, "retried 3 times");

    let t = get_transcript(&db, mid).await.expect("get").expect("row");
    assert_eq!(t.error.as_deref(), Some("simulated upload failure"));
    assert!(t.text.is_none());

    db.close().await;
    cleanup(&path);
}
