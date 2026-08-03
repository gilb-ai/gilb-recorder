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

mod aec;
mod tap;
pub use aec::{EchoCanceller, EchoCancellerConfig};
pub use tap::{AudioChunk, AudioTap};

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

    /// Install a live audio tap; chunks flow from the next `start` on. The
    /// default ignores the tap — the no-op and test capturers have no audio
    /// to offer, and consumers must treat the tap as best-effort anyway.
    fn set_audio_tap(&self, _tap: Arc<AudioTap>) {}
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
    // The mic/mic-raw/system sidecars exist only on the
    // abandoned-successful-start path, where the capturer's stop already wrote
    // them; on plain start failures they are absent and skipped as NotFound.
    let sidecars: Vec<PathBuf> = audio
        .parent()
        .map(|d| {
            vec![
                d.join("mic.wav"),
                d.join("mic-raw.wav"),
                d.join("system.wav"),
            ]
        })
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

/// Offline acoustic echo cancellation (speexdsp): clean the mic track against
/// the system track, both mono `f32` at `sample_rate`. Without headphones the
/// mic picks up the remote side from the speakers; uncancelled, the mic track
/// double-attributes their speech (decision D11 in `docs/assist.md`). Runs
/// as a batch at finalization — the live capture path is untouched. The tail
/// past the last full 20 ms frame is passed through unprocessed.
pub fn cancel_echo(mic: &[f32], system: &[f32], sample_rate: u32) -> Vec<f32> {
    if mic.is_empty() || system.is_empty() {
        return mic.to_vec();
    }
    let frame = (sample_rate / 50) as usize; // 20 ms
    let mut aec = EchoCanceller::new(&EchoCancellerConfig {
        sample_rate,
        frame_size: frame,
        tail_ms: 200, // covers speaker->mic delay
        // Pure cancellation only: denoise would alter the recording's character.
        enable_preprocess: false,
    });

    // Frame-wise interleave keeps the canceller's bounded far FIFO holding
    // exactly the matching reference audio — larger blocks would overflow it
    // and silently misalign the tracks.
    let mut out = Vec::with_capacity(mic.len());
    let mut pos = 0;
    while pos < mic.len() {
        let end = (pos + frame).min(mic.len());
        let far_end = end.min(system.len());
        if pos < far_end {
            aec.push_far(&system[pos..far_end]);
        }
        out.extend(aec.push_near(&mic[pos..end]));
        pos = end;
    }
    // The sub-frame remainder stays buffered inside the canceller; the track
    // must keep its length, so pass those samples through untouched.
    out.extend_from_slice(&mic[out.len()..]);
    out
}

/// The finalized 16 kHz audio tracks of a meeting, ready for
/// [`write_meeting_audio`]. `mic` is echo-cancelled; `mic_raw` is the
/// uncancelled original, kept as the safety net (a filter misbehaving must
/// never lose audio irrecoverably).
pub struct MeetingAudioTracks {
    pub mixed: Vec<i16>,
    pub mic: Vec<i16>,
    pub mic_raw: Vec<i16>,
    pub system: Vec<i16>,
}

/// Resample both capture buffers to 16 kHz, echo-cancel the mic against the
/// system audio, and produce all four tracks. Pure — host-tested; the platform
/// `stop()` impls feed their buffers here and write the result.
pub fn finalize_meeting_audio(
    mic: &[f32],
    mic_rate: u32,
    system: &[f32],
    system_rate: u32,
) -> MeetingAudioTracks {
    let mic_16 = resample_linear(mic, mic_rate, TARGET_SAMPLE_RATE);
    let sys_16 = resample_linear(system, system_rate, TARGET_SAMPLE_RATE);
    let clean_16 = cancel_echo(&mic_16, &sys_16, TARGET_SAMPLE_RATE);
    MeetingAudioTracks {
        mixed: mix_to_mono_16k_dual(&clean_16, TARGET_SAMPLE_RATE, &sys_16, TARGET_SAMPLE_RATE),
        mic: mix_to_mono_16k_dual(&clean_16, TARGET_SAMPLE_RATE, &[], 1),
        mic_raw: mix_to_mono_16k_dual(&mic_16, TARGET_SAMPLE_RATE, &[], 1),
        system: mix_to_mono_16k_dual(&sys_16, TARGET_SAMPLE_RATE, &[], 1),
    }
}

