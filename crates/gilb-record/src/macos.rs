//! macOS capture backend for [`ScreenAudioCapturer`].
//!
//! This module is compiled only on macOS and is **not** exercised by CI (the
//! workspace is built/tested on Linux). It is the thin, impure edge of the
//! recorder: ScreenCaptureKit captures the call app's window directly (a
//! desktop-independent window filter that follows the window across monitors;
//! see [`build_window_stream`] + [`spawn_window_watcher`]) into an AVAssetWriter
//! encoding HEVC to `.mp4` ([`VideoWriter`]). Audio comes from two sources — the
//! same `SCStream` carries *system* audio ([`system_audio_samples`]) and `cpal`
//! carries the *mic* ([`spawn_mic_capture`]) — both accumulated and, on stop,
//! written via the host-tested helpers in [`crate::mix_to_mono_16k`] /
//! [`crate::write_wav_16k_mono`] as three sidecars: `mic.wav`, `system.wav`, and
//! the `audio.wav` mix. The mix is then muxed into the `.mp4`
//! ([`mux_audio_into_video`]) so the final video plays with sound.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

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
use tracing::{info, warn};

use crate::{mix_to_mono_16k_dual, write_wav_16k_mono, ScreenAudioCapturer};

/// Sample rate requested from ScreenCaptureKit's audio output and the `cpal`
/// mic. Both streams are resampled to 16 kHz by [`mix_to_mono_16k`] on stop.
const CAPTURE_SAMPLE_RATE: u32 = 48_000;

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

/// A running capture: the SCK stream, the mic thread's stop signal + handle,
/// the shared audio buffers, and the output paths to finalize on stop.
struct Session {
    /// Shared so the window watcher can swap in a fresh stream when the call
    /// app changes its window. `stop()` stops whatever stream is current.
    stream: Arc<Mutex<SCStream>>,
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
/// shared accumulator (audio output) and feeds video frames to the HEVC writer
/// (screen output). `video` is `Some` only on the screen-output instance.
struct CaptureSink {
    audio: Arc<Mutex<AudioBuffers>>,
    video: Option<Arc<Mutex<VideoWriter>>>,
}

impl SCStreamOutputTrait for CaptureSink {
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
                if let Some(video) = &self.video {
                    // Only `Complete` frames carry a valid image buffer. Appending
                    // idle/blank/started frames pushes the AVAssetWriter into a
                    // failed state, so `finishWriting` later fails to write the
                    // `moov` atom and the `.mp4` won't open. Skip the rest.
                    let complete = SCStreamFrameInfo::from_sample_buffer(&sample)
                        .map(|info| info.status() == SCFrameStatus::Complete)
                        .unwrap_or(false);
                    if complete {
                        if let Ok(mut writer) = video.lock() {
                            writer.append(&sample);
                        }
                    }
                }
            }
        }
    }
}

/// Minimal stream delegate — SCK reports asynchronous stop/errors through it;
/// log and continue.
struct CaptureErrors;

