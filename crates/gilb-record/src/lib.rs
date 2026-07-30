//! Screen + audio recording pipeline for meetings.
//!
//! On [`RecordingEvent::Armed`] the [`Recorder`] captures the call app's window
//! to a native HEVC `.mp4` and mixes mic + system audio into a 16 kHz mono WAV
//! sidecar, writing both under a per-meeting folder named by start time —
//! `<data_dir>/meetings/<stamp>/{video.mp4, audio.wav}` (plus `mic.wav` /
//! `system.wav`) — and pointing the `meetings` row at them. On
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
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use gilb_db::{meetings, Db};
use gilb_events::{EventBus, RecordingEvent};
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

#[cfg(target_os = "macos")]
mod macos;
/// Public for `examples/tap_smoke.rs`; not part of the stable API.
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub mod macos_tap;

#[cfg(target_os = "windows")]
mod windows;

/// Target sample rate of the audio sidecar — 16 kHz mono, the rate downstream
/// transcription ([GILB-6]) consumes.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// How many times [`Recorder::arm`] tries to bring capture up before marking the
/// meeting failed.
///
/// Starting capture is racy through no fault of ours: the platform refuses when
/// the call app's process/window set is still churning, which is exactly the
/// moment a meeting is detected. On macOS that surfaces as ScreenCaptureKit
/// `-3818` out of a `replayd` race, and it clears within seconds. Treating the
/// first refusal as terminal cost a whole two-hour recording, so retry.
pub const START_ATTEMPTS: usize = 5;

/// Waits before each retry — ~15s of coverage in total, which outlasts the
/// window in which a call app is still settling after join.
const START_BACKOFF: [Duration; START_ATTEMPTS - 1] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];

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
    /// (16 kHz mono WAV). The parent directory already exists. `app_bundle_id`,
    /// when set, scopes the screen capture to that application's windows (the
    /// call app) rather than the whole display.
    fn start(
        &self,
        video_path: &Path,
        audio_path: &Path,
        app_bundle_id: Option<&str>,
    ) -> Result<()>;
    /// Stop capture and finalize both files.
    fn stop(&self) -> Result<()>;
}

/// No-op capturer for the non-macOS/non-Windows build and for tests. Writes
/// nothing.
#[derive(Debug, Default)]
pub struct NoopCapturer;

impl ScreenAudioCapturer for NoopCapturer {
    fn start(
        &self,
        _video_path: &Path,
        _audio_path: &Path,
        _app_bundle_id: Option<&str>,
    ) -> Result<()> {
        Ok(())
    }
    fn stop(&self) -> Result<()> {
        Ok(())
    }
}

/// The capturer wired up by [`spawn_recorder`]: the real ScreenCaptureKit stack
/// on macOS, the WGC + Media Foundation + WASAPI stack on Windows ([GILB-46]),
/// a no-op everywhere else.
#[cfg(target_os = "macos")]
pub type PlatformCapturer = macos::MacosCapturer;
/// See [`PlatformCapturer`].
#[cfg(target_os = "windows")]
pub type PlatformCapturer = windows::WindowsCapturer;
/// See [`PlatformCapturer`].
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub type PlatformCapturer = NoopCapturer;

/// Delete what a failed capture start left behind.
///
/// A backend may create its output before the step that actually fails —
/// AVAssetWriter opens `video.mp4` up front, so a refused `startCapture` leaves a
/// zero-byte file. That file then blocks every retry, because AVAssetWriter will
/// not open a URL that already exists (`AVErrorFileAlreadyExists`). Clearing it
/// also keeps a permanently failed meeting from leaving an orphan on disk that
/// nothing references, since the row never gets its paths set.
///
/// Best-effort: a file that cannot be removed is logged and the retry proceeds,
/// where it will surface as the start error instead.
fn clear_failed_attempt(video: &Path, audio: &Path) {
    // The mic/system sidecars exist only on the abandoned-successful-start
    // path, where the capturer's stop already wrote them; on plain start
    // failures they are absent and skipped as NotFound.
    let sidecars: Vec<PathBuf> = audio
        .parent()
        .map(|d| vec![d.join("mic.wav"), d.join("system.wav")])
        .unwrap_or_default();
    for path in [video, audio]
        .into_iter()
        .chain(sidecars.iter().map(PathBuf::as_path))
    {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "cannot remove a partial recording; a retry may fail on it"
                );
            }
        }
    }
}

