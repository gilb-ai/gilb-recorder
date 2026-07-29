//! The assembled realtime front-end (§4.3 of REALTIME_ASSIST.md):
//!
//! ```text
//! AudioTap (gilb-record) ─ mic ──► resample 16k ─► AEC (near) ─► Segmenter ─┐
//!                        └ system► resample 16k ─► AEC (far)  ─► Segmenter ─┤
//!                                                                           ▼
//!                                            STT worker ─► gilb-assist Turns
//! ```
//!
//! One task owns both channels so the echo canceller sees near and far in
//! arrival order; a second task forwards recognized utterances to the assist
//! engine. Everything is best-effort: a lagged tap (the ring overwrote old
//! chunks while we were busy) is logged and skipped — the recording itself is
//! never involved.

use gilb_assist::{AssistHandle, Speaker, Turn};
use gilb_record::{AudioChunk, AudioTap};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::{
    spawn_stt_worker, EchoCanceller, EchoCancellerConfig, SegmentTranscriber, Segmenter,
    SegmenterConfig, SttChannel, SttWorkerConfig, SttWorkerHandle,
};

/// Pipeline sample rate — whisper's input rate, and the rate the segmenter
/// and echo canceller run at.
const PIPELINE_RATE: u32 = 16_000;

#[derive(Debug, Clone, Default)]
pub struct AssistPipelineConfig {
    pub aec: EchoCancellerConfig,
    pub segmenter: SegmenterConfig,
    pub worker: SttWorkerConfig,
}

/// Everything the running pipeline owns; dropping it aborts the tasks.
pub struct AssistPipeline {
    audio_task: JoinHandle<()>,
    turns_task: JoinHandle<()>,
    /// Exposed so the app can inject synthetic segments in diagnostics.
    pub worker: SttWorkerHandle,
}

impl Drop for AssistPipeline {
    fn drop(&mut self) {
        self.audio_task.abort();
        self.turns_task.abort();
    }
}

/// Wire the tap through resample → AEC → segmentation → STT into the assist
/// engine. Chunks flow once the recorder starts feeding the tap; the pipeline
/// survives across meetings (the model unloads itself when idle).
pub fn spawn_assist_pipeline<T: SegmentTranscriber>(
    tap: &AudioTap,
    transcriber: T,
    assist: AssistHandle,
    config: AssistPipelineConfig,
) -> AssistPipeline {
    let (worker, mut utterances) = spawn_stt_worker(transcriber, config.worker);

    let mic_rx = tap.subscribe_mic();
    let sys_rx = tap.subscribe_system();
    let audio_task = tokio::spawn(audio_loop(
        mic_rx,
        sys_rx,
        config.aec,
        config.segmenter,
        worker.clone(),
    ));

    let turns_task = tokio::spawn(async move {
        while let Some(u) = utterances.recv().await {
            let speaker = match u.channel {
                SttChannel::Mic => Speaker::Me,
                SttChannel::System => Speaker::Them,
            };
            assist.push_turn(Turn {
                speaker,
                text: u.text,
                at_secs: u.start_secs as f32,
            });
        }
    });

    AssistPipeline {
        audio_task,
        turns_task,
        worker,
    }
}

/// Segmenter with the best available voice detector: Silero when the feature
/// is compiled in and its model loads, the energy heuristic otherwise (D12).
fn build_segmenter(cfg: SegmenterConfig) -> Segmenter {
    #[cfg(feature = "silero")]
    match crate::SileroVad::new() {
        Ok(vad) => return Segmenter::with_vad(cfg, Box::new(vad)),
        Err(e) => warn!(error = %e, "silero vad unavailable; using the energy vad"),
    }
    Segmenter::new(cfg)
}

/// A channel's resampler, recreated if the tap's reported rate changes
/// (e.g. the user switches microphones mid-meeting).
struct ChannelResampler {
    rate: u32,
    inner: crate::StreamResampler,
}

impl ChannelResampler {
    fn push(&mut self, chunk: &AudioChunk) -> Vec<f32> {
        if chunk.sample_rate != self.rate {
            info!(from = self.rate, to = chunk.sample_rate, "audio device rate changed");
            match crate::StreamResampler::new(chunk.sample_rate, PIPELINE_RATE) {
                Ok(fresh) => {
                    self.inner = fresh;
                    self.rate = chunk.sample_rate;
                }
                Err(e) => {
                    warn!(error = %e, "failed to rebuild resampler; dropping chunk");
                    return Vec::new();
                }
            }
        }
        match self.inner.push(&chunk.samples) {
            Ok(out) => out,
            Err(e) => {
                warn!(error = %e, "resample failed; dropping chunk");
                Vec::new()
            }
        }
    }
}

