//! Silero VAD (D12) behind [`FrameVad`]: a ~2 MB neural voice detector run
//! through onnxruntime, replacing the energy heuristic for segmentation.
//!
//! Why it earns its dependency: every false "voiced" frame can grow into a
//! segment that costs a whisper inference *while the recording is competing
//! for the same GPU*, and every missed quiet phrase is a lost turn. Energy
//! RMS fires on keyboards, notification sounds and music; Silero does not,
//! and it hears speech well below the noise floor heuristic's threshold.
//! (Adopted after studying huggingface/speech-to-speech, which uses Silero
//! v5 as its turn-taking front-end.)
//!
//! The model wants fixed 512-sample frames at 16 kHz (32 ms) — the segmenter
//! adapts its framing via [`FrameVad::required_frame_size`]. Detector state
//! (the model's recurrence) lives per instance: one per channel.

use anyhow::{Context, Result};
use voice_activity_detector::VoiceActivityDetector;

use crate::segment::FrameVad;

/// Frame Silero v5 requires at 16 kHz.
const SILERO_FRAME: usize = 512;
/// Speech probability above which a frame counts as voiced. 0.5 is Silero's
/// recommended default; the segmenter's own open/close hysteresis sits on top.
const THRESHOLD: f32 = 0.5;

pub struct SileroVad {
    inner: VoiceActivityDetector,
}

impl SileroVad {
    pub fn new() -> Result<Self> {
        let inner = VoiceActivityDetector::builder()
            .sample_rate(16_000)
            .chunk_size(SILERO_FRAME)
            .build()
            .context("load silero vad model")?;
        Ok(Self { inner })
    }
}

impl FrameVad for SileroVad {
    fn required_frame_size(&self) -> Option<usize> {
        Some(SILERO_FRAME)
    }

    fn is_voiced(&mut self, frame: &[f32]) -> bool {
        self.inner.predict(frame.iter().copied()) > THRESHOLD
    }

    /// Clear the model's recurrent state — the ONNX session stays loaded, so a
    /// meeting boundary costs nothing.
    fn reset(&mut self) {
        self.inner.reset();
    }
}
