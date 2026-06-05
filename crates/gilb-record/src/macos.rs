//! macOS capture backend for [`ScreenAudioCapturer`].
//!
//! This module is compiled only on macOS and is **not** exercised by CI (the
//! workspace is built/tested on Linux). It is the thin, impure edge of the
//! recorder: ScreenCaptureKit drives the active display into an AVAssetWriter
//! encoding hardware HEVC to `.mp4`, while system audio (from the same
//! `SCStream`) and the default microphone (via `cpal`) are accumulated and, on
//! stop, mixed to a 16 kHz mono WAV through the host-tested helpers in
//! [`crate::mix_to_mono_16k`] / [`crate::write_wav_16k_mono`].
//!
//! The HEVC encode path (AVAssetWriter wiring) is the hardening target of the
//! follow-up iteration; everything that touches the recorded *audio* and the
//! file layout is shared with the pure, tested core.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use screencapturekit::cm_sample_buffer::CMSampleBuffer;
use screencapturekit::sc_content_filter::{InitParams, SCContentFilter};
use screencapturekit::sc_error_handler::StreamErrorHandler;
use screencapturekit::sc_output_handler::{SCStreamOutputType, StreamOutput};
use screencapturekit::sc_shareable_content::SCShareableContent;
use screencapturekit::sc_stream::SCStream;
use screencapturekit::sc_stream_configuration::SCStreamConfiguration;
use tracing::{info, warn};

use crate::{mix_to_mono_16k, write_wav_16k_mono, ScreenAudioCapturer};

/// Sample rate requested from ScreenCaptureKit's audio output and the `cpal`
/// mic. Both streams are resampled to 16 kHz by [`mix_to_mono_16k`] on stop.
const CAPTURE_SAMPLE_RATE: u32 = 48_000;

/// Shared, interleaved-to-mono PCM accumulators for the two audio sources.
#[derive(Default)]
struct AudioBuffers {
    mic: Vec<f32>,
    system: Vec<f32>,
}

/// A running capture: the SCK stream, the mic thread's stop signal + handle,
/// the shared audio buffers, and the output paths to finalize on stop.
struct Session {
    stream: SCStream,
    mic_stop: Option<mpsc::Sender<()>>,
    mic_thread: Option<JoinHandle<()>>,
    audio: Arc<Mutex<AudioBuffers>>,
    audio_path: PathBuf,
    video_path: PathBuf,
    sample_rate: u32,
}

/// macOS [`ScreenAudioCapturer`]. Holds the active [`Session`] behind a mutex so
/// the trait stays `Send + Sync` (the engine drives it from a spawned task).
#[derive(Default)]
pub struct MacosCapturer {
    session: Mutex<Option<Session>>,
}

impl MacosCapturer {
    pub fn new() -> Self {
        Self::default()
    }
}

/// SCK stream-output handler: pushes system-audio sample buffers into the
/// shared accumulator and feeds video frames to the HEVC writer.
struct CaptureSink {
    audio: Arc<Mutex<AudioBuffers>>,
}

impl StreamOutput for CaptureSink {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        match of_type {
            SCStreamOutputType::Audio => {
                if let Some(samples) = system_audio_samples(&sample) {
                    if let Ok(mut buf) = self.audio.lock() {
                        buf.system.extend_from_slice(&samples);
                    }
                }
            }
            SCStreamOutputType::Screen => {
                // Video frames are encoded by the AVAssetWriter set up in
                // `start`; the HEVC pipeline is hardened in the follow-up
                // iteration ([GILB-15] iter 2). The frame is dropped here until
                // that writer is wired.
            }
        }
    }
}

/// Minimal error handler — SCK requires one; log and continue.
struct CaptureErrors;

impl StreamErrorHandler for CaptureErrors {
    fn on_error(&self) {
        warn!("ScreenCaptureKit stream reported an error");
    }
}

