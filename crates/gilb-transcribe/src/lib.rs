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
//! dropped. After the merge, [`suppress_mic_echoes`] removes mic utterances that
//! are just the speakers bleeding into the microphone (the same line would
//! otherwise appear as both "Me" and "Others"). The whisper.cpp call lives
//! behind the [`Transcriber`] trait (and the `local-whisper` feature) so the
//! orchestration is unit-testable with a mock — no model, no GPU. Only
//! [`LocalTranscriber`] loads a real model.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use gilb_db::transcripts::{set_transcript_error, upsert_transcript};
use gilb_db::Db;
use tracing::{info, warn};

/// Model label stored in `meeting_transcripts.model`.
pub mod shared_model;
pub use shared_model::{SharedModel, DEFAULT_IDLE_UNLOAD};

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
///
/// Two different questions, deliberately answered from two different
/// statistics:
///
/// * *Is there any speech in here at all?* — asked of the *loudest* frame.
///   Asking a percentile silences sparse recordings: someone who says one
///   sentence into a two-minute recording leaves 99% of frames silent, so the
///   95th percentile is silence too, and the whole file gets thrown away
///   before Whisper sees it. That is a real recording lost, not a hallucination
///   avoided — and it happened (a 2-minute meeting whose 1.1 s of speech was
///   0.89% of the frames transcribed to nothing).
/// * *Which frames are speech?* — asked of the percentile spread, which is
///   what makes the threshold adapt to the room rather than to a door slam.
///
/// A lone impulse in an otherwise silent file therefore gets past the first
/// question and is caught by the second: it clears the threshold for a frame
/// or two, and [`MIN_VOICED_SECS`] then declines the channel anyway.
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
    let loudest = *sorted.last().expect("rms is non-empty");
    if loudest < ABS_SILENCE {
        return vec![false; rms.len()];
    }
    let floor = sorted[sorted.len() * 10 / 100];
    let spread = sorted[sorted.len() * 95 / 100];
    let thresh = (floor + 0.15 * (spread - floor)).max(0.006);
    rms.iter().map(|&e| e > thresh).collect()
}

/// Total voiced time in seconds implied by `mask`.
pub fn voiced_secs(mask: &[bool]) -> f32 {
    mask.iter().filter(|&&v| v).count() as f32 * VAD_FRAME as f32 / 16_000.0
}

/// A frame-wise voiced mask that knows its own frame geometry, so masks from
/// different detectors travel intact: the batch energy VAD uses
/// [`VAD_FRAME`]-sample frames, Silero (gilb-assist-audio) 512-sample ones.
/// Carrying the mask with the audio is what keeps VAD a single pass — the
/// realtime path hands the segmenter's decisions to the transcription filters
/// instead of re-detecting.
#[derive(Debug, Clone)]
pub struct VoicedMask {
    /// Samples per mask frame, at 16 kHz.
    pub frame_size: usize,
    pub frames: Vec<bool>,
}

impl VoicedMask {
    /// Compute with the batch energy detector ([`voiced_mask`]).
    pub fn energy(samples: &[f32]) -> Self {
        Self {
            frame_size: VAD_FRAME,
            frames: voiced_mask(samples),
        }
    }

    /// Total voiced seconds.
    pub fn secs(&self) -> f32 {
        self.frames.iter().filter(|&&v| v).count() as f32 * self.frame_size as f32 / 16_000.0
    }

