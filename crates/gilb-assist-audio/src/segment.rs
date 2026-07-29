//! Pause-bounded segmentation (§6.2 of REALTIME_ASSIST.md): accumulate speech,
//! close the segment when a pause exceeds the threshold, force-close with
//! overlap when someone talks without pausing. One instance per channel.
//!
//! gilb-transcribe's `voiced_mask` is batch-adaptive (its threshold comes from
//! the RMS distribution of the whole buffer) and cannot run frame-by-frame, so
//! the streaming path keeps its own energy VAD: a running noise-floor estimate
//! (fast to fall, slow to rise) with the same 20 ms frame and the same absolute
//! floor. The anti-hallucination filter carries over: segments with too little
//! voiced time are dropped, never sent to whisper.

use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct SegmenterConfig {
    /// Stream rate; the pipeline feeds 16 kHz.
    pub sample_rate: u32,
    /// VAD frame, samples. 320 @ 16 kHz = 20 ms — gilb-transcribe's VAD_FRAME.
    pub frame_size: usize,
    /// Consecutive voiced frames that open a segment (debounces clicks).
    pub open_frames: usize,
    /// Silence that closes a segment — the phrase boundary.
    pub close_pause_ms: u32,
    /// Force-close threshold for pauseless speech; without it a monologue
    /// would never produce a suggestion.
    pub max_segment_secs: f32,
    /// Segments with less voiced time than this are dropped (whisper
    /// hallucinates on near-silence) — gilb-transcribe's MIN_VOICED_SECS.
    pub min_voiced_secs: f32,
    /// Audio kept from before the opening frame so the first word isn't clipped.
    pub pre_roll_ms: u32,
    /// Tail carried into the next segment on a forced close, so the cut does
    /// not split a word.
    pub overlap_ms: u32,
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        Self {
            sample_rate: 16_000,
            frame_size: 320,
            open_frames: 2,
            close_pause_ms: 700,
            max_segment_secs: 15.0,
            min_voiced_secs: 0.3,
            pre_roll_ms: 200,
            overlap_ms: 250,
        }
    }
}

/// A closed, filtered segment ready for the whisper queue.
#[derive(Debug, Clone)]
pub struct Segment {
    pub samples: Vec<f32>,
    /// Offset of the first sample from the start of the stream, seconds.
    pub start_secs: f64,
    pub end_secs: f64,
    /// The detector's per-frame decisions over `samples` (frame =
    /// `vad_frame_size`). Travels with the segment so downstream filters
    /// reuse them — VAD runs exactly once per sample.
    pub voiced: Vec<bool>,
    pub vad_frame_size: usize,
}

enum State {
    Idle {
        /// Recent audio ring so an opening segment starts pre_roll_ms early.
        pre_roll: VecDeque<f32>,
        /// Per-frame flags matching `pre_roll` (one per full frame in it).
        pre_roll_flags: VecDeque<bool>,
        consecutive_voiced: usize,
    },
    Active {
        buf: Vec<f32>,
        /// Per-frame flags matching `buf`.
        flags: Vec<bool>,
        /// Stream position of buf[0], in samples.
        start_sample: u64,
        voiced_frames: usize,
        silence_frames: usize,
    },
}

pub struct Segmenter {
    cfg: SegmenterConfig,
    /// The detector's frame; equals `cfg.frame_size` unless the detector
    /// requires its own (Silero wants 512 samples).
    frame_size: usize,
    close_frames: usize,
    max_frames: usize,
    min_voiced_frames: usize,
    pre_roll_samples: usize,
    overlap_samples: usize,
    state: State,
    /// Partial VAD frame awaiting completion.
    fifo: Vec<f32>,
    /// Stream position, in samples, of the *end* of the last consumed frame.
    consumed: u64,
    vad: Box<dyn FrameVad>,
}

/// Frame-wise voice decision, pluggable per detector (D12): the energy
/// detector below is the dependency-free default, Silero (feature `silero`)
/// the robust one. The segmenter owns everything else — hysteresis, pauses,
/// limits — so a detector only answers "is this frame speech?".
pub trait FrameVad: Send {
    /// Frame length the detector requires, if any; `None` = the config's.
    fn required_frame_size(&self) -> Option<usize> {
        None
    }
    fn is_voiced(&mut self, frame: &[f32]) -> bool;
}

/// Absolute silence floor, matching gilb-transcribe's threshold clamp.
const ABS_FLOOR: f32 = 0.006;
/// Noise floor recovery per frame (~5% per second at 20 ms frames).
const FLOOR_RISE: f32 = 1.001;
/// A frame is voiced when its RMS clears the floor by this factor.
const SNR_FACTOR: f32 = 3.0;

/// RMS energy against a running noise floor: fast to fall, slow to rise, so a
/// long burst of speech does not become the new "silence". Cheap and
/// dependency-free, but anything loud (keyboard, music) reads as speech.
pub struct EnergyVad {
    noise_floor: f32,
}

impl Default for EnergyVad {
    fn default() -> Self {
        Self { noise_floor: ABS_FLOOR }
    }
}

