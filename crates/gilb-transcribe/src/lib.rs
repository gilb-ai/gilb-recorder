//! Local meeting transcription via whisper.cpp (whisper-rs), fully on-device.
//!
//! A meeting records two audio channels — `mic.wav` (the local participant) and
//! `system.wav` (remote call participants). [`transcribe_meeting`] transcribes
//! each separately, tags every utterance with its [`Channel`], merges them by
//! start time, and stores the result in `meeting_transcripts` (1:1 with
//! `meetings`). Per-channel transcription gives a free local-vs-remote split;
//! the stored `channel` ("mic"/"system") is rendered as "Me"/"Others" upstream.
//!
//! An energy [`voiced_mask`] gates Whisper's silence hallucinations: a silent
//! channel is skipped entirely and segments landing in unvoiced spans are
//! dropped. The whisper.cpp call lives behind the [`Transcriber`] trait (and the
//! `local-whisper` feature) so the orchestration is unit-testable with a mock —
//! no model, no GPU. Only [`LocalTranscriber`] loads a real model.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use gilb_db::transcripts::{set_transcript_error, upsert_transcript};
use gilb_db::Db;
use tracing::{info, warn};

/// Model label stored in `meeting_transcripts.model`.
pub const MODEL: &str = "whisper-large-v3-turbo";

// ---------------------------------------------------------------------------
// Energy VAD (pure — no model, unit-testable). Operates on 16 kHz mono f32.
// ---------------------------------------------------------------------------

/// VAD analysis frame: 20 ms at 16 kHz.
pub const VAD_FRAME: usize = 320;
/// A channel whose loudest frames stay below this RMS is treated as pure silence.
const ABS_SILENCE: f32 = 0.01;
/// Minimum voiced time for a channel to be worth transcribing at all.
pub const MIN_VOICED_SECS: f32 = 0.3;
/// A Whisper segment is kept only if at least this fraction of it is voiced —
/// drops the "Thank you."/"Спасибо." hallucinations whisper emits over silence.
pub const SEG_VOICED_MIN: f32 = 0.2;

/// Per-frame voiced mask via RMS energy with an adaptive threshold. An all-false
/// mask means the channel is (near) silent.
pub fn voiced_mask(samples: &[f32]) -> Vec<bool> {
    let rms: Vec<f32> = samples
        .chunks(VAD_FRAME)
        .map(|c| (c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    if rms.is_empty() {
        return vec![];
    }
    let mut sorted = rms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let floor = sorted[sorted.len() * 10 / 100];
    let peak = sorted[sorted.len() * 95 / 100];
    if peak < ABS_SILENCE {
        return vec![false; rms.len()];
    }
    let thresh = (floor + 0.15 * (peak - floor)).max(0.006);
    rms.iter().map(|&e| e > thresh).collect()
}

/// Total voiced time in seconds implied by `mask`.
pub fn voiced_secs(mask: &[bool]) -> f32 {
    mask.iter().filter(|&&v| v).count() as f32 * VAD_FRAME as f32 / 16_000.0
}

/// Fraction of frames overlapping `[t0, t1]` (seconds) that are voiced.
pub fn voiced_fraction(mask: &[bool], t0: f32, t1: f32) -> f32 {
    let secs_per_frame = VAD_FRAME as f32 / 16_000.0;
    let lo = ((t0 / secs_per_frame).floor() as usize).min(mask.len());
    let hi = ((t1 / secs_per_frame).ceil() as usize).min(mask.len());
    if hi <= lo {
        return 0.0;
    }
    let voiced = mask[lo..hi].iter().filter(|&&v| v).count();
    voiced as f32 / (hi - lo) as f32
}

// ---------------------------------------------------------------------------
// Transcriber trait + merged segment model
// ---------------------------------------------------------------------------

/// Which audio channel an utterance came from. Stored verbatim ("mic"/"system")
/// in `segments_json`; presentation ("Me"/"Others") is decided upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Mic,
    System,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Mic => "mic",
            Channel::System => "system",
        }
    }
}

/// One transcribed span from a single channel (channel assigned by the caller).
#[derive(Debug, Clone)]
pub struct Utterance {
    /// Start time in seconds.
    pub t0: f32,
    /// End time in seconds.
    pub t1: f32,
    pub text: String,
}

/// A merged, channel-tagged utterance — the unit serialized to `segments_json`.
#[derive(Debug, Clone)]
pub struct Segment {
    pub t0: f32,
    pub t1: f32,
    pub channel: Channel,
    pub text: String,
}

/// Transcribes one audio file into VAD-filtered [`Utterance`]s. Returns an empty
/// vec for a silent file; `Err` only on a hard failure (unreadable file, model
/// error). Abstracted so the meeting flow is testable with a mock.
#[async_trait]
pub trait Transcriber: Send + Sync {
    async fn transcribe(&self, audio_path: &Path) -> Result<Vec<Utterance>>;
}

/// Serialize merged segments to the compact JSON stored in `segments_json`.
fn segments_to_json(segs: &[Segment]) -> String {
    let arr: Vec<serde_json::Value> = segs
        .iter()
        .map(|s| {
            serde_json::json!({
                "t0": s.t0,
                "t1": s.t1,
                "channel": s.channel.as_str(),
                "text": s.text,
            })
        })
        .collect();
    serde_json::Value::Array(arr).to_string()
}

/// Sibling path `<dir>/<name>` next to `audio_path`, or `None` if it has no parent.
fn sibling(audio_path: &Path, name: &str) -> Option<std::path::PathBuf> {
    audio_path.parent().map(|d| d.join(name))
}

