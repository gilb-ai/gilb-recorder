//! Batch meeting transcription via the remote OpenAI Whisper API (BYOK).
//!
//! After a recording finalizes, [`transcribe_meeting`] uploads the meeting's
//! `.audio.wav` and stores the result in `meeting_transcripts` (1:1 with
//! `meetings`). The OpenAI HTTP call lives behind the [`Transcriber`] trait so
//! the whole module — retry/backoff, response parsing, the DB upsert, and the
//! error path — is unit-testable with a mock, no network or API key required.
//! Only [`OpenAiTranscriber`] makes a real request.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use gilb_db::transcripts::{set_transcript_error, upsert_transcript};
use gilb_db::Db;
use serde::Deserialize;
use tracing::{info, warn};

/// OpenAI audio transcription endpoint.
const OPENAI_URL: &str = "https://api.openai.com/v1/audio/transcriptions";
/// Whisper model used for transcription.
pub const MODEL: &str = "whisper-1";
/// Total attempts before giving up (one initial try + retries).
const MAX_ATTEMPTS: usize = 3;

/// A successful transcription, split into the fields persisted by
/// [`upsert_transcript`]: the flat `text` and the `segments` array of
/// `verbose_json` serialized back to a JSON string.
#[derive(Debug, Clone)]
pub struct WhisperResponse {
    pub text: String,
    pub segments_json: String,
}

/// The OpenAI HTTP call, abstracted so tests can substitute a mock.
#[async_trait]
pub trait Transcriber: Send + Sync {
    /// Upload `audio_path` and return its transcription. Returns `Err` on any
    /// network / HTTP / parse failure (the caller retries).
    async fn transcribe(&self, audio_path: &Path, api_key: &str) -> Result<WhisperResponse>;
}

/// Real transcriber: a multipart POST to the OpenAI Whisper API.
pub struct OpenAiTranscriber {
    client: reqwest::Client,
}

impl OpenAiTranscriber {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for OpenAiTranscriber {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Transcriber for OpenAiTranscriber {
    async fn transcribe(&self, audio_path: &Path, api_key: &str) -> Result<WhisperResponse> {
        let bytes = tokio::fs::read(audio_path)
            .await
            .with_context(|| format!("read audio file {}", audio_path.display()))?;
        let file_name = audio_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("audio.wav")
            .to_string();

        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str("audio/wav")
            .context("build multipart audio part")?;
        let form = reqwest::multipart::Form::new()
            .text("model", MODEL)
            .text("response_format", "verbose_json")
            .part("file", part);

        let resp = self
            .client
            .post(OPENAI_URL)
            .bearer_auth(api_key)
            .multipart(form)
            .send()
            .await
            .context("send transcription request")?;

        let status = resp.status();
        let body = resp.text().await.context("read transcription response")?;
        if !status.is_success() {
            return Err(anyhow!("openai transcription failed ({status}): {body}"));
        }

        let (text, segments_json) = parse_response(&body)?;
        Ok(WhisperResponse {
            text,
            segments_json,
        })
    }
}

/// Exponential backoff between retries: 250ms, 500ms, 1s. `backoff_delays()[i]`
/// is slept *before* attempt `i + 1`, so the last entry is unused at
/// [`MAX_ATTEMPTS`] = 3 (kept for a 4th attempt and to keep the curve obvious).
pub fn backoff_delays() -> [Duration; 3] {
    [
        Duration::from_millis(250),
        Duration::from_millis(500),
        Duration::from_millis(1000),
    ]
}

/// Pure parse of a Whisper `verbose_json` body into the persisted
/// `(text, segments_json)` pair. `segments_json` is the `segments` array
/// re-serialized to a compact JSON string (empty array if absent).
pub fn parse_response(body: &str) -> Result<(String, String)> {
    #[derive(Deserialize)]
    struct Raw {
        text: String,
        #[serde(default)]
        segments: Vec<serde_json::Value>,
    }
    let raw: Raw = serde_json::from_str(body).context("parse whisper verbose_json")?;
    let segments_json =
        serde_json::to_string(&raw.segments).context("serialize transcript segments")?;
    Ok((raw.text, segments_json))
}

/// Transcribe `meeting_id`'s audio and persist the outcome.
///
/// Retries the upload up to [`MAX_ATTEMPTS`] times with exponential backoff. On
/// success the transcript is stored via [`upsert_transcript`]; once attempts are
/// exhausted the last error is recorded via [`set_transcript_error`] so the
/// failure is visible in the DB. Returns `Err` only if writing the DB row
/// itself fails.
pub async fn transcribe_meeting(
    db: &Db,
    api_key: &str,
    meeting_id: i64,
    audio_path: &Path,
    t: &dyn Transcriber,
) -> Result<()> {
    let delays = backoff_delays();
    let mut last_err: Option<String> = None;

    for (attempt, delay) in delays.iter().enumerate().take(MAX_ATTEMPTS) {
        match t.transcribe(audio_path, api_key).await {
            Ok(resp) => {
                upsert_transcript(db, meeting_id, &resp.text, &resp.segments_json, MODEL)
                    .await
                    .context("store transcript")?;
                info!(meeting_id, attempt, "meeting transcribed");
                return Ok(());
            }
            Err(err) => {
                let msg = err.to_string();
                warn!(meeting_id, attempt, error = %msg, "transcription attempt failed");
                last_err = Some(msg);
                if attempt + 1 < MAX_ATTEMPTS {
                    tokio::time::sleep(*delay).await;
                }
            }
        }
    }

    let error = last_err.unwrap_or_else(|| "transcription failed".to_string());
    warn!(meeting_id, error = %error, "transcription failed after retries");
    set_transcript_error(db, meeting_id, &error)
        .await
        .context("store transcript error")?;
    Ok(())
}
