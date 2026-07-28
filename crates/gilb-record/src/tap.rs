//! Live audio tap for real-time consumers (assist / realtime STT).
//!
//! The platform capturers already accumulate every mic/system chunk into
//! [`crate::AudioBuffers`]-style storage for the on-stop files; a tap, when
//! installed, receives copies of the same chunks as they arrive. Disabled by
//! default — nothing is sent until a product calls
//! [`crate::Recorder::set_audio_tap`] before arming.
//!
//! Transport is a `tokio::sync::broadcast` ring per channel: `send` never
//! blocks, and when a consumer falls behind the ring overwrites the *oldest*
//! chunks — the consumer resumes from newer audio after a `Lagged` error. Both
//! properties serve the same invariant: the recording must never wait on, or
//! degrade because of, the suggestions pipeline.

use std::sync::Arc;

use tokio::sync::broadcast;

/// One captured chunk, as delivered by the platform callback. `sample_rate`
/// rides along because the two channels run on different clocks (mic at the
/// device rate, system audio at the capture rate) and the mic rate is only
/// known once the device is open.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    /// Mono PCM. `Arc` so fan-out to several receivers doesn't copy the data.
    pub samples: Arc<Vec<f32>>,
    pub sample_rate: u32,
}

/// Default ring capacity per channel, in chunks. Capture callbacks deliver
/// chunks every ~10–100 ms, so this holds several seconds of backlog.
const DEFAULT_CAPACITY: usize = 256;

/// The pair of chunk rings a capturer feeds. Create one, install it via
/// [`crate::Recorder::set_audio_tap`], subscribe from the consumer side.
pub struct AudioTap {
    mic: broadcast::Sender<AudioChunk>,
    system: broadcast::Sender<AudioChunk>,
}

impl Default for AudioTap {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl AudioTap {
    pub fn new(capacity: usize) -> Self {
        let (mic, _) = broadcast::channel(capacity);
        let (system, _) = broadcast::channel(capacity);
        Self { mic, system }
    }

    pub fn subscribe_mic(&self) -> broadcast::Receiver<AudioChunk> {
        self.mic.subscribe()
    }

    pub fn subscribe_system(&self) -> broadcast::Receiver<AudioChunk> {
        self.system.subscribe()
    }

    /// Called from the mic capture callback (public also for test feeds). Never
    /// blocks; a send with no live receivers is a no-op.
    pub fn send_mic(&self, samples: &[f32], sample_rate: u32) {
        let _ = self.mic.send(AudioChunk {
            samples: Arc::new(samples.to_vec()),
            sample_rate,
        });
    }

    /// Called from the system-audio capture callback. Never blocks.
    pub fn send_system(&self, samples: &[f32], sample_rate: u32) {
        let _ = self.system.send(AudioChunk {
            samples: Arc::new(samples.to_vec()),
            sample_rate,
        });
    }
}