/// Transcribe both channels next to `audio_path` and return their merged,
/// time-sorted segments. A channel file that doesn't exist is skipped.
async fn transcribe_channels(
    audio_path: &Path,
    t: &dyn Transcriber,
) -> Result<Vec<Segment>> {
    let mut segs = Vec::new();
    for (name, channel) in [("mic.wav", Channel::Mic), ("system.wav", Channel::System)] {
        let Some(path) = sibling(audio_path, name) else {
            continue;
        };
        if !path.exists() {
            continue;
        }
        let utts = t
            .transcribe(&path)
            .await
            .with_context(|| format!("transcribe {}", path.display()))?;
        segs.extend(utts.into_iter().map(|u| Segment {
            t0: u.t0,
            t1: u.t1,
            channel,
            text: u.text,
        }));
    }
    segs.sort_by(|a, b| a.t0.partial_cmp(&b.t0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(segs)
}

/// Transcribe `meeting_id`'s two audio channels and persist the outcome.
///
/// On success the merged transcript is stored via [`upsert_transcript`] (flat
/// `text` for search, `segments_json` carrying per-utterance `channel`/timings).
/// On a hard failure the error is recorded via [`set_transcript_error`] so it's
/// visible in the DB. Returns `Err` only if writing the DB row itself fails.
pub async fn transcribe_meeting(
    db: &Db,
    meeting_id: i64,
    audio_path: &Path,
    t: &dyn Transcriber,
) -> Result<()> {
    match transcribe_channels(audio_path, t).await {
        Ok(segs) => {
            let text = segs
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let json = segments_to_json(&segs);
            upsert_transcript(db, meeting_id, &text, &json, MODEL)
                .await
                .context("store transcript")?;
            info!(meeting_id, segments = segs.len(), "meeting transcribed");
            Ok(())
        }
        Err(err) => {
            let msg = format!("{err:#}");
            warn!(meeting_id, error = %msg, "transcription failed");
            set_transcript_error(db, meeting_id, &msg)
                .await
                .context("store transcript error")?;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// LocalTranscriber — the real whisper.cpp path (feature-gated).
// ---------------------------------------------------------------------------

#[cfg(feature = "local-whisper")]
mod local {
    use std::path::Path;
    use std::sync::Arc;

    use anyhow::{Context, Result};
    use async_trait::async_trait;
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

    use crate::{voiced_fraction, voiced_mask, voiced_secs, Transcriber, Utterance, MIN_VOICED_SECS, SEG_VOICED_MIN};

    /// On-device transcriber holding a loaded whisper.cpp model. The model is
    /// loaded once (it costs ~hundreds of MB + seconds) and reused across calls;
    /// each call creates a fresh decode state.
    pub struct LocalTranscriber {
        ctx: Arc<WhisperContext>,
        /// "auto" | "ru" | "en" — passed to whisper for language selection.
        language: String,
    }

    impl LocalTranscriber {
        /// Load `model_path` (a ggml model) for transcription in `language`.
        pub fn new(model_path: &Path, language: impl Into<String>) -> Result<Self> {
            let ctx = WhisperContext::new_with_params(
                &model_path.to_string_lossy(),
                WhisperContextParameters::default(),
            )
            .with_context(|| format!("load whisper model {}", model_path.display()))?;
            Ok(Self {
                ctx: Arc::new(ctx),
                language: language.into(),
            })
        }
    }

    /// Read a 16 kHz mono i16 wav as f32 in [-1, 1].
    fn read_wav_16k_mono(path: &Path) -> Result<Vec<f32>> {
        let mut reader = hound::WavReader::open(path)
            .with_context(|| format!("open wav {}", path.display()))?;
        let spec = reader.spec();
        anyhow::ensure!(spec.sample_rate == 16_000, "expected 16 kHz wav");
        anyhow::ensure!(spec.channels == 1, "expected mono wav");
        reader
            .samples::<i16>()
            .map(|s| Ok(s.context("read sample")? as f32 / 32768.0))
            .collect()
    }

    /// Blocking whisper.cpp pass over `samples`, returning raw segments.
    fn run_whisper(
        ctx: &WhisperContext,
        samples: &[f32],
        language: &str,
    ) -> Result<Vec<Utterance>> {
        let mut state = ctx.create_state().context("create whisper state")?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some(language));
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);
        state.full(params, samples).context("whisper full")?;

        let n = state.full_n_segments().context("n segments")?;
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let text = state
                .full_get_segment_text(i)
                .unwrap_or_default()
                .trim()
                .to_string();
            out.push(Utterance {
                t0: state.full_get_segment_t0(i).unwrap_or(0) as f32 / 100.0,
                t1: state.full_get_segment_t1(i).unwrap_or(0) as f32 / 100.0,
                text,
            });
        }
        Ok(out)
    }

    #[async_trait]
    impl Transcriber for LocalTranscriber {
        async fn transcribe(&self, audio_path: &Path) -> Result<Vec<Utterance>> {
            let samples = read_wav_16k_mono(audio_path)?;
            let mask = voiced_mask(&samples);
            if voiced_secs(&mask) < MIN_VOICED_SECS {
                return Ok(vec![]); // silent channel — skip, no hallucinations
            }

            let ctx = self.ctx.clone();
            let language = self.language.clone();
            let raw = tokio::task::spawn_blocking(move || run_whisper(&ctx, &samples, &language))
                .await
                .context("whisper task join")??;

            Ok(raw
                .into_iter()
                .filter(|u| !u.text.is_empty() && voiced_fraction(&mask, u.t0, u.t1) >= SEG_VOICED_MIN)
                .collect())
        }
    }
}

#[cfg(feature = "local-whisper")]
pub use local::LocalTranscriber;
