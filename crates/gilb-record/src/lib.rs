//! Screen + audio recording pipeline for meetings.
//!
//! On [`RecordingEvent::Armed`] the [`Recorder`] captures the active macOS
//! display to a native HEVC `.mp4` and mixes mic + system audio into a 16 kHz
//! mono WAV sidecar, writing both under `<data_dir>/meetings/<id>.{mp4,
//! audio.wav}` and pointing the `meetings` row at them. On
//! [`RecordingEvent::Cancelled`] (or an engine-driven [`Recorder::stop`]) it
//! stops capture and stamps the row's terminal `status` + `ended_at`.
//!
//! The platform capture stack (ScreenCaptureKit + AVAssetWriter + `cpal`) lives
//! behind the [`ScreenAudioCapturer`] trait in `macos.rs`, compiled only on
//! macOS. Everything host-testable — path derivation, the audio mix/resample,
//! the WAV writer, the meetings-row SQL and the Armed/Cancelled state machine —
//! is pure and lives here. The recorder owns the row's recording fields, not
//! its creation (the engine inserts the row before arming).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::Utc;
use gilb_db::{meetings, Db};
use gilb_events::{EventBus, RecordingEvent};
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

#[cfg(target_os = "macos")]
mod macos;

/// Target sample rate of the audio sidecar — 16 kHz mono, the rate downstream
/// transcription ([GILB-6]) consumes.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Terminal state written to the `meetings` row when capture stops. Maps onto
/// the `status` CHECK constraint in `0004_meetings.sql`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingOutcome {
    Completed,
    Cancelled,
    Failed,
}

impl RecordingOutcome {
    /// The `meetings.status` string for this outcome.
    pub fn as_status(self) -> &'static str {
        match self {
            RecordingOutcome::Completed => "completed",
            RecordingOutcome::Cancelled => "cancelled",
            RecordingOutcome::Failed => "failed",
        }
    }
}

/// Captures the active display + mic/system audio to disk. The macOS impl wraps
/// ScreenCaptureKit + AVAssetWriter + `cpal`; tests use a no-op or counting
/// stand-in. Calls are quick: `start` kicks off background streams and returns,
/// `stop` finalizes the on-disk files.
pub trait ScreenAudioCapturer: Send + Sync {
    /// Begin capturing to `video_path` (HEVC `.mp4`) and `audio_path`
    /// (16 kHz mono WAV). The parent directory already exists.
    fn start(&self, video_path: &Path, audio_path: &Path) -> Result<()>;
    /// Stop capture and finalize both files.
    fn stop(&self) -> Result<()>;
}

/// No-op capturer for the non-macOS build and for tests. Writes nothing.
#[derive(Debug, Default)]
pub struct NoopCapturer;

impl ScreenAudioCapturer for NoopCapturer {
    fn start(&self, _video_path: &Path, _audio_path: &Path) -> Result<()> {
        Ok(())
    }
    fn stop(&self) -> Result<()> {
        Ok(())
    }
}

/// The capturer wired up by [`spawn_recorder`]: the real ScreenCaptureKit stack
/// on macOS, a no-op everywhere else (Windows recording is [GILB-46]).
#[cfg(target_os = "macos")]
pub type PlatformCapturer = macos::MacosCapturer;
/// See [`PlatformCapturer`].
#[cfg(not(target_os = "macos"))]
pub type PlatformCapturer = NoopCapturer;

/// Derive the on-disk paths for a meeting's recordings:
/// `<data_dir>/meetings/<id>.mp4` and `<data_dir>/meetings/<id>.audio.wav`.
pub fn meeting_paths(data_dir: &Path, meeting_id: i64) -> (PathBuf, PathBuf) {
    let dir = data_dir.join("meetings");
    let video = dir.join(format!("{meeting_id}.mp4"));
    let audio = dir.join(format!("{meeting_id}.audio.wav"));
    (video, audio)
}

/// Mix mic and system PCM (both mono `f32` at `src_rate` Hz) into a single mono
/// 16 kHz `i16` stream. Each channel is linearly resampled to 16 kHz, then the
/// two are summed sample-for-sample and clamped to `[-1, 1]` before scaling to
/// `i16`. The shorter channel is padded with silence so trailing audio from the
/// longer one is preserved.
pub fn mix_to_mono_16k(mic: &[f32], system: &[f32], src_rate: u32) -> Vec<i16> {
    let mic = resample_linear(mic, src_rate, TARGET_SAMPLE_RATE);
    let system = resample_linear(system, src_rate, TARGET_SAMPLE_RATE);
    let len = mic.len().max(system.len());
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let m = mic.get(i).copied().unwrap_or(0.0);
        let s = system.get(i).copied().unwrap_or(0.0);
        let mixed = (m + s).clamp(-1.0, 1.0);
        out.push((mixed * i16::MAX as f32).round() as i16);
    }
    out
}

/// Linear-interpolation resample of mono `f32` samples from `from` to `to` Hz.
/// Returns the input unchanged when the rates match; empty in, empty out.
fn resample_linear(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if samples.is_empty() || from == 0 || to == 0 {
        return Vec::new();
    }
    if from == to {
        return samples.to_vec();
    }
    let ratio = to as f64 / from as f64;
    let out_len = ((samples.len() as f64) * ratio).round() as usize;
    let last = samples.len() - 1;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 / ratio;
        let idx = src_pos.floor() as usize;
        let frac = (src_pos - idx as f64) as f32;
        let a = samples[idx.min(last)];
        let b = samples[(idx + 1).min(last)];
        out.push(a + (b - a) * frac);
    }
    out
}