/// Derive the on-disk paths for a meeting's recordings: each meeting gets its
/// own folder named by the recording start time `stamp` (e.g.
/// `2026-06-05_16-57-03`), holding `video.mp4` + `audio.wav` (and the
/// `mic.wav`/`system.wav` sidecars the recorder writes alongside).
pub fn meeting_paths(data_dir: &Path, stamp: &str) -> (PathBuf, PathBuf) {
    let dir = data_dir.join("meetings").join(stamp);
    let video = dir.join("video.mp4");
    let audio = dir.join("audio.wav");
    (video, audio)
}

/// Format `now` as the recording filename stamp (`%Y-%m-%d_%H-%M-%S`) in **local**
/// time so the name matches the user's wall clock. Filesystem-safe; sorts
/// chronologically.
fn recording_stamp(now: chrono::DateTime<chrono::Local>) -> String {
    now.format("%Y-%m-%d_%H-%M-%S").to_string()
}

/// Mix mic and system PCM (both mono `f32` at `src_rate` Hz) into a single mono
/// 16 kHz `i16` stream. Each channel is linearly resampled to 16 kHz, then the
/// two are summed sample-for-sample and clamped to `[-1, 1]` before scaling to
/// `i16`. The shorter channel is padded with silence so trailing audio from the
/// longer one is preserved.
pub fn mix_to_mono_16k(mic: &[f32], system: &[f32], src_rate: u32) -> Vec<i16> {
    mix_to_mono_16k_dual(mic, src_rate, system, src_rate)
}