fn channel_resampler(initial_rate: u32) -> Option<ChannelResampler> {
    match crate::StreamResampler::new(initial_rate, PIPELINE_RATE) {
        Ok(inner) => Some(ChannelResampler { rate: initial_rate, inner }),
        Err(e) => {
            warn!(error = %e, "failed to build resampler");
            None
        }
    }
}

async fn audio_loop(
    mut mic_rx: tokio::sync::broadcast::Receiver<AudioChunk>,
    mut sys_rx: tokio::sync::broadcast::Receiver<AudioChunk>,
    aec_config: EchoCancellerConfig,
    seg_config: SegmenterConfig,
    worker: SttWorkerHandle,
) {
    let mut aec = EchoCanceller::new(&aec_config);
    let mut seg_mic = build_segmenter(seg_config.clone());
    let mut seg_sys = build_segmenter(seg_config);
    let mut mic_rs: Option<ChannelResampler> = None;
    let mut sys_rs: Option<ChannelResampler> = None;

    loop {
        tokio::select! {
            chunk = mic_rx.recv() => match chunk {
                Ok(chunk) => {
                    let rs = match &mut mic_rs {
                        Some(rs) => rs,
                        None => match channel_resampler(chunk.sample_rate) {
                            Some(rs) => mic_rs.insert(rs),
                            None => continue,
                        },
                    };
                    let at_16k = rs.push(&chunk);
                    let clean = aec.push_near(&at_16k);
                    for segment in seg_mic.push(&clean) {
                        worker.push(SttChannel::Mic, segment);
                    }
                }
                Err(RecvError::Lagged(n)) => warn!(skipped = n, "mic tap lagged"),
                Err(RecvError::Closed) => break,
            },
            chunk = sys_rx.recv() => match chunk {
                Ok(chunk) => {
                    let rs = match &mut sys_rs {
                        Some(rs) => rs,
                        None => match channel_resampler(chunk.sample_rate) {
                            Some(rs) => sys_rs.insert(rs),
                            None => continue,
                        },
                    };
                    let at_16k = rs.push(&chunk);
                    aec.push_far(&at_16k);
                    for segment in seg_sys.push(&at_16k) {
                        worker.push(SttChannel::System, segment);
                    }
                }
                Err(RecvError::Lagged(n)) => warn!(skipped = n, "system tap lagged"),
                Err(RecvError::Closed) => break,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use gilb_assist::echo::{EchoBackend, StaticConfig};
    use gilb_assist::{AssistEvent, EngineParams};
    use std::time::Duration;

    /// "STT" that reports the segment's duration — enough to prove segments
    /// flow end-to-end without a model.
    struct StubStt;

    #[async_trait]
    impl SegmentTranscriber for StubStt {
        async fn transcribe(&mut self, segment: crate::Segment) -> Result<String> {
            Ok(format!("[{} ms]", segment.samples.len() * 1000 / 16_000))
        }
    }

    fn noise(len: usize, amp: f32, mut seed: u64) -> Vec<f32> {
        (0..len)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (((seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5) * 2.0 * amp
            })
            .collect()
    }

    /// Real conversational speech (16 kHz, from the silero-vad test corpus) —
    /// works under both the energy and the Silero detector; white noise would
    /// not survive Silero, correctly.
    const SPEECH: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/speech_16k.wav"));

    fn speech_samples() -> Vec<f32> {
        let mut reader = hound::WavReader::new(std::io::Cursor::new(SPEECH)).unwrap();
        assert_eq!(reader.spec().sample_rate, 16_000);
        reader
            .samples::<i16>()
            .map(|s| f32::from(s.unwrap()) / 32768.0)
            .collect()
    }

    /// Tap → resample → AEC → segmentation → STT stub → assist engine with the
    /// echo backend: speech on the system channel must come out as a `them:`
    /// suggestion.
    #[tokio::test]
    async fn tap_to_suggestion_end_to_end() {
        let tap = AudioTap::new(1024);
        let (assist, mut events) = gilb_assist::spawn(
            StaticConfig::default(),
            EchoBackend,
            EngineParams { min_analysis_interval: Duration::from_millis(1) },
        );
        let _pipeline = spawn_assist_pipeline(
            &tap,
            StubStt,
            assist,
            AssistPipelineConfig::default(),
        );
        tokio::task::yield_now().await; // let the tasks subscribe

        // Real speech, then silence long enough to close the last segment.
        let mut stream = speech_samples();
        stream.extend(noise(24_000, 0.001, 11));
        for chunk in stream.chunks(1_600) {
            tap.send_system(chunk, 16_000);
            tokio::task::yield_now().await;
        }

        let event = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await.expect("assist engine closed") {
                    AssistEvent::Update(text) => break text,
                    _ => continue,
                }
            }
        })
        .await
        .expect("no suggestion within 5 s");

        assert!(event.contains("them: ["), "expected a them-turn, got: {event}");
    }
}
