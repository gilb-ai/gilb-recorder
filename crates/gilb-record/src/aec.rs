//! Acoustic echo cancellation over speexdsp (via the `aec-rs` crate) — the
//! single speex wrapper both consumers share: the realtime assist pipeline
//! (gilb-assist-audio, streaming) and this crate's offline finalization pass
//! ([`crate::cancel_echo`]).
//!
//! speex's MDF canceller works on fixed-size i16 frames and needs the near-end
//! (mic) and far-end (system audio) streams fed in lockstep. Capture callbacks
//! deliver chunks of arbitrary length on independent clocks, so this wrapper
//! owns the framing: both streams go through FIFOs, full frames are processed
//! as soon as the near side has one, and the far side is zero-padded on
//! underrun / trimmed on overrun so a clock drift between the two devices
//! degrades adaptation instead of growing a queue forever.

use std::collections::VecDeque;

use aec_rs::{Aec, AecConfig};
use tracing::warn;

#[derive(Debug, Clone)]
pub struct EchoCancellerConfig {
    /// Sample rate both streams arrive at. The assist pipeline resamples to
    /// 16 kHz before AEC, matching the whisper input rate.
    pub sample_rate: u32,
    /// Samples per speex frame. 320 @ 16 kHz = 20 ms, the same granularity as
    /// gilb-transcribe's `VAD_FRAME`.
    pub frame_size: usize,
    /// Echo tail the adaptive filter can model. Must cover the output latency
    /// of the speakers plus room reverb; speex re-converges within it when the
    /// device delay shifts.
    pub tail_ms: u32,
    /// Run speex's preprocessor (denoise + residual echo suppression) on the
    /// cleaned frames.
    pub enable_preprocess: bool,
}

impl Default for EchoCancellerConfig {
    fn default() -> Self {
        Self {
            sample_rate: 16_000,
            frame_size: 320,
            tail_ms: 200,
            enable_preprocess: true,
        }
    }
}

pub struct EchoCanceller {
    config: EchoCancellerConfig,
    aec: Aec,
    frame_size: usize,
    /// Far-end samples the FIFO may hold before the oldest are dropped: the
    /// filter tail (older samples can no longer explain any echo) plus slack
    /// for delivery jitter.
    max_far_backlog: usize,
    near: VecDeque<i16>,
    far: VecDeque<i16>,
    near_frame: Vec<i16>,
    far_frame: Vec<i16>,
    out_frame: Vec<i16>,
    /// Reference samples discarded because the far side ran too far ahead of
    /// the near side. Counted, not ignored: sustained drops mean the two
    /// capture paths are skewed by more than the filter tail, and cancellation
    /// quietly stops working — the one failure here that looks like nothing.
    dropped_far: usize,
}

// The speex states are plain heap-allocated C structs with no thread affinity;
// aec-rs just holds raw pointers to them, which is why Send is not derived.
// Moving the canceller into the worker task is safe as long as calls stay
// exclusive, which `&mut self` enforces.
unsafe impl Send for EchoCanceller {}

impl EchoCanceller {
    pub fn new(config: &EchoCancellerConfig) -> Self {
        let filter_length = (config.sample_rate * config.tail_ms / 1000) as i32;
        let aec = Aec::new(&AecConfig {
            frame_size: config.frame_size,
            filter_length,
            sample_rate: config.sample_rate,
            enable_preprocess: config.enable_preprocess,
        });
        Self {
            config: config.clone(),
            aec,
            frame_size: config.frame_size,
            max_far_backlog: filter_length as usize + config.sample_rate as usize / 2,
            near: VecDeque::new(),
            far: VecDeque::new(),
            near_frame: vec![0; config.frame_size],
            far_frame: vec![0; config.frame_size],
            out_frame: vec![0; config.frame_size],
            dropped_far: 0,
        }
    }

    /// Throw away the adaptive filter and both FIFOs. speexdsp exposes no
    /// reset through `aec-rs`, so the state is rebuilt — cheap (a filter
    /// allocation, no model). Called at a meeting boundary: a filter converged
    /// on the previous room and device pair is worse than an empty one.
    pub fn reset(&mut self) {
        *self = Self::new(&self.config.clone());
    }

    /// Feed far-end samples: the system audio that the speakers are playing.
    pub fn push_far(&mut self, samples: &[f32]) {
        self.far.extend(samples.iter().map(|&s| to_i16(s)));
        if self.far.len() > self.max_far_backlog {
            let excess = self.far.len() - self.max_far_backlog;
            self.far.drain(..excess);
            self.dropped_far += excess;
            // One line per second of lost reference, no more.
            if self.dropped_far >= self.config.sample_rate as usize {
                warn!(
                    dropped_secs = self.dropped_far as f32 / self.config.sample_rate as f32,
                    "echo canceller: far channel runs ahead of near; cancellation degraded"
                );
                self.dropped_far = 0;
            }
        }
    }