/// Like [`mix_to_mono_16k`] but the two sources may be at **different** sample
/// rates — the mic (device rate) and system audio (SCK's 48 kHz) usually are.
/// Each is resampled to 16 kHz from its own rate, then summed.
pub fn mix_to_mono_16k_dual(a: &[f32], a_rate: u32, b: &[f32], b_rate: u32) -> Vec<i16> {
    let a = resample_linear(a, a_rate, TARGET_SAMPLE_RATE);
    let b = resample_linear(b, b_rate, TARGET_SAMPLE_RATE);
    let len = a.len().max(b.len());
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let x = a.get(i).copied().unwrap_or(0.0);
        let y = b.get(i).copied().unwrap_or(0.0);
        let mixed = (x + y).clamp(-1.0, 1.0);
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

/// Bilinearly scale a tightly-packed BGRA image (`src_w`×`src_h`, 4 bytes per
/// pixel, no row padding) to `dst_w`×`dst_h`, returning the packed result. Used
/// by the Windows backend to fit a variable-sized captured window into the
/// fixed-size video encoder (a stretch, matching macOS's fixed-config capture),
/// so a window resize or swap never changes the encoder's frame size. Returns
/// zeroed output for degenerate dimensions or a too-short source.
pub fn scale_bgra(src: &[u8], src_w: usize, src_h: usize, dst_w: usize, dst_h: usize) -> Vec<u8> {
    let mut dst = vec![0u8; dst_w * dst_h * 4];
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 || src.len() < src_w * src_h * 4 {
        return dst;
    }
    if src_w == dst_w && src_h == dst_h {
        dst.copy_from_slice(&src[..dst_w * dst_h * 4]);
        return dst;
    }
    let sx = src_w as f32 / dst_w as f32;
    let sy = src_h as f32 / dst_h as f32;
    for dy in 0..dst_h {
        // Map the dst pixel centre back to src space (the -0.5/+0.5 keeps the
        // sample grid centred so edges aren't biased).
        let fy = ((dy as f32 + 0.5) * sy - 0.5).max(0.0);
        let y0 = (fy.floor() as usize).min(src_h - 1);
        let y1 = (y0 + 1).min(src_h - 1);
        let wy = fy - y0 as f32;
        for dx in 0..dst_w {
            let fx = ((dx as f32 + 0.5) * sx - 0.5).max(0.0);
            let x0 = (fx.floor() as usize).min(src_w - 1);
            let x1 = (x0 + 1).min(src_w - 1);
            let wx = fx - x0 as f32;
            let di = (dy * dst_w + dx) * 4;
            let i00 = (y0 * src_w + x0) * 4;
            let i01 = (y0 * src_w + x1) * 4;
            let i10 = (y1 * src_w + x0) * 4;
            let i11 = (y1 * src_w + x1) * 4;
            for c in 0..4 {
                let top = src[i00 + c] as f32 + (src[i01 + c] as f32 - src[i00 + c] as f32) * wx;
                let bot = src[i10 + c] as f32 + (src[i11 + c] as f32 - src[i10 + c] as f32) * wx;
                dst[di + c] = (top + (bot - top) * wy).round() as u8;
            }
        }
    }
    dst
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
/// What the recorder is doing right now, behind a single mutex so [`arm`]'s
/// retry loop and a concurrent [`stop`] can never see a half-updated view.
///
/// [`arm`]: Recorder::arm
/// [`stop`]: Recorder::stop
#[derive(Default)]
struct State {
    /// `meeting_id` whose capture is live.
    active: Option<i64>,
    /// `meeting_id` whose capture is still being brought up, i.e. `arm` is
    /// between retries. [`Recorder::stop`] clears this, which is how a retry loop
    /// learns the call ended before capture ever started — without it, `arm`
    /// would happily bring a stream up minutes after everyone hung up.
    arming: Option<i64>,
}

pub struct Recorder<C: ScreenAudioCapturer = PlatformCapturer> {
    db: Db,
    data_dir: PathBuf,
    capturer: C,
    state: Mutex<State>,
}

impl<C: ScreenAudioCapturer> Recorder<C> {
    pub fn new(db: Db, data_dir: PathBuf, capturer: C) -> Self {
        Self {
            db,
            data_dir,
            capturer,
            state: Mutex::new(State::default()),
        }
    }

    /// The underlying capturer. Lets callers inspect backend state the trait
    /// doesn't expose — tests assert on how many starts a retry actually made.
    pub fn capturer(&self) -> &C {
        &self.capturer
    }

    /// Is this `arm` call still the one the recorder wants? `stop` clears
    /// `arming`, so `false` means we were abandoned mid-retry.
    fn still_arming(&self, meeting_id: i64) -> bool {
        self.state.lock().expect("recorder mutex poisoned").arming == Some(meeting_id)
    }

    /// Give up on arming `meeting_id`: clear the slot and stamp the row failed.
    async fn abandon_arming(&self, meeting_id: i64) {
        {
            let mut state = self.state.lock().expect("recorder mutex poisoned");
            if state.arming == Some(meeting_id) {
                state.arming = None;
            }
        }
        let now = Utc::now().timestamp_millis();
        let _ = meetings::finish_meeting(&self.db, meeting_id, now, "failed").await;
    }

    /// Start capturing for `meeting_id`: derive paths, kick off capture, and
    /// record the paths on the row. A second arm while one is active — or while
    /// another is still retrying — is ignored (the in-flight one keeps going).
    ///
    /// Capture start is retried up to [`START_ATTEMPTS`] times with
    /// [`START_BACKOFF`] between tries, because the platform commonly refuses for
    /// a few seconds right after a call app joins. The row is only stamped
    /// `failed` once every attempt is used up, or if [`Recorder::stop`] abandons
    /// us in the meantime.
    pub async fn arm(&self, meeting_id: i64) -> Result<()> {
        // Three cases, and they are not the same: a duplicate event for the
        // meeting already in hand, a *different* meeting arriving while one is
        // being captured, or a clean slate.
        enum Claim {
            Duplicate,
            Busy,
            Ours,
        }
        let claim = {
            let mut state = self.state.lock().expect("recorder mutex poisoned");
            if state.active == Some(meeting_id) || state.arming == Some(meeting_id) {
                Claim::Duplicate
            } else if state.active.is_some() || state.arming.is_some() {
                Claim::Busy
            } else {
                state.arming = Some(meeting_id);
                Claim::Ours
            }
        };
        match claim {
            Claim::Duplicate => {
                warn!(meeting_id, "arm ignored: already handling this meeting");
                return Ok(());
            }
            Claim::Busy => {
                warn!(meeting_id, "arm ignored: another recording is active");
                // Retire the row rather than leave it claiming to record. Nothing
                // will ever stop a meeting that was never armed, so it would sit
                // in `recording` until the next startup sweep and misreport an
                // active capture until then.
                let now = Utc::now().timestamp_millis();
                let _ = meetings::finish_meeting(&self.db, meeting_id, now, "cancelled").await;
                return Ok(());
            }
            Claim::Ours => {}
        }

        let stamp = recording_stamp(chrono::Local::now());
        let (video, audio) = meeting_paths(&self.data_dir, &stamp);
        if let Some(parent) = video.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                self.abandon_arming(meeting_id).await;
                return Err(err)
                    .with_context(|| format!("create meetings dir {}", parent.display()));
            }
        }

        // The meeting's `app` column holds the call app's bundle id; use it to
        // scope screen capture to that app's windows.
        let app_bundle_id = meetings::get_meeting(&self.db, meeting_id)
            .await
            .ok()
            .flatten()
            .map(|m| m.app);

        let mut last_err = None;
        for attempt in 1..=START_ATTEMPTS {
            // A failed attempt leaves its half-open output behind, and at least
            // AVAssetWriter refuses to open a URL that already exists
            // (`AVErrorFileAlreadyExists`) — so without this every retry past the
            // first is guaranteed to fail on the leftover, whatever the original
            // problem was.
            if attempt > 1 {
                clear_failed_attempt(&video, &audio);
            }

            match self
                .capturer
                .start(&video, &audio, app_bundle_id.as_deref())
            {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(err) => {
                    warn!(
                        meeting_id,
                        attempt,
                        attempts = START_ATTEMPTS,
                        error = %err,
                        "failed to start capture"
                    );
                    last_err = Some(err);
                }
            }

            // Out of attempts: fall through to the failure path below.
            let Some(backoff) = START_BACKOFF.get(attempt - 1) else {
                break;
            };
            tokio::time::sleep(*backoff).await;

            if !self.still_arming(meeting_id) {
                info!(
                    meeting_id,
                    "meeting ended while capture start was retrying; giving up"
                );
                break;
            }
        }

        if let Some(err) = last_err {
            clear_failed_attempt(&video, &audio);
            self.abandon_arming(meeting_id).await;
            return Err(err).context("start screen/audio capture");
        }

        // Capture is live. Claim it — unless `stop` cleared us while the
        // successful start was in flight, in which case the call is already over.
        // Decide under the lock, act outside it: the guard is not `Send` and the
        // cleanup path awaits.
        let claimed = {
            let mut state = self.state.lock().expect("recorder mutex poisoned");
            if state.arming == Some(meeting_id) {
                state.arming = None;
                state.active = Some(meeting_id);
                true
            } else {
                false
            }
        };

        if !claimed {
            // Tear the capture straight back down rather than leave an orphan
            // recording that nothing will ever stop.
            info!(
                meeting_id,
                "capture started after the meeting ended; stopping it again"
            );
            if let Err(err) = self.capturer.stop() {
                warn!(meeting_id, error = %err, "failed to stop an orphaned capture");
            }
            clear_failed_attempt(&video, &audio);
            let now = Utc::now().timestamp_millis();
            let _ = meetings::finish_meeting(&self.db, meeting_id, now, "cancelled").await;
            return Ok(());
        }

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
            let mut state = self.state.lock().expect("recorder mutex poisoned");
            // Abandon any in-flight arm: whatever it is waiting for is moot now,
            // and `arm` checks this between retries.
            state.arming = None;
            match state.active.take() {
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