    /// Fraction of frames overlapping `[t0, t1]` (seconds) that are voiced.
    pub fn fraction(&self, t0: f32, t1: f32) -> f32 {
        let secs_per_frame = self.frame_size as f32 / 16_000.0;
        let lo = ((t0 / secs_per_frame).floor() as usize).min(self.frames.len());
        let hi = ((t1 / secs_per_frame).ceil() as usize).min(self.frames.len());
        if hi <= lo {
            return 0.0;
        }
        let voiced = self.frames[lo..hi].iter().filter(|&&v| v).count();
        voiced as f32 / (hi - lo) as f32
    }
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

/// The `windows` most speech-dense, non-overlapping windows of `win_secs`, in
/// recording order, as `(start, end)` sample offsets. Fewer if the audio is
/// shorter than that.
///
/// Sample offsets follow the mask's own frame size — the batch energy VAD
/// emits [`VAD_FRAME`]-sample frames, the realtime Silero path 512-sample
/// ones. Assuming one for the other lands the probe windows at a fraction of
/// the intended offset (a 320-assumption over a 512-mask reads at 0.625× of
/// the real position).
///
/// Lives at the crate root (not next to its only caller, the feature-gated
/// whisper module) so the geometry is testable without a whisper build.
#[cfg(any(feature = "local-whisper", test))]
fn dense_voiced_windows(
    samples: &[f32],
    mask: &VoicedMask,
    win_secs: f32,
    windows: usize,
) -> Vec<(usize, usize)> {
    let win_len = (win_secs * 16_000.0) as usize;
    if samples.len() <= win_len || win_len == 0 {
        return vec![(0, samples.len())];
    }
    let frames_per_win = (win_len / mask.frame_size).max(1);
    let stride = frames_per_win * mask.frame_size;
    // Score every candidate start on a window-sized stride: cheap, and
    // non-overlapping by construction.
    let mut scored: Vec<(usize, usize)> = mask
        .frames
        .chunks(frames_per_win)
        .enumerate()
        .map(|(i, frames)| (frames.iter().filter(|&&v| v).count(), i * stride))
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

// ---------------------------------------------------------------------------
// Acoustic-echo suppression at the transcript level.
//
// The two channels are not acoustically isolated: without headphones the mic
// also hears the speakers, so a remote participant's sentence is transcribed on
// *both* channels and shows up twice — once as "Me", once as "Others". Observed
// on a real recording (single remote line duplicated across channels within a
// second). The suppression below mirrors the approach OpenOats ships in its
// `AcousticEchoFilter` (MIT): treat a mic utterance as an echo when it starts
// shortly *after* a system utterance that says (almost) the same thing, and
// drop the mic copy. Always the mic copy: the system tap physically cannot
// contain the local voice, so a cross-channel text match identifies the mic
// side as the bleed. Matching on text (not energy — measured unreliable here:
// the near-field mic wins every loudness comparison) means genuinely
// simultaneous speech in different words is never dropped.
// ---------------------------------------------------------------------------

/// A mic utterance counts as an echo when it starts within this window of the
/// matching system utterance, in either direction. Physically the echo always
/// trails the tap, but whisper segments each channel independently (boundary
/// jitter well over the acoustic delay) and the channels' clocks drift
/// (~2s/hour measured), so strict "mic after system" ordering misses real
/// echoes — OpenOats ships this same both-directions mode for its filter.
const ECHO_WINDOW_SECS: f32 = 1.75;
/// Word-set Jaccard similarity at or above which two texts are "the same".
const ECHO_SIMILARITY_MIN: f64 = 0.78;
/// Texts shorter than this many words *and* this many characters are too short
/// to match reliably ("да", "угу" recur constantly on both channels).
const ECHO_MIN_WORDS: usize = 4;
const ECHO_MIN_CHARS: usize = 20;

/// Lowercased alphanumeric words of `text`; everything else is a separator.
fn normalized_words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether `mic_text` reads as an echo of `sys_text`: both long enough to be
/// distinctive, and either near-identical word sets or one normalized text
/// containing the other (whisper often segments the echo differently, so one
/// side may carry a strict subset of the other's words).
fn is_echo_text(mic_text: &str, sys_text: &str) -> bool {
    let (mic_words, sys_words) = (normalized_words(mic_text), normalized_words(sys_text));
    // Characters, not bytes: byte length would halve the effective threshold
    // for Cyrillic and make short lines eligible that the filter is meant to
    // ignore. Counts the normalized text's length, separators included.
    let eligible = |words: &[String]| {
        words.len() >= ECHO_MIN_WORDS
            || words.iter().map(|w| w.chars().count()).sum::<usize>()
                + words.len().saturating_sub(1)
                >= ECHO_MIN_CHARS
    };
    if !eligible(&mic_words) || !eligible(&sys_words) {
        return false;
    }

    let mic_set: std::collections::HashSet<&str> = mic_words.iter().map(String::as_str).collect();
    let sys_set: std::collections::HashSet<&str> = sys_words.iter().map(String::as_str).collect();
    let intersection = mic_set.intersection(&sys_set).count();
    let union = mic_set.union(&sys_set).count();
    let jaccard = if union == 0 {
        1.0
    } else {
        intersection as f64 / union as f64
    };

    let (mic_norm, sys_norm) = (mic_words.join(" "), sys_words.join(" "));
    jaccard >= ECHO_SIMILARITY_MIN || mic_norm.contains(&sys_norm) || sys_norm.contains(&mic_norm)
}

/// Remove mic segments that are acoustic echoes of system segments. Segments
/// must be time-sorted. Returns the surviving segments plus how many were
/// dropped (for logging). Only mic segments are ever removed, so no content can
/// vanish — the system copy of every dropped line remains.
pub fn suppress_mic_echoes(segs: Vec<Segment>) -> (Vec<Segment>, usize) {
    let system: Vec<(f32, String)> = segs
        .iter()
        .filter(|s| s.channel == Channel::System)
        .map(|s| (s.t0, s.text.clone()))
        .collect();

    let mut dropped = 0usize;
    let kept = segs
        .into_iter()
        .filter(|seg| {
            if seg.channel != Channel::Mic {
                return true;
            }
            // Newest candidate first: the echo's source is the closest
            // system line, so search backwards from the segment.
            let is_echo = system
                .iter()
                .rev()
                .filter(|(sys_t0, _)| (seg.t0 - sys_t0).abs() <= ECHO_WINDOW_SECS)
                .any(|(_, sys_text)| is_echo_text(&seg.text, sys_text));
            if is_echo {
                dropped += 1;
            }
            !is_echo
        })
        .collect();
    (kept, dropped)
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
    let (segs, dropped) = suppress_mic_echoes(segs);
    if dropped > 0 {
        info!(dropped, "suppressed mic echoes of system audio");
    }
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

    use crate::{Transcriber, Utterance, VoicedMask, MIN_VOICED_SECS, SEG_VOICED_MIN};

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

        /// Transcribe an in-memory 16 kHz mono buffer — the realtime path:
        /// live-tap segments are already pause-bounded, so there is no file to
        /// read. Applies the same anti-hallucination filters as the file path
        /// (silent input short-circuits, mostly-unvoiced segments dropped);
        /// the model inference runs on the blocking pool.
        pub async fn transcribe_buffer(&self, samples: Vec<f32>) -> Result<Vec<Utterance>> {
            let mask = VoicedMask::energy(&samples);
            self.transcribe_buffer_masked(samples, mask).await
        }

        /// Like [`Self::transcribe_buffer`], but the caller supplies the
        /// voiced mask — the single-VAD-pass path: the realtime segmenter
        /// already decided which frames are speech (possibly with Silero),
        /// and those decisions drive the anti-hallucination filters here
        /// instead of a second, disagreeing detector.
        pub async fn transcribe_buffer_masked(
            &self,
            samples: Vec<f32>,
            mask: VoicedMask,
        ) -> Result<Vec<Utterance>> {
            if mask.secs() < MIN_VOICED_SECS {
                return Ok(vec![]); // silent buffer — skip, no hallucinations
            }

            let ctx = self.ctx.clone();
            let language = self.language.clone();
            let probe_mask = mask.clone();
            let raw = tokio::task::spawn_blocking(move || {
                // Pin "auto" to a concrete language before the real pass, so the
                // decoder can't switch languages — or start translating —
                // partway through. Costs a probe pass, and only when the caller
                // asked for auto-detection — a caller that passes a fixed
                // language pays nothing.
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
                .filter(|u| !u.text.is_empty() && mask.fraction(u.t0, u.t1) >= SEG_VOICED_MIN)
                .collect())
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
    fn pin_language(ctx: &WhisperContext, samples: &[f32], mask: &VoicedMask) -> Option<String> {
        let mut votes: Vec<(String, usize)> = Vec::new();
        for (start, end) in
            crate::dense_voiced_windows(samples, mask, LANG_PROBE_SECS, LANG_PROBE_WINDOWS)
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
        // in the recording. (Strictly-greater fold: `max_by_key` would return
        // the *last* maximum on a tie.)
        let (lang, n) =
            votes
                .into_iter()
                .fold(None::<(String, usize)>, |best, cand| match best {
                    Some(b) if b.1 >= cand.1 => Some(b),
                    _ => Some(cand),
                })?;
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
            self.transcribe_buffer(read_wav_16k_mono(audio_path)?).await
        }
    }
}

#[cfg(feature = "local-whisper")]
pub use local::LocalTranscriber;

#[cfg(test)]
mod tests {
    use super::*;

    /// Probe windows must honor the mask's frame size: a 512-sample Silero
    /// mask over the same audio must land the window inside the voiced span
    /// just like the 320-sample energy mask — not at 0.625× of the offset.
    #[test]
    fn dense_windows_follow_the_mask_frame_size() {
        const WIN_SECS: f32 = 30.0;
        let win_len = (WIN_SECS * 16_000.0) as usize;
        // Three minutes; only the second one is voiced.
        let samples = vec![0.0f32; 180 * 16_000];
        let voiced = 60 * 16_000..120 * 16_000;

        let mask = |frame_size: usize| VoicedMask {
            frame_size,
            frames: (0..samples.len() / frame_size)
                .map(|i| voiced.contains(&(i * frame_size)))
                .collect(),
        };

        for frame_size in [VAD_FRAME, 512] {
            let windows = dense_voiced_windows(&samples, &mask(frame_size), WIN_SECS, 1);
            assert_eq!(windows.len(), 1, "frame_size {frame_size}");
            let (start, end) = windows[0];
            assert_eq!(end - start, win_len, "frame_size {frame_size}");
            assert!(
                voiced.contains(&start),
                "frame_size {frame_size}: window starts at {start}, outside the voiced minute"
            );
        }
    }

    /// Audio shorter than one window is probed whole, whatever the mask says.
    #[test]
    fn dense_windows_short_audio_is_one_window() {
        let samples = vec![0.0f32; 16_000];
        let mask = VoicedMask {
            frame_size: 512,
            frames: vec![true; 50],
        };
        assert_eq!(
            dense_voiced_windows(&samples, &mask, 30.0, 3),
            vec![(0, samples.len())]
        );
    }
}
