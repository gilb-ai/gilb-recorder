//! macOS capture backend for [`ScreenAudioCapturer`].
//!
//! This module is compiled only on macOS and is **not** exercised by CI (the
//! workspace is built/tested on Linux). It is the thin, impure edge of the
//! recorder.
//!
//! **Two independent `SCStream`s, on purpose.** Video and system audio used to
//! ride one window-scoped stream, which made the *audio* hostage to the
//! *window*: every time the call app swapped or destroyed its window (join,
//! screen-share start/stop, panels) SCK killed the stream and the far end's
//! voice went missing for the rest of the call — silently, because the mic kept
//! recording. Rebuilding that stream also re-ran the audio-tap setup each time,
//! which is exactly the race that makes `startCapture` fail with `-3818`. So:
//!
//! * [`build_audio_stream`] — **app**-scoped ([`SCContentFilter`] over the app,
//!   not a window), audio output only. No `window_id` in the filter, so window
//!   churn can't touch it. Built once per recording and never rebuilt.
//! * [`build_window_stream`] — **window**-scoped (a desktop-independent window
//!   filter that follows the window across monitors), screen output only, audio
//!   explicitly off. Fragile by nature, so [`spawn_window_watcher`] watches it
//!   and re-targets onto the app's new window; a failure here costs picture, not
//!   the transcript.
//!
//! Screen frames go to an AVAssetWriter encoding HEVC to `.mp4`
//! ([`VideoWriter`]). Audio comes from two sources — the app-scoped stream
//! carries *system* audio ([`system_audio_samples`]) and `cpal` carries the
//! *mic* ([`spawn_mic_capture`]) — both accumulated and, on stop, written via
//! the host-tested helpers in [`crate::mix_to_mono_16k`] /
//! [`crate::write_wav_16k_mono`] as three sidecars: `mic.wav`, `system.wav`, and
//! the `audio.wav` mix. The mix is then muxed into the `.mp4`
//! ([`mux_audio_into_video`]) so the final video plays with sound.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use core_foundation::base::{CFRelease, TCFType};
use core_foundation::error::CFError;
use core_media_rs::cm_sample_buffer::CMSampleBuffer;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AllocAnyThread, Message};
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVFileTypeMPEG4, AVMediaTypeVideo, AVVideoCodecKey,
    AVVideoCodecTypeHEVC, AVVideoHeightKey, AVVideoWidthKey,
};
use objc2_core_media::{
    kCMTimeInvalid, kCMTimeZero, CMSampleBuffer as ObjcCMSampleBuffer, CMSampleTimingInfo, CMTime,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
use screencapturekit::output::sc_stream_frame_info::{SCFrameStatus, SCStreamFrameInfo};
use screencapturekit::shareable_content::{SCShareableContent, SCWindow};
use screencapturekit::stream::configuration::SCStreamConfiguration;
use screencapturekit::stream::content_filter::SCContentFilter;
use screencapturekit::stream::delegate_trait::SCStreamDelegateTrait;
use screencapturekit::stream::output_trait::SCStreamOutputTrait;
use screencapturekit::stream::output_type::SCStreamOutputType;
use screencapturekit::stream::SCStream;
use tracing::{info, trace, warn};

use crate::{mix_to_mono_16k_dual, write_wav_16k_mono, ScreenAudioCapturer};

/// Sample rate requested from ScreenCaptureKit's audio output and the `cpal`
/// mic. Both streams are resampled to 16 kHz by [`mix_to_mono_16k`] on stop.
const CAPTURE_SAMPLE_RATE: u32 = 48_000;

/// Capture frame rate cap. ScreenCaptureKit defaults to ~60 fps; meetings are
/// low-motion, so cap to 15 (parity with the Windows backend) to roughly halve
/// the recording size. Applied via `minimumFrameInterval` = 1/`VIDEO_FPS`.
const VIDEO_FPS: i32 = 15;

/// How often [`spawn_window_watcher`] re-examines the capture. Kept coarse: a
/// re-target is disruptive, and `SCShareableContent::get` is not free.
const WATCH_TICK: Duration = Duration::from_millis(1_500);

/// Rebuild the video stream if no `Complete` frame arrived for this long. Covers
/// the case SCK stops delivering *without* reporting an error through the
/// delegate — the failure mode that silently truncated past recordings.
///
/// Deliberately far above the frame interval (1/[`VIDEO_FPS`]): SCK does not
/// keep emitting frames for a window whose contents don't change. Measured on an
/// idle window, gaps reach ~8s, so anything near that would spend the recording
/// rebuilding a perfectly healthy stream. During a real call the window updates
/// continuously, which makes a gap this long unambiguous — and the cost of
/// waiting is now bounded to picture only, since audio is on its own stream.
const FRAME_STALL: Duration = Duration::from_secs(30);

/// Output size for the audio-only stream. SCK wants a valid video config even
/// when no screen output handler is attached; keep it minimal so compositing
/// costs nothing.
const AUDIO_STREAM_DIMS: u32 = 16;

/// Shared, interleaved-to-mono PCM accumulators for the two audio sources.
#[derive(Default)]
struct AudioBuffers {
    mic: Vec<f32>,
    system: Vec<f32>,
}

/// Upcast any objc2 object reference to `&AnyObject` (for heterogeneous
/// dictionary values). Sound: every Objective-C object shares the `isa` layout.
fn as_any<T: Message>(obj: &T) -> &AnyObject {
    unsafe { &*(obj as *const T as *const AnyObject) }
}

/// AVAssetWriter-backed HEVC `.mp4` encoder for the captured screen frames.
///
/// SCK delivers `CMSampleBuffer`s on its own dispatch queue, so this lives
/// behind a `Mutex`: `append` (capture queue) and `finish` (stop) are
/// serialized. The objc2 writer objects aren't `Send`, but every access goes
/// through that mutex, so the `unsafe impl Send` is sound.
struct VideoWriter {
    writer: Retained<AVAssetWriter>,
    input: Retained<AVAssetWriterInput>,
    started: bool,
    /// Wall clock at the first frame; frame PTS are derived from it.
    start: Option<std::time::Instant>,
}

// SAFETY: all access is serialized through the owning `Mutex<VideoWriter>`.
unsafe impl Send for VideoWriter {}

impl VideoWriter {
    /// Build an HEVC writer targeting `path` at `width`×`height` and start it.
    fn new(path: &Path, width: u32, height: u32) -> Result<Self> {
        unsafe {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            let file_type =
                AVFileTypeMPEG4.ok_or_else(|| anyhow!("AVFileTypeMPEG4 unavailable"))?;
            let writer =
                AVAssetWriter::initWithURL_fileType_error(AVAssetWriter::alloc(), &url, file_type)
                    .map_err(|e| anyhow!("create AVAssetWriter: {e:?}"))?;

            // outputSettings = { AVVideoCodecKey: HEVC, width, height }.
            let codec_key =
                AVVideoCodecKey.ok_or_else(|| anyhow!("AVVideoCodecKey unavailable"))?;
            let width_key =
                AVVideoWidthKey.ok_or_else(|| anyhow!("AVVideoWidthKey unavailable"))?;
            let height_key =
                AVVideoHeightKey.ok_or_else(|| anyhow!("AVVideoHeightKey unavailable"))?;
            let hevc =
                AVVideoCodecTypeHEVC.ok_or_else(|| anyhow!("AVVideoCodecTypeHEVC unavailable"))?;
            let w = NSNumber::numberWithInt(width as i32);
            let h = NSNumber::numberWithInt(height as i32);
            let keys: [&NSString; 3] = [codec_key, width_key, height_key];
            let values: [&AnyObject; 3] = [as_any(hevc), as_any(&*w), as_any(&*h)];
            let settings: Retained<NSDictionary<NSString, AnyObject>> =
                NSDictionary::from_slices(&keys, &values);

            let media_video =
                AVMediaTypeVideo.ok_or_else(|| anyhow!("AVMediaTypeVideo unavailable"))?;
            let input = AVAssetWriterInput::initWithMediaType_outputSettings(
                AVAssetWriterInput::alloc(),
                media_video,
                Some(&settings),
            );
            input.setExpectsMediaDataInRealTime(true);

            if !writer.canAddInput(&input) {
                return Err(anyhow!("AVAssetWriter rejects the video input"));
            }
            writer.addInput(&input);
            if !writer.startWriting() {
                return Err(anyhow!(
                    "AVAssetWriter startWriting failed: {:?}",
                    writer.error()
                ));
            }
            Ok(Self {
                writer,
                input,
                started: false,
                start: None,
            })
        }
    }

    /// Append one screen frame, re-timestamped to wall-clock seconds since the
    /// first frame. SCK's own PTS produced a 2x-inflated, gapped timeline, so we
    /// drive the video clock ourselves — keeping it real-time and aligned with
    /// the audio's length.
    fn append(&mut self, sample: &CMSampleBuffer) {
        let objc_orig: &ObjcCMSampleBuffer =
            unsafe { &*sample.as_concrete_TypeRef().cast::<ObjcCMSampleBuffer>() };

        let elapsed = match self.start {
            Some(t0) => t0.elapsed().as_secs_f64(),
            None => {
                self.start = Some(std::time::Instant::now());
                0.0
            }
        };
        let timing = CMSampleTimingInfo {
            duration: unsafe { kCMTimeInvalid },
            presentationTimeStamp: unsafe { CMTime::with_seconds(elapsed, 600) },
            decodeTimeStamp: unsafe { kCMTimeInvalid },
        };
        let mut out: *mut ObjcCMSampleBuffer = std::ptr::null_mut();
        let status = unsafe {
            ObjcCMSampleBuffer::create_copy_with_new_timing(
                None,
                objc_orig,
                1,
                &timing,
                std::ptr::NonNull::from(&mut out),
            )
        };
        if status != 0 || out.is_null() {
            return;
        }
        unsafe {
            let retimed: &ObjcCMSampleBuffer = &*out;
            if !self.started {
                self.writer.startSessionAtSourceTime(kCMTimeZero);
                self.started = true;
            }
            if self.input.isReadyForMoreMediaData() {
                let _ = self.input.appendSampleBuffer(retimed);
            }
            CFRelease(out as *const _);
        }
    }

    /// Finalize the `.mp4`. The synchronous `finishWriting` is deprecated in
    /// favour of the async completion-handler form, but synchronous is exactly
    /// what the stop path needs — it blocks until the container is flushed and
    /// closed before we mark the meeting completed.
    #[allow(deprecated)]
    fn finish(&mut self) {
        unsafe {
            if self.started {
                self.input.markAsFinished();
                if !self.writer.finishWriting() {
                    warn!(error = ?self.writer.error(), "AVAssetWriter finishWriting failed");
                }
            }
        }
    }
}

/// A running capture: the two SCK streams, the mic thread's stop signal +
/// handle, the shared audio buffers, and the output paths to finalize on stop.
struct Session {
    /// App-scoped, audio only. Deliberately not shared with the watcher: nothing
    /// re-targets it, which is the whole point — window churn can't kill it.
    audio_stream: Arc<Mutex<SCStream>>,
    /// Window-scoped, screen only. Shared so the window watcher can swap in a
    /// fresh stream when the call app changes its window. `stop()` stops
    /// whatever stream is current.
    video_stream: Arc<Mutex<SCStream>>,
    /// Set true to tell the window watcher to exit; joined on stop.
    watcher_stop: Arc<AtomicBool>,
    watcher_thread: Option<JoinHandle<()>>,
    mic_stop: Option<mpsc::Sender<()>>,
    mic_thread: Option<JoinHandle<()>>,
    audio: Arc<Mutex<AudioBuffers>>,
    video: Arc<Mutex<VideoWriter>>,
    audio_path: PathBuf,
    video_path: PathBuf,
    sample_rate: u32,
}

/// Stops the wrapped stream on drop unless [`disarm`](Self::disarm)ed.
///
/// Bringing up a capture is several fallible steps with live SCK streams already
/// running; without this, an error partway through would leave a stream (and its
/// audio tap) capturing for the rest of the process's lifetime, since `stop()`
/// only ever reaches streams that made it into a [`Session`].
struct StreamGuard(Option<Arc<Mutex<SCStream>>>);

impl StreamGuard {
    fn new(stream: &Arc<Mutex<SCStream>>) -> Self {
        Self(Some(stream.clone()))
    }

    /// Hand ownership over to the caller — the stream is now someone else's to
    /// stop (i.e. it reached the `Session`).
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        if let Some(stream) = self.0.take() {
            if let Ok(s) = stream.lock() {
                let _ = s.stop_capture();
            }
        }
    }
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

/// Liveness of the window-scoped video stream, shared between the SCK callbacks
/// and [`spawn_window_watcher`].
struct VideoHealth {
    /// Set when SCK tears the stream down on its own (delegate callback).
    dead: AtomicBool,
    /// Millis since `epoch` at the last `Complete` frame.
    last_frame_ms: AtomicU64,
    /// Generation whose frames the writer currently accepts. Bumped at swap
    /// time, so a replacement stream that is already capturing but not yet
    /// swapped in — and the outgoing one just after — cannot write frames.
    generation: AtomicU64,
    epoch: Instant,
}

impl VideoHealth {
    fn new() -> Self {
        Self {
            dead: AtomicBool::new(false),
            last_frame_ms: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            epoch: Instant::now(),
        }
    }

    fn mark_frame(&self) {
        self.last_frame_ms
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// Time since the last `Complete` frame (since capture start if none yet).
    fn since_last_frame(&self) -> Duration {
        self.epoch.elapsed().saturating_sub(Duration::from_millis(
            self.last_frame_ms.load(Ordering::Relaxed),
        ))
    }
}

/// Audio output handler for the app-scoped stream: accumulates system audio.
struct AudioSink {
    audio: Arc<Mutex<AudioBuffers>>,
}

impl SCStreamOutputTrait for AudioSink {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        match of_type {
            SCStreamOutputType::Audio => {
                if let Some(samples) = system_audio_samples(&sample) {
                    if let Ok(mut buf) = self.audio.lock() {
                        buf.system.extend_from_slice(&samples);
                    }
                }
            }
            SCStreamOutputType::Screen => {}
        }
    }
}

/// Screen output handler for the window-scoped stream: feeds frames to the HEVC
/// writer while this instance's `generation` is the one the watcher installed.
struct VideoSink {
    video: Arc<Mutex<VideoWriter>>,
    health: Arc<VideoHealth>,
    generation: u64,
}

impl SCStreamOutputTrait for VideoSink {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        match of_type {
            SCStreamOutputType::Screen => {
                // A stream built by the watcher starts capturing before it is
                // swapped in; drop its frames until it becomes current so the
                // two never interleave into the writer.
                if self.generation != self.health.generation.load(Ordering::Acquire) {
                    return;
                }
                // Only `Complete` frames carry a valid image buffer. Appending
                // idle/blank/started frames pushes the AVAssetWriter into a
                // failed state, so `finishWriting` later fails to write the
                // `moov` atom and the `.mp4` won't open. Skip the rest.
                let complete = SCStreamFrameInfo::from_sample_buffer(&sample)
                    .map(|info| info.status() == SCFrameStatus::Complete)
                    .unwrap_or(false);
                if complete {
                    if let Ok(mut writer) = self.video.lock() {
                        writer.append(&sample);
                    }
                    self.health.mark_frame();
                }
            }
            SCStreamOutputType::Audio => {}
        }
    }
}

