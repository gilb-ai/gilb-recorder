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
    /// Neutral machine tag stored in the DB / `transcript.json` channel field.
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Mic => "mic",
            Channel::System => "system",
        }
    }

    /// Human-facing speaker label for the on-disk transcript files: the local
    /// participant (mic) is "Me", remote call audio (system) is "Others".
    pub fn speaker(self) -> &'static str {
        match self {
            Channel::Mic => "Me",
            Channel::System => "Others",
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

/// Format seconds as `M:SS` (or `H:MM:SS` past an hour) for the text transcript.
fn fmt_timestamp(secs: f32) -> String {
    let total = secs.max(0.0) as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Write the meeting's transcript into its folder as a human-readable
/// `transcript.txt` and a machine-readable `transcript.json`, both using the
/// "Me"/"Others" speaker labels. Best-effort: a write failure is logged, not
/// propagated — the DB row is the source of truth.
fn write_transcript_files(dir: &Path, segs: &[Segment]) {
    let mut txt = String::new();
    for s in segs {
        txt.push_str(&format!(
            "[{}] {}: {}\n",
            fmt_timestamp(s.t0),
            s.channel.speaker(),
            s.text
        ));
    }
    let json = serde_json::json!({
        "model": MODEL,
        "segments": segs
            .iter()
            .map(|s| serde_json::json!({
                "start": s.t0,
                "end": s.t1,
                "speaker": s.channel.speaker(),
                "text": s.text,
            }))
            .collect::<Vec<_>>(),
    });

    if let Err(err) = std::fs::write(dir.join("transcript.txt"), txt) {
        warn!(error = %err, "failed to write transcript.txt");
    }
    match serde_json::to_vec_pretty(&json) {
        Ok(bytes) => {
            if let Err(err) = std::fs::write(dir.join("transcript.json"), bytes) {
                warn!(error = %err, "failed to write transcript.json");
            }
        }
        Err(err) => warn!(error = %err, "failed to serialize transcript.json"),
    }
}

/// Transcribe both channels next to `audio_path` and return their merged,
/// time-sorted segments. A channel file that doesn't exist is skipped.
async fn transcribe_channels(audio_path: &Path, t: &dyn Transcriber) -> Result<Vec<Segment>> {
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
            // Also drop the transcript into the meeting folder (best-effort).
            if let Some(dir) = audio_path.parent() {
                write_transcript_files(dir, &segs);
            }
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

    use crate::{
        voiced_fraction, voiced_mask, voiced_secs, Transcriber, Utterance, MIN_VOICED_SECS,
        SEG_VOICED_MIN,
    };

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
            let ctx =
                WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
                    .with_context(|| format!("load whisper model {}", model_path.display()))?;
            Ok(Self {
                ctx: Arc::new(ctx),
                language: language.into(),
            })
        }
    }

    /// Read a 16 kHz mono i16 wav as f32 in [-1, 1].
    fn read_wav_16k_mono(path: &Path) -> Result<Vec<f32>> {
        let mut reader =
            hound::WavReader::open(path).with_context(|| format!("open wav {}", path.display()))?;
        let spec = reader.spec();
        anyhow::ensure!(spec.sample_rate == 16_000, "expected 16 kHz wav");
        anyhow::ensure!(spec.channels == 1, "expected mono wav");
        reader
            .samples::<i16>()
            .map(|s| Ok(s.context("read sample")? as f32 / 32768.0))
            .collect()
    }

    /// Seconds per language-probe window. 30s is whisper's own detection window;
    /// a longer probe wouldn't make the vote any better, only slower.
    const LANG_PROBE_SECS: f32 = 30.0;

    /// How many probe windows vote on the language. Three dense, well-separated
    /// stretches outvote a single unlucky one — an opening line in the "wrong"
    /// language, or thirty seconds of hold music.
    const LANG_PROBE_WINDOWS: usize = 3;

    /// The `LANG_PROBE_WINDOWS` most speech-dense, non-overlapping windows of
    /// `win_secs`, in recording order. Fewer if the audio is shorter than that.
    fn dense_voiced_windows(
        samples: &[f32],
        mask: &[bool],
        win_secs: f32,
        windows: usize,
    ) -> Vec<(usize, usize)> {
        let win_len = (win_secs * 16_000.0) as usize;
        if samples.len() <= win_len || win_len == 0 {
            return vec![(0, samples.len())];
        }
        let frames_per_win = (win_len / crate::VAD_FRAME).max(1);
        // Score every candidate start on a window-sized stride: cheap, and
        // non-overlapping by construction.
        let mut scored: Vec<(usize, usize)> = mask
            .chunks(frames_per_win)
            .enumerate()
            .map(|(i, frames)| (frames.iter().filter(|&&v| v).count(), i * win_len))
            .filter(|&(_, start)| start + win_len <= samples.len())
            .collect();
        scored.sort_by_key(|&(voiced, _)| std::cmp::Reverse(voiced));
        let mut picked: Vec<usize> = scored
            .into_iter()
            .take(windows)
            .map(|(_, start)| start)
            .collect();
        picked.sort_unstable();
        picked.into_iter().map(|s| (s, s + win_len)).collect()
    }

    /// Resolve `"auto"` to one concrete language for the whole recording.
    ///
    /// Left on `"auto"`, whisper.cpp re-runs detection per 30-second window, and
    /// on a long call it can drift mid-file from transcribing into *translating*;
    /// the carried decoder context then locks the new language in for everything
    /// after. Reproduced on a real two-hour Russian call: correct Russian for the
    /// first ~140 seconds, then every remaining segment arrived as English prose
    /// ("okay, now our conversation, which we have been here..."). That is what
    /// shipped into the stored transcript — the speech was never garbled, it was
    /// silently translated. Pinning one language removes the per-window vote.
    ///
    /// Returns `None` if detection fails; the caller then keeps `"auto"`, since a
    /// drifting transcript still beats no transcript.
    fn pin_language(ctx: &WhisperContext, samples: &[f32], mask: &[bool]) -> Option<String> {
        let mut votes: Vec<(String, usize)> = Vec::new();
        for (start, end) in dense_voiced_windows(samples, mask, LANG_PROBE_SECS, LANG_PROBE_WINDOWS)
        {
            let Ok(mut state) = ctx.create_state() else {
                continue;
            };
            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_language(Some("auto"));
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_special(false);
            params.set_print_timestamps(false);
            if state.full(params, &samples[start..end]).is_err() {
                continue;
            }
            let Some(lang) = whisper_rs::get_lang_str(state.full_lang_id_from_state()) else {
                continue;
            };
            match votes.iter_mut().find(|(l, _)| l == lang) {
                Some((_, n)) => *n += 1,
                None => votes.push((lang.to_string(), 1)),
            }
        }
        // Highest vote wins; ties go to whichever was seen first, i.e. earliest
        // in the recording.
        let (lang, n) = votes.into_iter().max_by_key(|&(_, n)| n)?;
        tracing::info!(language = %lang, votes = n, "pinned transcription language");
        Some(lang)
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

        // whisper-rs 0.16: segment access is via WhisperSegment objects.
        let n = state.full_n_segments();
        let mut out = Vec::with_capacity(n.max(0) as usize);
        for i in 0..n {
            let Some(seg) = state.get_segment(i) else {
                continue;
            };
            let text = seg
                .to_str_lossy()
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            // Timestamps are in centiseconds (10ms units) → seconds.
            out.push(Utterance {
                t0: seg.start_timestamp() as f32 / 100.0,
                t1: seg.end_timestamp() as f32 / 100.0,
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
            let probe_mask = mask.clone();
            let raw = tokio::task::spawn_blocking(move || {
                // Pin "auto" to a concrete language before the real pass, so the
                // decoder can't switch languages — or start translating —
                // partway through the recording.
                let language = if language.eq_ignore_ascii_case("auto") {
                    pin_language(&ctx, &samples, &probe_mask).unwrap_or_else(|| "auto".to_string())
                } else {
                    language
                };
                run_whisper(&ctx, &samples, &language)
            })
            .await
            .context("whisper task join")??;

            Ok(raw
                .into_iter()
                .filter(|u| {
                    !u.text.is_empty() && voiced_fraction(&mask, u.t0, u.t1) >= SEG_VOICED_MIN
                })
                .collect())
        }
    }
}

#[cfg(feature = "local-whisper")]
pub use local::LocalTranscriber;