impl SCStreamDelegateTrait for CaptureErrors {
    fn did_stop_with_error(&self, _stream: SCStream, error: CFError) {
        warn!(error = %error, "ScreenCaptureKit stream stopped with an error");
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

/// Stream configuration shared by the initial capture and every watcher
/// re-target: a fixed `w`×`h` output (so the HEVC writer's frame size never
/// changes when the window moves between monitors or is swapped) plus mono
/// 48 kHz audio.
fn make_capture_config(w: u32, h: u32) -> Result<SCStreamConfiguration> {
    SCStreamConfiguration::new()
        .set_width(w)
        .map_err(|e| anyhow!("set capture width: {e}"))?
        .set_height(h)
        .map_err(|e| anyhow!("set capture height: {e}"))?
        .set_captures_audio(true)
        .map_err(|e| anyhow!("enable audio capture: {e}"))?
        .set_sample_rate(CAPTURE_SAMPLE_RATE)
        .map_err(|e| anyhow!("set audio sample rate: {e}"))?
        .set_channel_count(1)
        .map_err(|e| anyhow!("set audio channel count: {e}"))
}

/// Build and start a desktop-independent **window** capture stream feeding the
/// shared audio/video sinks. Used for the initial start and on every watcher
/// re-target when the call app swaps its window. A window filter follows the
/// window across monitors natively, so dragging the call to another display
/// keeps recording.
fn build_window_stream(
    window: &SCWindow,
    config: &SCStreamConfiguration,
    audio: &Arc<Mutex<AudioBuffers>>,
    video: &Arc<Mutex<VideoWriter>>,
) -> Result<SCStream> {
    let filter = SCContentFilter::new().with_desktop_independent_window(window);
    let mut stream = SCStream::new_with_delegate(&filter, config, CaptureErrors);
    stream.add_output_handler(
        CaptureSink {
            audio: audio.clone(),
            video: None,
        },
        SCStreamOutputType::Audio,
    );
    stream.add_output_handler(
        CaptureSink {
            audio: audio.clone(),
            video: Some(video.clone()),
        },
        SCStreamOutputType::Screen,
    );
    stream
        .start_capture()
        .map_err(|e| anyhow!("start ScreenCaptureKit capture: {e}"))?;
    Ok(stream)
}

/// Watch the call app's main window and re-target the capture when the app
/// swaps it. Window-direct capture follows the window across monitors on its
/// own, but if the app replaces the window (Zoom on join/share/panel) SCK kills
/// the stream — so we poll the window id and, when it changes, stop the old
/// stream and start a fresh one onto the new window, reusing the same sinks and
/// HEVC writer. Exits when `stop` is set.
#[allow(clippy::too_many_arguments)]
fn spawn_window_watcher(
    bundle_id: String,
    initial_window_id: u32,
    cap_w: u32,
    cap_h: u32,
    stream: Arc<Mutex<SCStream>>,
    audio: Arc<Mutex<AudioBuffers>>,
    video: Arc<Mutex<VideoWriter>>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut current_id = initial_window_id;
        loop {
            // ~1.5s between checks, but wake every 100ms so stop() is prompt.
            for _ in 0..15 {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if stop.load(Ordering::Acquire) {
                return;
            }

            let content = match SCShareableContent::get() {
                Ok(c) => c,
                Err(_) => continue,
            };

            // Keep recording the current window as long as it still exists.
            // Only the app actually closing/replacing it triggers a re-target —
            // otherwise a larger *auxiliary* window (Zoom's share toolbar, chat,
            // or participants panel) would steal the capture away from the main
            // meeting/video window the moment it out-sized it.
            let current_present = content
                .windows()
                .into_iter()
                .any(|w| w.window_id() == current_id);
            if current_present {
                continue;
            }

            // The current window is gone — re-target onto its replacement (the
            // largest remaining window of the app, i.e. the new main window).
            let win = match pick_app_window(&content, &bundle_id) {
                Some(w) => w,
                None => continue, // no window right now; keep the old stream
            };
            let new_id = win.window_id();
            if new_id == current_id {
                continue;
            }

            // The app swapped its window. Stop the old stream and start a new
            // one on the new window. Hold the lock across both so no frames
            // arrive from two streams at once. Rebuild the config here: the
            // SCK config type isn't `Send`, so it can't be carried into the
            // thread — but it's cheap to recreate at the fixed output size.
            let config = match make_capture_config(cap_w, cap_h) {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "failed to rebuild capture config");
                    continue;
                }
            };
            if let Ok(mut slot) = stream.lock() {
                let _ = slot.stop_capture();
                match build_window_stream(&win, &config, &audio, &video) {
                    Ok(fresh) => {
                        *slot = fresh;
                        current_id = new_id;
                        info!(
                            window_id = new_id,
                            "re-targeted capture to the app's new window"
                        );
                    }
                    Err(e) => {
                        // Leave the (stopped) old stream in place; retry next tick.
                        warn!(error = %e, "failed to re-target capture window");
                    }
                }
            }
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

        let config = make_capture_config(cap_w, cap_h)?;

        // HEVC `.mp4` writer for the screen frames, at the fixed output size.
        let video = Arc::new(Mutex::new(
            VideoWriter::new(video_path, cap_w, cap_h).context("create video writer")?,
        ));

        // Capture the call app's **window** directly when we found one: a
        // desktop-independent window filter follows the window across monitors
        // natively, so dragging the call to another display keeps recording, and
        // it crops tight. The risk is that the app swaps its window (Zoom does
        // this on join/share/panel) and SCK kills the stream — a watcher thread
        // re-targets the new window to absorb that. With no window, fall back to
        // a full-display capture (can't follow the window, but never vanishes).
        let watcher_stop = Arc::new(AtomicBool::new(false));
        let (stream, watcher_thread) = match &window {
            Some(win) => {
                info!(
                    bundle = %win.owning_application().bundle_identifier(),
                    window_id = win.window_id(),
                    cap_w,
                    cap_h,
                    "capturing the call app window directly"
                );
                let stream = build_window_stream(win, &config, &audio, &video)?;
                let stream = Arc::new(Mutex::new(stream));

                // Watcher: poll the app's main window; if its id changes (the app
                // swapped its window) rebuild the stream onto the new window,
                // reusing the same sinks and writer. Wall-clock retiming in
                // `VideoWriter::append` absorbs the brief gap.
                let thread = spawn_window_watcher(
                    app_bundle_id.unwrap_or_default().to_string(),
                    win.window_id(),
                    cap_w,
                    cap_h,
                    stream.clone(),
                    audio.clone(),
                    video.clone(),
                    watcher_stop.clone(),
                );
                (stream, Some(thread))
            }
            None => {
                let display = &displays[0];
                let filter = match &app {
                    Some(app) => {
                        warn!(bundle = %app.bundle_identifier(), "no trackable window; capturing the app on its display");
                        SCContentFilter::new().with_display_including_application_excepting_windows(
                            display,
                            &[app],
                            &[],
                        )
                    }
                    None => {
                        if let Some(bid) = app_bundle_id {
                            warn!(
                                bundle = bid,
                                "call app not in shareable content; capturing full display"
                            );
                        }
                        SCContentFilter::new().with_display_excluding_windows(display, &[])
                    }
                };
                let mut stream = SCStream::new_with_delegate(&filter, &config, CaptureErrors);
                stream.add_output_handler(
                    CaptureSink {
                        audio: audio.clone(),
                        video: None,
                    },
                    SCStreamOutputType::Audio,
                );
                stream.add_output_handler(
                    CaptureSink {
                        audio: audio.clone(),
                        video: Some(video.clone()),
                    },
                    SCStreamOutputType::Screen,
                );
                stream
                    .start_capture()
                    .map_err(|e| anyhow!("start ScreenCaptureKit capture: {e}"))?;
                (Arc::new(Mutex::new(stream)), None)
            }
        };

        let (mic_stop, mic_thread, sample_rate) = spawn_mic_capture(audio.clone())?;

        *guard = Some(Session {
            stream,
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
        if let Ok(stream) = session.stream.lock() {
            if let Err(e) = stream.stop_capture() {
                warn!(error = ?e, "stopping ScreenCaptureKit stream");
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

        // Mux the mixed audio into the (silent) video on a background thread:
        // the export re-encodes and can take a while, and `stop()` must return
        // promptly so the UI doesn't hang. The mixed wav is already on disk, so
        // the mux just rewrites the mp4 with sound when it finishes.
        let video_path = session.video_path.clone();
        let audio_path = session.audio_path.clone();
        std::thread::spawn(
            move || match mux_audio_into_video(&video_path, &audio_path) {
                Ok(()) => info!(video = %video_path.display(), "muxed audio into video"),
                Err(err) => warn!(error = %err, "failed to mux audio into video; mp4 stays silent"),
            },
        );

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
        AVAssetExportPreset1280x720, AVAssetExportSession, AVAssetExportSessionStatus,
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
            AVAssetExportPreset1280x720,
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