/// Delegate for the window-scoped stream: flag it dead so the watcher rebuilds
/// on its next tick instead of leaving a stopped stream in place.
struct VideoStreamErrors {
    health: Arc<VideoHealth>,
}

impl SCStreamDelegateTrait for VideoStreamErrors {
    fn did_stop_with_error(&self, _stream: SCStream, error: CFError) {
        warn!(error = %error, "video capture stream stopped with an error; will re-target");
        self.health.dead.store(true, Ordering::Release);
    }
}

/// Delegate for the app-scoped audio stream. Nothing rebuilds this one, so a
/// failure here means the far end's audio is lost for the rest of the recording
/// — log it at that weight.
struct AudioStreamErrors;

impl SCStreamDelegateTrait for AudioStreamErrors {
    fn did_stop_with_error(&self, _stream: SCStream, error: CFError) {
        warn!(error = %error, "audio capture stream stopped with an error; system audio is lost for the rest of this recording");
    }
}

/// Pick the call app's main capture window: its largest on-screen,
/// normal-layer (layer 0 — excludes floating panels/menubar items) window.
/// `None` if the app has no such window (then we fall back to display capture).
fn pick_app_window(content: &SCShareableContent, bundle_id: &str) -> Option<SCWindow> {
    content
        .windows()
        .into_iter()
        .filter(|w| {
            w.is_on_screen()
                && w.window_layer() == 0
                && w.owning_application().bundle_identifier() == bundle_id
        })
        .max_by_key(|w| {
            let f = w.get_frame();
            (f.size.width * f.size.height) as i64
        })
}