    /// Feed near-end (mic) samples; returns the cleaned samples. Whole frames
    /// are processed immediately, the remainder stays buffered until the next
    /// call, so output length trails input by less than one frame.
    pub fn push_near(&mut self, samples: &[f32]) -> Vec<f32> {
        self.near.extend(samples.iter().map(|&s| to_i16(s)));
        let mut out = Vec::with_capacity(self.near.len() - self.near.len() % self.frame_size);
        while self.near.len() >= self.frame_size {
            for slot in self.near_frame.iter_mut() {
                *slot = self.near.pop_front().unwrap();
            }
            // Far underrun (nothing playing yet, or delivery jitter) is
            // padded with silence: cancelling against silence is a no-op.
            for slot in self.far_frame.iter_mut() {
                *slot = self.far.pop_front().unwrap_or(0);
            }
            self.aec
                .cancel_echo(&self.near_frame, &self.far_frame, &mut self.out_frame);
            out.extend(self.out_frame.iter().map(|&s| from_i16(s)));
        }
        out
    }
}

fn to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0) as i16
}

fn from_i16(s: i16) -> f32 {
    f32::from(s) / 32768.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: usize = 16_000;
    const FRAME: usize = 320;

    /// Deterministic white noise in [-0.5, 0.5].
    fn noise(len: usize, mut seed: u64) -> Vec<f32> {
        (0..len)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5
            })
            .collect()
    }

    fn energy(samples: &[f32]) -> f64 {
        samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum()
    }

    fn config(enable_preprocess: bool) -> EchoCancellerConfig {
        EchoCancellerConfig {
            sample_rate: SR as u32,
            frame_size: FRAME,
            tail_ms: 200,
            enable_preprocess,
        }
    }

    /// Echo-only mic signal: after convergence the canceller must attenuate it
    /// by well over 10 dB.
    #[test]
    fn cancels_pure_echo() {
        let secs = 5;
        let delay = SR * 40 / 1000; // 40 ms speaker-to-mic path
        let far = noise(SR * secs, 7);
        let near: Vec<f32> = (0..far.len())
            .map(|n| {
                if n >= delay {
                    far[n - delay] * 0.6
                } else {
                    0.0
                }
            })
            .collect();

        let mut aec = EchoCanceller::new(&config(false));
        let mut out = Vec::with_capacity(near.len());
        for (far_chunk, near_chunk) in far.chunks(FRAME).zip(near.chunks(FRAME)) {
            aec.push_far(far_chunk);
            out.extend(aec.push_near(near_chunk));
        }

        // Judge only the last second, after the filter has adapted.
        let tail = SR;
        let erle =
            10.0 * (energy(&near[near.len() - tail..]) / energy(&out[out.len() - tail..])).log10();
        assert!(
            erle > 10.0,
            "expected > 10 dB echo attenuation, got {erle:.1} dB"
        );
    }

    /// With a silent far end the canceller must pass the mic through intact.
    #[test]
    fn passes_near_through_when_far_is_silent() {
        let near = noise(SR, 42);
        let mut aec = EchoCanceller::new(&config(false));
        let out = aec.push_near(&near);

        assert_eq!(out.len(), near.len());
        let ratio = energy(&out) / energy(&near);
        assert!(
            (0.9..=1.1).contains(&ratio),
            "expected pass-through, energy ratio {ratio:.3}"
        );
    }

    /// Input that doesn't align to frame boundaries stays buffered, not lost.
    #[test]
    fn buffers_partial_frames() {
        let mut aec = EchoCanceller::new(&config(false));
        let out = aec.push_near(&vec![0.1; 500]);
        assert_eq!(out.len(), FRAME);
        let out = aec.push_near(&vec![0.1; 140]);
        assert_eq!(out.len(), FRAME);
    }

    /// The preprocessor path (denoise + residual echo suppression) must at
    /// least run and produce finite output.
    #[test]
    fn preprocess_smoke() {
        let far = noise(SR, 7);
        let near: Vec<f32> = far.iter().map(|&s| s * 0.5).collect();
        let mut aec = EchoCanceller::new(&config(true));
        aec.push_far(&far);
        let out = aec.push_near(&near);
        assert_eq!(out.len(), near.len());
        assert!(out.iter().all(|s| s.is_finite()));
    }
}