impl FrameVad for EnergyVad {
    fn is_voiced(&mut self, frame: &[f32]) -> bool {
        let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt();
        if rms < self.noise_floor {
            self.noise_floor = rms.max(ABS_FLOOR * 0.1);
        } else {
            self.noise_floor *= FLOOR_RISE;
        }
        rms > (self.noise_floor * SNR_FACTOR).max(ABS_FLOOR)
    }
}

impl Segmenter {
    pub fn new(cfg: SegmenterConfig) -> Self {
        Self::with_vad(cfg, Box::new(EnergyVad::default()))
    }

    /// Build with an explicit detector; its required frame size (if any)
    /// overrides the config's.
    pub fn with_vad(cfg: SegmenterConfig, vad: Box<dyn FrameVad>) -> Self {
        let frame_size = vad.required_frame_size().unwrap_or(cfg.frame_size);
        let frame_ms = frame_size as f32 * 1000.0 / cfg.sample_rate as f32;
        let close_frames = (cfg.close_pause_ms as f32 / frame_ms).ceil() as usize;
        let max_frames = (cfg.max_segment_secs * 1000.0 / frame_ms).ceil() as usize;
        let min_voiced_frames = (cfg.min_voiced_secs * 1000.0 / frame_ms).ceil() as usize;
        let pre_roll_samples =
            (cfg.pre_roll_ms as usize * cfg.sample_rate as usize / 1000) / frame_size
                * frame_size;
        // Rounded to whole frames: the carried tail must keep its per-frame
        // flags aligned with its samples.
        let overlap_samples =
            cfg.overlap_ms as usize * cfg.sample_rate as usize / 1000 / frame_size * frame_size;
        Self {
            frame_size,
            close_frames,
            max_frames,
            min_voiced_frames,
            pre_roll_samples,
            overlap_samples,
            state: State::idle(),
            fifo: Vec::with_capacity(frame_size),
            consumed: 0,
            vad,
            cfg,
        }
    }

    /// Feed a chunk of any length; returns the segments it closed.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Segment> {
        let mut closed = Vec::new();
        let mut rest = samples;
        while !rest.is_empty() {
            let need = self.frame_size - self.fifo.len();
            let take = need.min(rest.len());
            self.fifo.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.fifo.len() == self.frame_size {
                let frame = std::mem::replace(
                    &mut self.fifo,
                    Vec::with_capacity(self.frame_size),
                );
                self.consume_frame(frame, &mut closed);
            }
        }
        closed
    }

    /// Close whatever is active — end of the meeting.
    pub fn flush(&mut self) -> Option<Segment> {
        let state = std::mem::replace(&mut self.state, State::idle());
        match state {
            State::Idle { .. } => None,
            State::Active { buf, flags, start_sample, voiced_frames, .. } => {
                self.finish(buf, flags, start_sample, voiced_frames)
            }
        }
    }

    fn consume_frame(&mut self, frame: Vec<f32>, closed: &mut Vec<Segment>) {
        let voiced = self.vad.is_voiced(&frame);
        self.consumed += frame.len() as u64;

        match &mut self.state {
            State::Idle { pre_roll, pre_roll_flags, consecutive_voiced } => {
                pre_roll.extend(frame.iter());
                pre_roll_flags.push_back(voiced);
                while pre_roll.len() > self.pre_roll_samples + self.frame_size {
                    pre_roll.pop_front();
                }
                while pre_roll_flags.len() * self.frame_size > pre_roll.len() {
                    pre_roll_flags.pop_front();
                }
                *consecutive_voiced = if voiced { *consecutive_voiced + 1 } else { 0 };
                if *consecutive_voiced >= self.cfg.open_frames {
                    let buf: Vec<f32> = pre_roll.iter().copied().collect();
                    let flags: Vec<bool> = pre_roll_flags.iter().copied().collect();
                    let voiced_frames = *consecutive_voiced;
                    let start_sample = self.consumed - buf.len() as u64;
                    self.state = State::Active {
                        buf,
                        flags,
                        start_sample,
                        voiced_frames,
                        silence_frames: 0,
                    };
                }
            }
            State::Active { buf, flags, start_sample, voiced_frames, silence_frames } => {
                buf.extend_from_slice(&frame);
                flags.push(voiced);
                if voiced {
                    *voiced_frames += 1;
                    *silence_frames = 0;
                } else {
                    *silence_frames += 1;
                }

                if *silence_frames >= self.close_frames {
                    let (buf, flags, start, vf) = (
                        std::mem::take(buf),
                        std::mem::take(flags),
                        *start_sample,
                        *voiced_frames,
                    );
                    self.state = State::idle();
                    closed.extend(self.finish(buf, flags, start, vf));
                } else if buf.len() >= self.max_frames * self.frame_size {
                    // Pauseless speech: emit, carry the tail so the forced cut
                    // does not lose the word it landed on.
                    let (full, full_flags, start, vf) = (
                        std::mem::take(buf),
                        std::mem::take(flags),
                        *start_sample,
                        *voiced_frames,
                    );
                    let overlap_start = full.len().saturating_sub(self.overlap_samples);
                    let carry = full[overlap_start..].to_vec();
                    let carry_flags = full_flags[overlap_start / self.frame_size..].to_vec();
                    self.state = State::Active {
                        start_sample: start + overlap_start as u64,
                        voiced_frames: carry.len() / self.frame_size,
                        silence_frames: 0,
                        buf: carry,
                        flags: carry_flags,
                    };
                    closed.extend(self.finish(full, full_flags, start, vf));
                }
            }
        }
    }

    /// Apply the anti-hallucination filter and stamp times.
    fn finish(
        &self,
        buf: Vec<f32>,
        flags: Vec<bool>,
        start_sample: u64,
        voiced_frames: usize,
    ) -> Option<Segment> {
        if voiced_frames < self.min_voiced_frames {
            return None;
        }
        let sr = f64::from(self.cfg.sample_rate);
        Some(Segment {
            start_secs: start_sample as f64 / sr,
            end_secs: (start_sample + buf.len() as u64) as f64 / sr,
            samples: buf,
            voiced: flags,
            vad_frame_size: self.frame_size,
        })
    }
}