/// Video stream configuration, shared by the initial capture and every watcher
/// re-target: a fixed `w`×`h` output (so the HEVC writer's frame size never
/// changes when the window moves between monitors or is swapped). Audio is
/// explicitly **off** — it rides the app-scoped stream instead, so that
/// re-targeting this one never touches the audio tap.
fn make_video_config(w: u32, h: u32) -> Result<SCStreamConfiguration> {
    SCStreamConfiguration::new()
        .set_width(w)
        .map_err(|e| anyhow!("set capture width: {e}"))?
        .set_height(h)
        .map_err(|e| anyhow!("set capture height: {e}"))?
        // Cap to VIDEO_FPS (SCK defaults to ~60). minimumFrameInterval is the
        // minimum spacing between frames, i.e. 1/fps.
        .set_minimum_frame_interval(&core_media_rs::cm_time::CMTime {
            value: 1,
            timescale: VIDEO_FPS,
            flags: 1,
            epoch: 0,
        })
        .map_err(|e| anyhow!("set minimum frame interval: {e}"))?
        .set_captures_audio(false)
        .map_err(|e| anyhow!("disable audio on the video stream: {e}"))
}

/// Audio stream configuration: mono at [`CAPTURE_SAMPLE_RATE`], our own output
/// excluded. No screen output handler is attached to this stream, but SCK still
/// wants a valid video config, so keep the frame tiny and slow.
fn make_audio_config() -> Result<SCStreamConfiguration> {
    SCStreamConfiguration::new()
        .set_width(AUDIO_STREAM_DIMS)
        .map_err(|e| anyhow!("set audio stream width: {e}"))?
        .set_height(AUDIO_STREAM_DIMS)
        .map_err(|e| anyhow!("set audio stream height: {e}"))?
        .set_minimum_frame_interval(&core_media_rs::cm_time::CMTime {
            value: 1,
            timescale: 1,
            flags: 1,
            epoch: 0,
        })
        .map_err(|e| anyhow!("set audio stream frame interval: {e}"))?
        .set_captures_audio(true)
        .map_err(|e| anyhow!("enable audio capture: {e}"))?
        .set_sample_rate(CAPTURE_SAMPLE_RATE)
        .map_err(|e| anyhow!("set audio sample rate: {e}"))?
        .set_channel_count(1)
        .map_err(|e| anyhow!("set audio channel count: {e}"))?
        .set_excludes_current_process_audio(true)
        .map_err(|e| anyhow!("exclude own audio: {e}"))
}