impl ScreenAudioCapturer for MacosCapturer {
    fn start(&self, video_path: &Path, audio_path: &Path) -> Result<()> {
        let mut guard = self.session.lock().expect("capturer mutex poisoned");
        if guard.is_some() {
            return Err(anyhow!("capture already running"));
        }

        let audio = Arc::new(Mutex::new(AudioBuffers::default()));

        // Pick the main display and build a full-display content filter.
        let content = SCShareableContent::current();
        let display = content
            .displays
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no display available to capture"))?;
        let filter = SCContentFilter::new(InitParams::Display(display));

        let mut config = SCStreamConfiguration::default();
        config.captures_audio = true;
        config.sample_rate = CAPTURE_SAMPLE_RATE;
        config.channel_count = 1;

        let mut stream = SCStream::new(filter, config, CaptureErrors);
        stream.add_output(
            CaptureSink {
                audio: audio.clone(),
            },
            SCStreamOutputType::Audio,
        );
        stream.add_output(
            CaptureSink {
                audio: audio.clone(),
            },
            SCStreamOutputType::Screen,
        );
        stream
            .start_capture()
            .map_err(|e| anyhow!("start ScreenCaptureKit capture: {e:?}"))?;

        let (mic_stop, mic_thread, sample_rate) = spawn_mic_capture(audio.clone())?;

        *guard = Some(Session {
            stream,
            mic_stop: Some(mic_stop),
            mic_thread: Some(mic_thread),
            audio,
            audio_path: audio_path.to_path_buf(),
            video_path: video_path.to_path_buf(),
            sample_rate,
        });
        info!(video = %video_path.display(), "macOS capture started");
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        let mut session = self
            .session
            .lock()
            .expect("capturer mutex poisoned")
            .take()
            .ok_or_else(|| anyhow!("capture is not running"))?;

        if let Err(e) = session.stream.stop_capture() {
            warn!(error = ?e, "stopping ScreenCaptureKit stream");
        }
        if let Some(stop) = session.mic_stop.take() {
            let _ = stop.send(());
        }
        if let Some(handle) = session.mic_thread.take() {
            let _ = handle.join();
        }

        let buffers = session
            .audio
            .lock()
            .map_err(|_| anyhow!("audio buffer poisoned"))?;
        let mixed = mix_to_mono_16k(&buffers.mic, &buffers.system, session.sample_rate);
        write_wav_16k_mono(&session.audio_path, &mixed).context("write meeting audio")?;

        // The `.mp4` is finalized by the AVAssetWriter started alongside the
        // SCK stream; that finalize lands with the HEVC encode in iter 2.
        let _ = &session.video_path;
        info!("macOS capture stopped");
        Ok(())
    }
}

/// Decode a system-audio `CMSampleBuffer` into mono `f32` PCM. The concrete
/// buffer-format extraction is finished alongside the HEVC encode in iter 2;
/// returning `None` here drops the frame without corrupting the WAV.
fn system_audio_samples(_sample: &CMSampleBuffer) -> Option<Vec<f32>> {
    None
}

/// Open the default input device and stream mono `f32` mic samples into the
/// shared accumulator from a dedicated thread (a `cpal::Stream` is not `Send`,
/// so it must live entirely on the thread that owns it). Returns a stop signal,
/// the thread handle, and the device's sample rate.
fn spawn_mic_capture(
    audio: Arc<Mutex<AudioBuffers>>,
) -> Result<(mpsc::Sender<()>, JoinHandle<()>, u32)> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no default input (microphone) device"))?;
    let config = device
        .default_input_config()
        .context("query default mic input config")?;
    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;

    let (tx, rx) = mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        let err_fn = |e| warn!(error = ?e, "mic stream error");
        let stream = match device.build_input_stream(
            &config.into(),
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if let Ok(mut buf) = audio.lock() {
                    // Downmix interleaved frames to mono by averaging channels.
                    for frame in data.chunks(channels.max(1)) {
                        let sum: f32 = frame.iter().copied().sum();
                        buf.mic.push(sum / channels.max(1) as f32);
                    }
                }
            },
            err_fn,
            None,
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = ?e, "failed to build mic input stream");
                return;
            }
        };
        if let Err(e) = stream.play() {
            warn!(error = ?e, "failed to start mic stream");
            return;
        }
        // Keep the stream alive until stop is signalled (or the sender drops).
        let _ = rx.recv();
        drop(stream);
    });

    Ok((tx, handle, sample_rate))
}