/// Write the meeting's audio files next to `audio_path` (the `audio.wav` mix):
/// `mic.wav` (echo-cancelled), `mic-raw.wav` (uncancelled safety net) and
/// `system.wav`.
pub fn write_meeting_audio(audio_path: &Path, tracks: &MeetingAudioTracks) -> Result<()> {
    let dir = audio_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    write_wav_16k_mono(audio_path, &tracks.mixed).context("write mixed meeting audio")?;
    write_wav_16k_mono(&dir.join("mic.wav"), &tracks.mic).context("write mic track")?;
    write_wav_16k_mono(&dir.join("mic-raw.wav"), &tracks.mic_raw).context("write raw mic track")?;
    write_wav_16k_mono(&dir.join("system.wav"), &tracks.system).context("write system track")?;
    Ok(())
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
    /// Waits between capture-start attempts; [`START_BACKOFF`] in production.
    start_backoff: Vec<Duration>,
}

impl<C: ScreenAudioCapturer> Recorder<C> {
    pub fn new(db: Db, data_dir: PathBuf, capturer: C) -> Self {
        Self {
            db,
            data_dir,
            capturer,
            state: Mutex::new(State::default()),
            start_backoff: START_BACKOFF.to_vec(),
        }
    }

    /// Install a live audio tap on the underlying capturer (see [`AudioTap`]).
    /// Takes effect on the next [`arm`](Self::arm).
    pub fn set_audio_tap(&self, tap: Arc<AudioTap>) {
        self.capturer.set_audio_tap(tap);
    }

    /// Replace the retry waits — a test hook, hidden from docs.
    ///
    /// Tests cannot shortcut the real backoff with tokio's paused clock:
    /// sqlite runs on its own thread, so during any sqlx call the runtime looks
    /// idle and auto-advances straight to the pool's acquire deadline — a race
    /// that passes on a fast machine and times the pool out on a loaded CI
    /// runner. Millisecond real waits are reliable everywhere.
    #[doc(hidden)]
    pub fn with_start_backoff(mut self, waits: Vec<Duration>) -> Self {
        self.start_backoff = waits;
        self
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
            let Some(backoff) = self.start_backoff.get(attempt - 1) else {
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

#[cfg(test)]
mod finalize_tests {
    use super::*;

    /// Deterministic white noise in [-0.4, 0.4].
    fn noise(len: usize, mut seed: u64) -> Vec<f32> {
        (0..len)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (((seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5) * 0.8
            })
            .collect()
    }

    fn energy(samples: &[i16]) -> f64 {
        samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum()
    }

    /// Speaker-echo-only mic: after finalization the cleaned mic track must be
    /// far quieter than the raw one, and all four tracks must line up.
    #[test]
    fn finalize_cancels_speaker_echo_and_keeps_raw() {
        let secs = 5;
        let system = noise(48_000 * secs, 7);
        let delay = 48_000 * 40 / 1000;
        // Mic hears only the speakers: delayed, attenuated system audio.
        let mic: Vec<f32> = (0..system.len())
            .map(|n| {
                if n >= delay {
                    system[n - delay] * 0.5
                } else {
                    0.0
                }
            })
            .collect();

        let tracks = finalize_meeting_audio(&mic, 48_000, &system, 48_000);

        assert_eq!(tracks.mic.len(), tracks.mic_raw.len());
        assert_eq!(
            tracks.mixed.len(),
            tracks.system.len().max(tracks.mic.len())
        );

        // Judge the last second, after the adaptive filter has converged.
        let tail = TARGET_SAMPLE_RATE as usize;
        let raw = energy(&tracks.mic_raw[tracks.mic_raw.len() - tail..]);
        let clean = energy(&tracks.mic[tracks.mic.len() - tail..]);
        let erle = 10.0 * (raw / clean.max(1.0)).log10();
        assert!(
            erle > 10.0,
            "expected > 10 dB echo attenuation, got {erle:.1} dB"
        );
    }

    /// Without system audio the mic passes through the finalizer unchanged.
    #[test]
    fn finalize_without_system_audio_is_passthrough() {
        let mic = noise(48_000, 42);
        let tracks = finalize_meeting_audio(&mic, 48_000, &[], 48_000);
        assert_eq!(tracks.mic, tracks.mic_raw);
        assert!(tracks.system.is_empty());
    }
}