/// Build and start the **audio** stream over `filter`, feeding the shared PCM
/// accumulator. Built once per recording; never re-targeted.
fn build_audio_stream(
    filter: &SCContentFilter,
    config: &SCStreamConfiguration,
    audio: &Arc<Mutex<AudioBuffers>>,
) -> Result<SCStream> {
    let mut stream = SCStream::new_with_delegate(filter, config, AudioStreamErrors);
    stream.add_output_handler(
        AudioSink {
            audio: audio.clone(),
        },
        SCStreamOutputType::Audio,
    );
    stream
        .start_capture()
        .map_err(|e| anyhow!("start audio capture: {e}"))?;
    Ok(stream)
}

/// Build and start a **video** stream over `filter`, feeding the HEVC writer.
/// `generation` gates the sink: frames are dropped until the watcher publishes
/// this generation via [`VideoHealth::generation`], so a replacement stream can
/// be brought up while the outgoing one is still running.
fn build_video_stream(
    filter: &SCContentFilter,
    config: &SCStreamConfiguration,
    video: &Arc<Mutex<VideoWriter>>,
    health: &Arc<VideoHealth>,
    generation: u64,
) -> Result<SCStream> {
    let mut stream = SCStream::new_with_delegate(
        filter,
        config,
        VideoStreamErrors {
            health: health.clone(),
        },
    );
    stream.add_output_handler(
        VideoSink {
            video: video.clone(),
            health: health.clone(),
            generation,
        },
        SCStreamOutputType::Screen,
    );
    stream
        .start_capture()
        .map_err(|e| anyhow!("start video capture: {e}"))?;
    Ok(stream)
}