impl State {
    fn idle() -> Self {
        State::Idle {
            pre_roll: VecDeque::new(),
            pre_roll_flags: VecDeque::new(),
            consecutive_voiced: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: usize = 16_000;

    /// Deterministic white noise in [-amp, amp].
    fn noise(len: usize, amp: f32, mut seed: u64) -> Vec<f32> {
        (0..len)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (((seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5) * 2.0 * amp
            })
            .collect()
    }

    fn silence(len: usize) -> Vec<f32> {
        noise(len, 0.001, 99)
    }

    fn feed(seg: &mut Segmenter, stream: &[f32]) -> Vec<Segment> {
        let mut out = Vec::new();
        for chunk in stream.chunks(333) {
            out.extend(seg.push(chunk));
        }
        out
    }

    /// Two speech bursts separated by a pause become two segments with sane
    /// boundaries.
    #[test]
    fn splits_on_pause() {
        let mut stream = silence(SR);
        stream.extend(noise(SR * 2, 0.2, 1)); // speech at 1.0–3.0 s
        stream.extend(silence(SR));
        stream.extend(noise(SR * 3 / 2, 0.2, 2)); // speech at 4.0–5.5 s
        stream.extend(silence(SR));

        let mut seg = Segmenter::new(SegmenterConfig::default());
        let got = feed(&mut seg, &stream);

        assert_eq!(got.len(), 2, "expected 2 segments, got {}", got.len());
        assert!((got[0].start_secs - 1.0).abs() < 0.3, "start {}", got[0].start_secs);
        assert!((got[0].end_secs - 3.0).abs() < 1.0, "end {}", got[0].end_secs);
        assert!((got[1].start_secs - 4.0).abs() < 0.3, "start {}", got[1].start_secs);
    }

    /// Pauseless speech is force-split with overlapping boundaries.
    #[test]
    fn force_splits_long_speech() {
        let mut stream = silence(SR / 2);
        stream.extend(noise(SR * 40, 0.2, 3));

        let mut seg = Segmenter::new(SegmenterConfig::default());
        let mut got = feed(&mut seg, &stream);
        got.extend(seg.flush());

        assert!(got.len() >= 3, "expected >= 3 segments, got {}", got.len());
        for s in &got {
            assert!(s.end_secs - s.start_secs <= 15.5, "segment too long");
        }
        for pair in got.windows(2) {
            assert!(
                pair[1].start_secs < pair[0].end_secs,
                "forced split lost the overlap: {} !< {}",
                pair[1].start_secs,
                pair[0].end_secs
            );
        }
    }

    /// Silence produces nothing at all.
    #[test]
    fn silence_yields_no_segments() {
        let mut seg = Segmenter::new(SegmenterConfig::default());
        let got = feed(&mut seg, &silence(SR * 10));
        assert!(got.is_empty());
        assert!(seg.flush().is_none());
    }

    /// A blip shorter than min_voiced_secs is dropped, not sent to whisper.
    #[test]
    fn drops_short_blip() {
        let mut stream = silence(SR);
        stream.extend(noise(SR / 10, 0.2, 4)); // 100 ms
        stream.extend(silence(SR * 2));

        let mut seg = Segmenter::new(SegmenterConfig::default());
        let got = feed(&mut seg, &stream);
        assert!(got.is_empty(), "100 ms blip must be filtered out");
    }

    /// flush() closes the segment that was still accumulating.
    #[test]
    fn flush_closes_active_segment() {
        let mut stream = silence(SR / 2);
        stream.extend(noise(SR, 0.2, 5));

        let mut seg = Segmenter::new(SegmenterConfig::default());
        let mut got = feed(&mut seg, &stream);
        got.extend(seg.flush());
        assert_eq!(got.len(), 1);
        assert!(got[0].end_secs - got[0].start_secs >= 1.0);
    }
}