/// Write `samples` as a 16 kHz mono 16-bit PCM WAV at `path`.
pub fn write_wav_16k_mono(path: &Path, samples: &[i16]) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .with_context(|| format!("create wav at {}", path.display()))?;
    for &s in samples {
        writer.write_sample(s).context("write wav sample")?;
    }
    writer.finalize().context("finalize wav")?;
    Ok(())
}

/// Drives capture in response to [`RecordingEvent`]s and owns the `meetings`
/// row's recording fields. Generic over the capturer so tests inject a
/// stand-in; production uses [`PlatformCapturer`].
pub struct Recorder<C: ScreenAudioCapturer = PlatformCapturer> {
    db: Db,
    data_dir: PathBuf,
    capturer: C,
    /// `meeting_id` currently being captured, if any.
    active: Mutex<Option<i64>>,
}

impl<C: ScreenAudioCapturer> Recorder<C> {
    pub fn new(db: Db, data_dir: PathBuf, capturer: C) -> Self {
        Self {
            db,
            data_dir,
            capturer,
            active: Mutex::new(None),
        }
    }

    /// Start capturing for `meeting_id`: derive paths, kick off capture, and
    /// record the paths on the row. A second arm while one is active is
    /// ignored (the in-flight capture keeps running).
    pub async fn arm(&self, meeting_id: i64) -> Result<()> {
        {
            let active = self.active.lock().expect("recorder mutex poisoned");
            if active.is_some() {
                warn!(meeting_id, "arm ignored: a recording is already active");
                return Ok(());
            }
        }

        let (video, audio) = meeting_paths(&self.data_dir, meeting_id);
        if let Some(parent) = video.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create meetings dir {}", parent.display()))?;
        }

        if let Err(err) = self.capturer.start(&video, &audio) {
            let now = Utc::now().timestamp_millis();
            let _ = meetings::finish_meeting(&self.db, meeting_id, now, "failed").await;
            return Err(err).context("start screen/audio capture");
        }

        *self.active.lock().expect("recorder mutex poisoned") = Some(meeting_id);

        meetings::set_recording_paths(
            &self.db,
            meeting_id,
            &video.to_string_lossy(),
            &audio.to_string_lossy(),
        )
        .await
        .context("record meeting paths")?;

        info!(meeting_id, "recording armed");
        Ok(())
    }

    /// Stop the active capture (if any) and stamp the row with `outcome` +
    /// `ended_at`. A no-op when nothing is being captured.
    pub async fn stop(&self, outcome: RecordingOutcome) -> Result<()> {
        let meeting_id = {
            let mut active = self.active.lock().expect("recorder mutex poisoned");
            match active.take() {
                Some(id) => id,
                None => return Ok(()),
            }
        };

        let capture_result = self.capturer.stop();
        let status = if capture_result.is_err() {
            RecordingOutcome::Failed
        } else {
            outcome
        };

        let now = Utc::now().timestamp_millis();
        meetings::finish_meeting(&self.db, meeting_id, now, status.as_status())
            .await
            .context("finish meeting")?;

        match &capture_result {
            Ok(()) => info!(meeting_id, status = status.as_status(), "recording stopped"),
            Err(err) => warn!(meeting_id, error = %err, "capture stop failed; marked failed"),
        }
        capture_result.context("stop screen/audio capture")
    }

    /// Subscribe to the bus and map [`RecordingEvent`]s onto the state machine
    /// until the bus closes. `Armed` → [`arm`](Self::arm); `Cancelled` →
    /// [`stop`](Self::stop) with [`RecordingOutcome::Cancelled`].
    pub async fn run(self: Arc<Self>, bus: EventBus) {
        let mut rx = bus.subscribe_recording();
        loop {
            match rx.recv().await {
                Ok(msg) => match msg.payload {
                    RecordingEvent::Armed { meeting_id } => {
                        if let Err(err) = self.arm(meeting_id).await {
                            warn!(meeting_id, error = %err, "failed to arm recorder");
                        }
                    }
                    RecordingEvent::Cancelled { meeting_id } => {
                        if let Err(err) = self.stop(RecordingOutcome::Cancelled).await {
                            warn!(meeting_id, error = %err, "failed to stop recorder on cancel");
                        }
                    }
                },
                Err(RecvError::Lagged(skipped)) => {
                    warn!(skipped, "recorder lagged behind the event bus");
                }
                Err(RecvError::Closed) => break,
            }
        }
    }
}

/// Build a [`Recorder`] with the platform capturer, spawn its [`run`](Recorder::run)
/// loop on the current runtime, and return the handle so the engine can drive
/// [`Recorder::stop`] on `MeetingEvent::Ended`.
pub fn spawn_recorder(bus: EventBus, db: Db, data_dir: PathBuf) -> Arc<Recorder<PlatformCapturer>> {
    let recorder = Arc::new(Recorder::new(db, data_dir, PlatformCapturer::default()));
    tokio::spawn(recorder.clone().run(bus));
    recorder
}