/// Build and start a desktop-independent **window** video stream. Used for the
/// initial start and on every watcher re-target when the call app swaps its
/// window. A window filter follows the window across monitors natively, so
/// dragging the call to another display keeps recording.
fn build_window_stream(
    window: &SCWindow,
    config: &SCStreamConfiguration,
    video: &Arc<Mutex<VideoWriter>>,
    health: &Arc<VideoHealth>,
    generation: u64,
) -> Result<SCStream> {
    let filter = SCContentFilter::new().with_desktop_independent_window(window);
    build_video_stream(&filter, config, video, health, generation)
}

/// Watch the call app's main window and keep the video stream alive on it.
///
/// Three things trigger a re-target:
///
/// * the captured window disappears — the app replaced it (Zoom does this on
///   join, and on screen-share start *and* stop),
/// * SCK tore the stream down and reported it through the delegate
///   ([`VideoHealth::dead`]),
/// * no `Complete` frame arrived for [`FRAME_STALL`] — the silent variant of the
///   same failure, and the one that truncated recordings while nothing watched
///   for it.
///
/// The replacement stream is started *before* the outgoing one is stopped, with
/// the swap published by bumping [`VideoHealth::generation`]. A failed rebuild
/// therefore leaves a working capture running rather than a stopped one, and when
/// the app has no capturable window at this instant the loop just retries on the
/// next tick — it never gives up while the recording is live. Audio is not
/// touched here at all: it lives on its own app-scoped stream. Exits when `stop`
/// is set.
#[allow(clippy::too_many_arguments)]
fn spawn_window_watcher(
    bundle_id: String,
    initial_window_id: u32,
    cap_w: u32,
    cap_h: u32,
    stream: Arc<Mutex<SCStream>>,
    video: Arc<Mutex<VideoWriter>>,
    health: Arc<VideoHealth>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        // 0 means "started without a window" (app-scoped or display fallback);
        // no real window carries id 0, so the first tick looks for one to
        // upgrade to.
        let mut current_id = initial_window_id;
        let mut generation = 0u64;
        loop {
            // WATCH_TICK between checks, but wake often so stop() is prompt.
            let step = Duration::from_millis(100);
            let mut slept = Duration::ZERO;
            while slept < WATCH_TICK {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(step);
                slept += step;
            }
            if stop.load(Ordering::Acquire) {
                return;
            }

            // Consume the dead flag: if the rebuild below fails we want the next
            // tick to retry on the stall check rather than on a stale flag.
            let dead = health.dead.swap(false, Ordering::AcqRel);
            let stalled = health.since_last_frame() > FRAME_STALL;

            let content = match SCShareableContent::get() {
                Ok(c) => c,
                Err(e) => {
                    // Not fatal on its own, but if it keeps failing the watcher
                    // is blind and a dead stream never gets rebuilt — which is
                    // exactly how truncated recordings used to happen unnoticed.
                    warn!(error = %e, "cannot enumerate shareable content; capture is unsupervised this tick");
                    continue;
                }
            };

            // Keep recording the current window as long as it still exists and
            // is actually delivering frames. Only the app closing/replacing it —
            // or the stream itself breaking — triggers a re-target; otherwise a
            // larger *auxiliary* window (Zoom's share toolbar, chat, or
            // participants panel) would steal the capture away from the main
            // meeting/video window the moment it out-sized it.
            let current_present = content
                .windows()
                .into_iter()
                .any(|w| w.window_id() == current_id);
            // This path is the only window into a capture that has gone wrong, so
            // it reports what the decision was made on. Gated on the level being
            // live: it walks and formats the whole window list, which is pure
            // waste at the default INFO.
            if tracing::enabled!(tracing::Level::TRACE) {
                let app_windows: Vec<String> = content
                    .windows()
                    .into_iter()
                    .filter(|w| w.owning_application().bundle_identifier() == bundle_id)
                    .map(|w| {
                        format!(
                            "{}(layer={},onscreen={})",
                            w.window_id(),
                            w.window_layer(),
                            w.is_on_screen()
                        )
                    })
                    .collect();
                trace!(
                    current_id,
                    current_present,
                    dead,
                    stalled,
                    since_last_frame_ms = health.since_last_frame().as_millis(),
                    app_windows = app_windows.join(" "),
                    "watcher tick"
                );
            }
            if current_present && !dead && !stalled {
                continue;
            }

            let win = match pick_app_window(&content, &bundle_id) {
                Some(w) => w,
                // The app has no capturable window at this instant (it is
                // mid-transition). Keep what we have and try again next tick.
                None => {
                    trace!(%bundle_id, "no capturable window yet; retrying next tick");
                    continue;
                }
            };
            let new_id = win.window_id();
            // Same window and a healthy stream: nothing to do. When the stream
            // is dead or stalled we *do* rebuild onto the same window — there
            // the window is fine and the stream is not.
            if new_id == current_id && !dead && !stalled {
                continue;
            }

            // Rebuild the config here: the SCK config type isn't `Send`, so it
            // can't be carried into the thread — but it's cheap to recreate at
            // the fixed output size.
            let config = match make_video_config(cap_w, cap_h) {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "failed to rebuild capture config");
                    continue;
                }
            };

            // Start the replacement first. Its frames are dropped by the sink
            // until its generation is published below, so both streams can be
            // live for a moment without interleaving into the writer.
            let next_generation = generation + 1;
            let fresh = match build_window_stream(&win, &config, &video, &health, next_generation) {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        error = %e,
                        window_id = new_id,
                        "failed to re-target video capture; keeping the current stream"
                    );
                    continue;
                }
            };

            let Ok(mut slot) = stream.lock() else {
                // Poisoned: stop `fresh` rather than leak a running stream.
                let _ = fresh.stop_capture();
                warn!("video stream mutex poisoned; cannot re-target");
                continue;
            };
            let _ = slot.stop_capture();
            *slot = fresh;
            generation = next_generation;
            // Reset staleness before publishing so the fresh stream gets a full
            // FRAME_STALL window to deliver its first frame.
            health.mark_frame();
            health.generation.store(generation, Ordering::Release);
            drop(slot);

            let reason = if dead {
                "stream reported dead"
            } else if stalled {
                "frames stalled"
            } else {
                "window replaced"
            };
            info!(
                from_window_id = current_id,
                window_id = new_id,
                reason,
                "re-targeted video capture"
            );
            current_id = new_id;
        }
    })
}

impl ScreenAudioCapturer for MacosCapturer {
    fn start(
        &self,
        video_path: &Path,
        audio_path: &Path,
        app_bundle_id: Option<&str>,
    ) -> Result<()> {
        let mut guard = self.session.lock().expect("capturer mutex poisoned");
        if guard.is_some() {
            return Err(anyhow!("capture already running"));
        }

        let audio = Arc::new(Mutex::new(AudioBuffers::default()));

        let content =
            SCShareableContent::get().map_err(|e| anyhow!("query shareable content: {e}"))?;
        let displays = content.displays();
        if displays.is_empty() {
            return Err(anyhow!("no display available to capture"));
        }

        // The call app and its main window. The window is the capture target;
        // the app is only the fallback scope when there's no trackable window.
        let app = app_bundle_id.and_then(|bid| {
            content
                .applications()
                .into_iter()
                .find(|a| a.bundle_identifier() == bid)
        });
        let window = app_bundle_id.and_then(|bid| pick_app_window(&content, bid));

        // Output is always 720p tall: width follows the window's aspect (window
        // mode) or the monitor (display fallback). A fixed writer size means the
        // HEVC encoder is undisturbed when the window moves between monitors or
        // is swapped — only the source filter changes, never the frame size.
        const CAP_H: u32 = 720;
        let (cap_w, cap_h) = match &window {
            Some(win) => {
                let f = win.get_frame();
                let aspect = if f.size.height > 1.0 {
                    (f.size.width / f.size.height).clamp(0.2, 5.0)
                } else {
                    16.0 / 9.0
                };
                let mut w = (CAP_H as f64 * aspect).round() as u32;
                w += w & 1; // even dimensions for the HEVC encoder
                (w.max(2), CAP_H)
            }
            None => {
                // No trackable window — pick the primary display for a full
                // capture (downscaled to 720p at mux time).
                let d = &displays[0];
                (d.width(), d.height())
            }
        };

        // HEVC `.mp4` writer for the screen frames, at the fixed output size.
        let video = Arc::new(Mutex::new(
            VideoWriter::new(video_path, cap_w, cap_h).context("create video writer")?,
        ));
        let health = Arc::new(VideoHealth::new());
        let display = &displays[0];

        // --- Audio first: it carries the far end's voice, so it gets the scope
        // that cannot be invalidated by anything the app does to its windows.
        // The filter names the *application*, not a window, so join/share/panel
        // transitions leave it alone — and because it is never re-targeted, the
        // audio tap is negotiated exactly once per recording instead of on every
        // window swap (that renegotiation is what fails with `-3818`).
        let audio_filter = match &app {
            Some(app) => SCContentFilter::new()
                .with_display_including_application_excepting_windows(display, &[app], &[]),
            None => {
                if let Some(bid) = app_bundle_id {
                    warn!(
                        bundle = bid,
                        "call app not in shareable content; capturing system-wide audio"
                    );
                }
                SCContentFilter::new().with_display_excluding_windows(display, &[])
            }
        };
        let audio_stream = Arc::new(Mutex::new(build_audio_stream(
            &audio_filter,
            &make_audio_config()?,
            &audio,
        )?));
        let mut audio_guard = StreamGuard::new(&audio_stream);

        // --- Video second: the call app's **window** when we have one. A
        // desktop-independent window filter follows the window across monitors
        // natively, so dragging the call to another display keeps recording, and
        // it crops tight. It is also the fragile half — the app can swap or
        // destroy the window at any moment — which is why the watcher below owns
        // its lifecycle. A failure here now costs picture only.
        let video_config = make_video_config(cap_w, cap_h)?;
        let video_stream = match &window {
            Some(win) => {
                info!(
                    bundle = %win.owning_application().bundle_identifier(),
                    window_id = win.window_id(),
                    cap_w,
                    cap_h,
                    "capturing the call app window directly"
                );
                build_window_stream(win, &video_config, &video, &health, 0)?
            }
            None => {
                // No trackable window *yet*. Stay scoped to the app when we can —
                // recording the whole desktop is not what this product does — and
                // let the watcher upgrade to the window as soon as one appears.
                let filter = match &app {
                    Some(app) => {
                        warn!(bundle = %app.bundle_identifier(), "no trackable window yet; capturing the app on its display");
                        SCContentFilter::new().with_display_including_application_excepting_windows(
                            display,
                            &[app],
                            &[],
                        )
                    }
                    None => SCContentFilter::new().with_display_excluding_windows(display, &[]),
                };
                build_video_stream(&filter, &video_config, &video, &health, 0)?
            }
        };
        let video_stream = Arc::new(Mutex::new(video_stream));
        let mut video_guard = StreamGuard::new(&video_stream);

        let (mic_stop, mic_thread, sample_rate) = spawn_mic_capture(audio.clone())?;

        // Watcher last: it is the only step that isn't fallible, so nothing can
        // orphan a running thread behind it.
        let watcher_stop = Arc::new(AtomicBool::new(false));
        let watcher_thread = app_bundle_id.map(|bid| {
            spawn_window_watcher(
                bid.to_string(),
                window.as_ref().map_or(0, |w| w.window_id()),
                cap_w,
                cap_h,
                video_stream.clone(),
                video.clone(),
                health.clone(),
                watcher_stop.clone(),
            )
        });

        // Both streams reached the session; it owns stopping them from here.
        audio_guard.disarm();
        video_guard.disarm();

        *guard = Some(Session {
            audio_stream,
            video_stream,
            watcher_stop,
            watcher_thread,
            mic_stop: Some(mic_stop),
            mic_thread: Some(mic_thread),
            audio,
            video,
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

        // Stop the watcher first so it can't swap in a new stream mid-teardown.
        session.watcher_stop.store(true, Ordering::Release);
        if let Some(handle) = session.watcher_thread.take() {
            let _ = handle.join();
        }
        if let Ok(stream) = session.video_stream.lock() {
            if let Err(e) = stream.stop_capture() {
                warn!(error = ?e, "stopping the video capture stream");
            }
        }
        if let Ok(stream) = session.audio_stream.lock() {
            if let Err(e) = stream.stop_capture() {
                warn!(error = ?e, "stopping the audio capture stream");
            }
        }
        if let Some(stop) = session.mic_stop.take() {
            let _ = stop.send(());
        }
        if let Some(handle) = session.mic_thread.take() {
            let _ = handle.join();
        }

        // Finalize the `.mp4` (stops the stream first so no more frames arrive).
        if let Ok(mut writer) = session.video.lock() {
            writer.finish();
        }

        let buffers = session
            .audio
            .lock()
            .map_err(|_| anyhow!("audio buffer poisoned"))?;
        // Mic runs at the device rate; SCK system audio at CAPTURE_SAMPLE_RATE.
        // Write three tracks: the mix (`<stamp>.wav`, also the transcription
        // source) plus separate mic/system sidecars for speaker-aware analysis.
        let mixed = mix_to_mono_16k_dual(
            &buffers.mic,
            session.sample_rate,
            &buffers.system,
            CAPTURE_SAMPLE_RATE,
        );
        // Single channel each: `mix_*_dual` with an empty second source just
        // resamples the first to 16 kHz.
        let mic_only = mix_to_mono_16k_dual(&buffers.mic, session.sample_rate, &[], 1);
        let sys_only = mix_to_mono_16k_dual(&buffers.system, CAPTURE_SAMPLE_RATE, &[], 1);

        let dir = session
            .audio_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        write_wav_16k_mono(&session.audio_path, &mixed).context("write mixed meeting audio")?;
        write_wav_16k_mono(&dir.join("mic.wav"), &mic_only).context("write mic track")?;
        write_wav_16k_mono(&dir.join("system.wav"), &sys_only).context("write system track")?;
        drop(buffers);

        // Mux the mixed audio into the (silent) video synchronously, before
        // `stop()` returns. The finished recording is handed to the upload queue
        // only *after* the recorder stops (`on_recording_stopped` runs once this
        // returns), so the mux must finish here — otherwise the queue registers
        // and uploads the silent capture, racing the mux's rename. The export
        // re-encodes and can take a while, but `stop()` runs on the recorder's
        // background task (driven off the event bus), not the UI thread, so
        // blocking here only delays the upload enqueue — exactly what we want —
        // without hanging the UI. Best-effort: on failure the video stays silent
        // and the WAV sidecars remain.
        match mux_audio_into_video(&session.video_path, &session.audio_path) {
            Ok(()) => info!(video = %session.video_path.display(), "muxed audio into video"),
            Err(err) => warn!(error = %err, "failed to mux audio into video; mp4 stays silent"),
        }

        info!(video = %session.video_path.display(), "macOS capture stopped");
        Ok(())
    }
}

/// Decode a system-audio `CMSampleBuffer` into mono `f32` PCM at
/// [`CAPTURE_SAMPLE_RATE`]. SCK delivers float PCM; we read the first audio
/// buffer (config requests 1 channel) and downmix if it ever arrives multi-channel.
fn system_audio_samples(sample: &CMSampleBuffer) -> Option<Vec<f32>> {
    let list = sample.get_audio_buffer_list().ok()?;
    let buffer = list.buffers().first()?;
    let channels = (buffer.number_channels as usize).max(1);
    let samples: Vec<f32> = buffer
        .data()
        .chunks_exact(std::mem::size_of::<f32>())
        .map(|b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    if channels <= 1 {
        Some(samples)
    } else {
        // Interleaved → mono by averaging each frame's channels.
        Some(
            samples
                .chunks(channels)
                .map(|frame| frame.iter().sum::<f32>() / channels as f32)
                .collect(),
        )
    }
}

/// Mux `audio_path` (the mixed WAV) into `video_path` (video-only HEVC) so the
/// final `.mp4` plays with sound. Builds an `AVMutableComposition` from the two
/// files and exports it. `AVAssetExportSession` has no "encode audio, pass
/// video through" preset, so this re-encodes the video — acceptable for a
/// meeting recording. Best-effort: on failure the silent video is left in place.
// `tracksWithMediaType` is deprecated in favour of the async
// `loadTracksWithMediaType:completionHandler:`, but synchronous is exactly what
// this wants — the whole mux already runs on a background thread and the track
// time ranges are needed inline to build the composition.
#[allow(deprecated)]
fn mux_audio_into_video(video_path: &Path, audio_path: &Path) -> Result<()> {
    use block2::RcBlock;
    use objc2_av_foundation::{
        AVAssetExportPreset960x540, AVAssetExportSession, AVAssetExportSessionStatus,
        AVMediaTypeAudio, AVMutableComposition, AVURLAsset,
    };

    let out_path = video_path.with_extension("muxed.mp4");
    let _ = std::fs::remove_file(&out_path); // export fails if the file exists

    unsafe {
        let url = |p: &Path| NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy()));
        let video_asset = AVURLAsset::URLAssetWithURL_options(&url(video_path), None);
        let audio_asset = AVURLAsset::URLAssetWithURL_options(&url(audio_path), None);

        // Use the track's `timeRange`, not `asset.duration()`: a freshly-created
        // asset's duration is 0 until loaded (synchronously it returns 0 for the
        // HEVC video → an empty insert and a video-less file). Accessing
        // `tracksWithMediaType` loads the tracks, so their time ranges are valid.
        let av_video = AVMediaTypeVideo.ok_or_else(|| anyhow!("AVMediaTypeVideo unavailable"))?;
        let av_audio = AVMediaTypeAudio.ok_or_else(|| anyhow!("AVMediaTypeAudio unavailable"))?;
        let v_track = video_asset
            .tracksWithMediaType(av_video)
            .firstObject()
            .ok_or_else(|| anyhow!("video file has no video track"))?;
        let a_track = audio_asset
            .tracksWithMediaType(av_audio)
            .firstObject()
            .ok_or_else(|| anyhow!("audio file has no audio track"))?;

        // Insert each track into its *own* composition track. The asset-level
        // insertTimeRange:ofAsset:atTime: shifts the whole timeline, so inserting
        // the audio at t=0 pushed the already-inserted video to [audio_len, ..],
        // making the video play in the second half with no overlap.
        let comp = AVMutableComposition::composition();
        let comp_v = comp
            .addMutableTrackWithMediaType_preferredTrackID(av_video, 0)
            .ok_or_else(|| anyhow!("add composition video track"))?;
        comp_v
            .insertTimeRange_ofTrack_atTime_error(v_track.timeRange(), &v_track, kCMTimeZero)
            .map_err(|e| anyhow!("compose video: {e:?}"))?;
        let comp_a = comp
            .addMutableTrackWithMediaType_preferredTrackID(av_audio, 0)
            .ok_or_else(|| anyhow!("add composition audio track"))?;
        comp_a
            .insertTimeRange_ofTrack_atTime_error(a_track.timeRange(), &a_track, kCMTimeZero)
            .map_err(|e| anyhow!("compose audio: {e:?}"))?;

        let export = AVAssetExportSession::initWithAsset_presetName(
            AVAssetExportSession::alloc(),
            &comp,
            // 540p (down from 720p) to roughly halve the file. The export already
            // re-encodes the HEVC capture, so this changes only the output size,
            // not whether a re-encode happens.
            AVAssetExportPreset960x540,
        )
        .ok_or_else(|| anyhow!("create export session"))?;
        let mp4 = AVFileTypeMPEG4.ok_or_else(|| anyhow!("AVFileTypeMPEG4 unavailable"))?;
        export.setOutputURL(Some(&url(&out_path)));
        export.setOutputFileType(Some(mp4));

        // The completion handler is required but we just poll `status`.
        let handler = RcBlock::new(|| {});
        export.exportAsynchronouslyWithCompletionHandler(&handler);
        loop {
            match export.status() {
                AVAssetExportSessionStatus::Completed => break,
                AVAssetExportSessionStatus::Failed | AVAssetExportSessionStatus::Cancelled => {
                    return Err(anyhow!("export failed: {:?}", export.error()));
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
    }

    // Replace the silent capture with the muxed (audio+video) result.
    std::fs::rename(&out_path, video_path).context("replace video with muxed mp4")?;
    Ok(())
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
