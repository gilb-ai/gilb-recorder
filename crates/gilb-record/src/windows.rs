//! Windows capture backend for [`ScreenAudioCapturer`].
//!
//! Like `macos.rs`, this module is compiled only on its target OS and is
//! **not** exercised by CI (the workspace is built/tested on Linux). It is
//! the thin, impure edge of the recorder:
//!
//! - **Video** — Windows.Graphics.Capture (WGC) drives the primary monitor
//!   into a `Direct3D11CaptureFramePool`; each frame is copied to a CPU
//!   staging texture and fed to a Media Foundation `SinkWriter` that encodes
//!   hardware/software H.264 into the meeting's `video.mp4`. **Parity gap:**
//!   macOS captures the call *window* and follows it across monitors, but this
//!   backend still captures the whole primary monitor and ignores
//!   `app_bundle_id` — switching to `CreateForWindow` is the follow-up (see
//!   [`WindowsCapturer::start`]).
//! - **Audio** — two WASAPI capture clients (the default render endpoint in
//!   loopback mode for *system* audio, the default capture endpoint for the
//!   *mic*), both forced to mono 16-bit-float at [`CAPTURE_SAMPLE_RATE`] via
//!   `AUTOCONVERTPCM`, are accumulated and, on stop, written as three 16 kHz
//!   mono WAVs — the `audio.wav` mix plus `mic.wav` / `system.wav` sidecars —
//!   through the host-tested helpers in [`crate::mix_to_mono_16k_dual`] /
//!   [`crate::write_wav_16k_mono`], byte-compatible with the macOS sidecars.
//!
//! The COM idioms (MTA threads, `Interface::cast`, `windows = "0.58"`) mirror
//! the WASAPI meeting detector in `gilb-meeting/src/wasapi.rs`. As there, the
//! whole live COM chain is the hardening target of a follow-up iteration and
//! is smoke-tested by hand on a Windows host; everything host-testable (path
//! derivation, the audio mix/resample, the WAV writer) is shared with the
//! pure, tested core in `lib.rs`.

use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Mutex;
use std::sync::{Arc, Mutex as StdMutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use tracing::{info, warn};

use gilb_meeting::allowlist::process_names_for_bundle_id;
use windows::core::{factory, IInspectable, Interface, Result as WinResult, PCWSTR, PWSTR};
use windows::Foundation::{EventRegistrationToken, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{CloseHandle, BOOL, HMODULE, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_FLAG,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_RESOURCE_MISC_FLAG, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::Media::MediaFoundation::{
    IMFSample, IMFSinkWriter, IMFSourceReader, MFAudioFormat_AAC, MFAudioFormat_PCM,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL,
    MFCreateSourceReaderFromURL, MFMediaType_Audio, MFMediaType_Video, MFShutdown, MFStartup,
    MFVideoFormat_H264, MFVideoFormat_RGB32, MFVideoInterlace_Progressive, MFSTARTUP_FULL,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_AVG_BITRATE,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowTextLengthW, GetWindowThreadProcessId, IsIconic, IsWindow,
    IsWindowVisible,
};

use crate::{mix_to_mono_16k_dual, write_wav_16k_mono, ScreenAudioCapturer};

/// Sample rate both WASAPI clients are forced to (via `AUTOCONVERTPCM`) so the
/// two mono streams line up for [`mix_to_mono_16k`], which resamples 48 kHz
/// down to the 16 kHz sidecar rate. Mirrors macOS's `CAPTURE_SAMPLE_RATE`.
const CAPTURE_SAMPLE_RATE: u32 = 48_000;

/// Target H.264 frame rate for the WGC -> SinkWriter pipeline.
const VIDEO_FPS: u32 = 30;

/// Nominal per-frame duration in 100 ns units (used for sample durations;
/// presentation times come from each frame's `SystemRelativeTime`).
const FRAME_DURATION_100NS: i64 = 10_000_000 / VIDEO_FPS as i64;

/// Average target bitrate for the H.264 stream (8 Mbit/s).
const VIDEO_BITRATE: u32 = 8_000_000;

/// WASAPI shared-mode buffer duration in 100 ns units (1 s).
const AUDIO_BUFFER_DURATION_100NS: i64 = 10_000_000;

/// `AUDCLNT_BUFFERFLAGS_SILENT` — the capture buffer is silence; skip the copy
/// and push zeros to keep the timeline aligned.
const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;

/// `WAVE_FORMAT_IEEE_FLOAT` tag for the requested 32-bit-float `WAVEFORMATEX`.
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;

/// Shared, mono-`f32` PCM accumulators for the two audio sources. Both are at
/// [`CAPTURE_SAMPLE_RATE`]; [`mix_to_mono_16k`] resamples and mixes on stop.
#[derive(Default)]
struct AudioBuffers {
    mic: Vec<f32>,
    system: Vec<f32>,
}

/// A running capture: the shared stop flag, the capture thread handles, the
/// shared audio buffers, and the audio output path to finalize on stop. (The
/// `.mp4` is finalized by the video thread itself when it observes the flag.)
struct Session {
    running: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    audio: Arc<Mutex<AudioBuffers>>,
    audio_path: PathBuf,
    video_path: PathBuf,
}

/// Windows [`ScreenAudioCapturer`]. Holds the active [`Session`] behind a mutex
/// so the trait stays `Send + Sync` (the engine drives it from a spawned task).
#[derive(Default)]
pub struct WindowsCapturer {
    session: Mutex<Option<Session>>,
}

impl WindowsCapturer {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ScreenAudioCapturer for WindowsCapturer {
    // `app_bundle_id` scopes the screen capture to the call app's window (parity
    // with the macOS backend): the video thread resolves it to an `HWND` and
    // captures that window via `CreateForWindow` (monitor-independent, so the
    // call can be dragged across displays), falling back to the primary monitor
    // when no window is found.
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

        let running = Arc::new(AtomicBool::new(true));
        let audio = Arc::new(Mutex::new(AudioBuffers::default()));

        // System audio (render endpoint, loopback) and mic (capture endpoint)
        // each run a blocking WASAPI poll loop on a dedicated MTA thread.
        let system_thread = spawn_audio_thread(false, audio.clone(), running.clone());
        let mic_thread = spawn_audio_thread(true, audio.clone(), running.clone());

        // Screen capture + H.264 encode runs on its own MTA thread, finalizing
        // the `.mp4` when `running` clears. A video-side failure is logged but
        // does not abort the (independent) audio capture.
        let video_thread = spawn_video_thread(
            video_path.to_path_buf(),
            app_bundle_id.map(str::to_string),
            running.clone(),
        );

        *guard = Some(Session {
            running,
            threads: vec![system_thread, mic_thread, video_thread],
            audio,
            audio_path: audio_path.to_path_buf(),
            video_path: video_path.to_path_buf(),
        });
        info!(video = %video_path.display(), "Windows capture started");
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        let session = self
            .session
            .lock()
            .expect("capturer mutex poisoned")
            .take()
            .ok_or_else(|| anyhow!("capture is not running"))?;

        // Signal all three threads, then wait for them to tear down their COM
        // chains (and the video thread to finalize the `.mp4`).
        session.running.store(false, Ordering::Relaxed);
        for handle in session.threads {
            let _ = handle.join();
        }

        let buffers = session
            .audio
            .lock()
            .map_err(|_| anyhow!("audio buffer poisoned"))?;
        // Both WASAPI clients run at CAPTURE_SAMPLE_RATE. Write three tracks to
        // match the macOS backend: the mix (`audio.wav`, also the transcription
        // source) plus separate mic/system sidecars for speaker-aware analysis.
        // `mix_*_dual` with an empty second source just resamples the first.
        let mixed = mix_to_mono_16k_dual(
            &buffers.mic,
            CAPTURE_SAMPLE_RATE,
            &buffers.system,
            CAPTURE_SAMPLE_RATE,
        );
        let mic_only = mix_to_mono_16k_dual(&buffers.mic, CAPTURE_SAMPLE_RATE, &[], 1);
        let sys_only = mix_to_mono_16k_dual(&buffers.system, CAPTURE_SAMPLE_RATE, &[], 1);

        let dir = session
            .audio_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        write_wav_16k_mono(&session.audio_path, &mixed).context("write mixed meeting audio")?;
        write_wav_16k_mono(&dir.join("mic.wav"), &mic_only).context("write mic track")?;
        write_wav_16k_mono(&dir.join("system.wav"), &sys_only).context("write system track")?;

        // Mix for the mp4 audio track, kept at the native 48 kHz capture rate:
        // the Media Foundation AAC encoder only accepts 44.1/48 kHz, not the
        // 16 kHz sidecar rate. Both sources are already mono at CAPTURE_SAMPLE_RATE
        // and index-aligned, so sum-and-clamp per sample.
        let n = buffers.mic.len().max(buffers.system.len());
        let mut mix48 = Vec::with_capacity(n);
        for i in 0..n {
            let m = buffers.mic.get(i).copied().unwrap_or(0.0);
            let s = buffers.system.get(i).copied().unwrap_or(0.0);
            mix48.push(((m + s).clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16);
        }
        drop(buffers);

        // Mux the mixed audio into the (silent) `.mp4` on a background thread so
        // stop() returns promptly (parity with the macOS backend). Best-effort:
        // on failure the video stays silent and the WAV sidecars remain.
        let video_path = session.video_path.clone();
        thread::spawn(move || {
            if let Err(err) = mux_audio_into_video(&video_path, &mix48, CAPTURE_SAMPLE_RATE) {
                warn!(error = %err, "failed to mux audio into video; mp4 stays silent");
            } else {
                info!(video = %video_path.display(), "muxed audio into video");
            }
        });

        info!("Windows capture stopped");
        Ok(())
    }
}

/// Spawn a WASAPI capture thread for the mic (`is_mic = true`, capture
/// endpoint) or system audio (`is_mic = false`, render endpoint in loopback),
/// pushing mono `f32` samples into the shared accumulator until `running`
/// clears. Capture failures are logged and end the thread without aborting
/// the rest of the recording.
fn spawn_audio_thread(
    is_mic: bool,
    audio: Arc<Mutex<AudioBuffers>>,
    running: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        if let Err(err) = capture_audio(is_mic, &audio, &running) {
            warn!(is_mic, error = %err, "WASAPI capture thread failed");
        }
    })
}

/// Blocking WASAPI capture loop. Initializes COM on this thread, opens the
/// default endpoint for the requested direction, forces a mono float format at
/// [`CAPTURE_SAMPLE_RATE`], and drains capture packets into `audio` until
/// `running` clears.
fn capture_audio(
    is_mic: bool,
    audio: &Arc<Mutex<AudioBuffers>>,
    running: &AtomicBool,
) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .context("CoInitializeEx (audio)")?;

        let result = capture_audio_inner(is_mic, audio, running);
        CoUninitialize();
        result
    }
}

unsafe fn capture_audio_inner(
    is_mic: bool,
    audio: &Arc<Mutex<AudioBuffers>>,
    running: &AtomicBool,
) -> Result<()> {
    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("MMDeviceEnumerator")?;
    let flow = if is_mic { eCapture } else { eRender };
    let device = enumerator
        .GetDefaultAudioEndpoint(flow, eConsole)
        .context("GetDefaultAudioEndpoint")?;
    let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .context("Activate IAudioClient")?;

    // Mono 32-bit float at the common rate; AUTOCONVERTPCM makes the engine
    // resample/downmix the endpoint to match regardless of its native format.
    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: 1,
        nSamplesPerSec: CAPTURE_SAMPLE_RATE,
        nAvgBytesPerSec: CAPTURE_SAMPLE_RATE * 4,
        nBlockAlign: 4,
        wBitsPerSample: 32,
        cbSize: 0,
    };

    let mut flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    if !is_mic {
        flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
    }
    client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            AUDIO_BUFFER_DURATION_100NS,
            0,
            &format,
            None,
        )
        .context("IAudioClient::Initialize")?;

    let capture: IAudioCaptureClient = client
        .GetService()
        .context("GetService IAudioCaptureClient")?;
    client.Start().context("IAudioClient::Start")?;

    while running.load(Ordering::Relaxed) {
        let mut packet = capture.GetNextPacketSize().context("GetNextPacketSize")?;
        while packet != 0 {
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut buffer_flags = 0u32;
            capture
                .GetBuffer(&mut data, &mut frames, &mut buffer_flags, None, None)
                .context("IAudioCaptureClient::GetBuffer")?;

            if frames > 0 {
                let mut chunk = vec![0f32; frames as usize];
                if buffer_flags & AUDCLNT_BUFFERFLAGS_SILENT == 0 && !data.is_null() {
                    let src = std::slice::from_raw_parts(data as *const f32, frames as usize);
                    chunk.copy_from_slice(src);
                }
                if let Ok(mut buf) = audio.lock() {
                    if is_mic {
                        buf.mic.extend_from_slice(&chunk);
                    } else {
                        buf.system.extend_from_slice(&chunk);
                    }
                }
            }

            capture.ReleaseBuffer(frames).context("ReleaseBuffer")?;
            packet = capture.GetNextPacketSize().context("GetNextPacketSize")?;
        }
        thread::sleep(Duration::from_millis(10));
    }

    client.Stop().context("IAudioClient::Stop")?;
    Ok(())
}

/// Spawn the screen-capture + H.264 encode thread. Like the audio threads it
/// owns its COM lifetime; failures are logged and end the thread.
fn spawn_video_thread(
    video_path: PathBuf,
    app_bundle_id: Option<String>,
    running: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        if let Err(err) = capture_video(&video_path, app_bundle_id.as_deref(), &running) {
            warn!(error = %err, "WGC/Media Foundation video thread failed");
        }
    })
}

/// One H.264 frame ready to encode: tightly packed BGRA bytes plus its
/// presentation time in 100 ns units (relative to the first frame).
type VideoFrame = (Vec<u8>, i64);

/// Set up WGC + Media Foundation, encode frames until `running` clears, then
/// finalize the `.mp4`. COM is initialized for the lifetime of this thread.
fn capture_video(
    video_path: &Path,
    app_bundle_id: Option<&str>,
    running: &AtomicBool,
) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .context("CoInitializeEx (video)")?;
        MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")?;

        let result = capture_video_inner(video_path, app_bundle_id, running);

        let _ = MFShutdown();
        CoUninitialize();
        result
    }
}

unsafe fn capture_video_inner(
    video_path: &Path,
    app_bundle_id: Option<&str>,
    running: &AtomicBool,
) -> Result<()> {
    if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
        return Err(anyhow!(
            "Windows.Graphics.Capture is not supported on this host"
        ));
    }

    // D3D11 device (BGRA support is required for WGC) and its WinRT wrapper.
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
    )
    .context("D3D11CreateDevice")?;
    let device = device.ok_or_else(|| anyhow!("D3D11CreateDevice returned no device"))?;
    let context = context.ok_or_else(|| anyhow!("D3D11CreateDevice returned no context"))?;

    let dxgi: IDXGIDevice = device.cast().context("ID3D11Device as IDXGIDevice")?;
    let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)
        .context("CreateDirect3D11DeviceFromDXGIDevice")?;
    let d3d_winrt: IDirect3DDevice = inspectable.cast().context("IDirect3DDevice cast")?;

    let interop: IGraphicsCaptureItemInterop =
        factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .context("GraphicsCaptureItem interop factory")?;

    // Capture target: the call app's window when we can resolve one (a window
    // item follows the window across monitors), else the primary monitor as a
    // fallback.
    let mut current_hwnd = app_bundle_id.and_then(find_app_window);
    let item: GraphicsCaptureItem = match current_hwnd {
        Some(hwnd) => {
            info!("capturing the call app window directly");
            interop
                .CreateForWindow(hwnd)
                .context("IGraphicsCaptureItemInterop::CreateForWindow")?
        }
        None => {
            if let Some(bundle) = app_bundle_id {
                warn!(
                    bundle,
                    "no call-app window found; capturing the primary monitor"
                );
            }
            let hmonitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            interop
                .CreateForMonitor(hmonitor)
                .context("IGraphicsCaptureItemInterop::CreateForMonitor")?
        }
    };

    let size = item.Size().context("GraphicsCaptureItem::Size")?;
    // Fixed encoder size: 720p tall, width from the capture aspect (parity with
    // macOS). Frames are scaled into this on the CPU, so a window resize or swap
    // never disturbs the encoder.
    let (out_w, out_h) = fixed_720p_dims(size.Width.max(0) as u32, size.Height.max(0) as u32);
    if out_w == 0 || out_h == 0 {
        return Err(anyhow!("capture item reported a zero size"));
    }

    let (writer, stream_index) = create_sink_writer(video_path, out_w, out_h)?;

    // Shared sinks, reused across watcher re-targets: the frame channel, the
    // staging-texture cache (recreated when the source size changes), and the
    // first-frame epoch for monotonic presentation times.
    let (frame_tx, frame_rx) = std_mpsc::channel::<VideoFrame>();
    let staging: Arc<StdMutex<Option<StagingTex>>> = Arc::new(StdMutex::new(None));
    let start_time: Arc<StdMutex<Option<i64>>> = Arc::new(StdMutex::new(None));

    // `Option` so the watcher can `take()` it to stop the old capture before
    // rebuilding (LiveCapture::stop consumes self), without leaving a moved value.
    let mut live = Some(start_capture_for_item(
        &d3d_winrt,
        &device,
        &context,
        &item,
        &staging,
        &start_time,
        &frame_tx,
        out_w,
        out_h,
    )?);

    // Drain frames and write them as they arrive; between frames poll `running`
    // and, in window mode, re-target if the captured window has gone away.
    let mut last_window_check = Instant::now();
    while running.load(Ordering::Relaxed) {
        match frame_rx.recv_timeout(Duration::from_millis(200)) {
            Ok((data, time_100ns)) => {
                write_video_sample(&writer, stream_index, &data, time_100ns)?;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Watcher (window mode only): if the captured window is gone, the app
        // swapped it (Teams/Zoom on join/share/panel) — re-target the new main
        // window onto the same writer. Only a *missing* window triggers this, so
        // an auxiliary window can't steal the capture from the main one.
        if current_hwnd.is_some() && last_window_check.elapsed() >= Duration::from_secs(1) {
            last_window_check = Instant::now();
            let gone = current_hwnd
                .map(|h| !IsWindow(h).as_bool())
                .unwrap_or(false);
            if gone {
                if let Some(new_hwnd) = app_bundle_id.and_then(find_app_window) {
                    match interop.CreateForWindow(new_hwnd) {
                        Ok(new_item) => {
                            if let Some(old) = live.take() {
                                old.stop();
                            }
                            match start_capture_for_item(
                                &d3d_winrt,
                                &device,
                                &context,
                                &new_item,
                                &staging,
                                &start_time,
                                &frame_tx,
                                out_w,
                                out_h,
                            ) {
                                Ok(fresh) => {
                                    live = Some(fresh);
                                    current_hwnd = Some(new_hwnd);
                                    info!("re-targeted capture to the app's new window");
                                }
                                Err(err) => {
                                    warn!(error = %err, "failed to re-target capture window");
                                    break;
                                }
                            }
                        }
                        Err(err) => warn!(error = %err, "CreateForWindow on re-target failed"),
                    }
                }
            }
        }
    }

    // Stop capture, then flush any frames already queued before finalizing.
    if let Some(l) = live.take() {
        l.stop();
    }
    while let Ok((data, time_100ns)) = frame_rx.try_recv() {
        write_video_sample(&writer, stream_index, &data, time_100ns)?;
    }
    writer.Finalize().context("IMFSinkWriter::Finalize")?;
    Ok(())
}

/// A live WGC capture for one item: the frame pool, the running session, the
/// frame-arrived delegate (kept alive until teardown), and its registration
/// token. [`stop`](LiveCapture::stop) tears the chain down; the watcher builds a
/// fresh one when the call app swaps its window.
struct LiveCapture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    handler: TypedEventHandler<Direct3D11CaptureFramePool, IInspectable>,
    token: EventRegistrationToken,
}

impl LiveCapture {
    /// Stop and release the capture chain. Unhook the frame handler first so no
    /// more callbacks fire, then close the session (which stops the capture —
    /// `GraphicsCaptureSession` has no explicit stop) and the pool.
    unsafe fn stop(self) {
        let _ = self.pool.RemoveFrameArrived(self.token);
        drop(self.handler);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// Cached CPU-readable staging texture plus the source size it was built for, so
/// it can be recreated when a resize or window swap changes the frame size.
struct StagingTex {
    tex: ID3D11Texture2D,
    width: u32,
    height: u32,
}

/// Build a free-threaded frame pool for `item`, hook `on_frame` onto it, and
/// start the capture session — feeding the shared sinks. The free-threaded pool
/// fires `FrameArrived` on a pool-owned MTA thread; the handler maps each frame
/// and forwards packed, scaled BGRA over `frame_tx` to the encoder loop.
#[allow(clippy::too_many_arguments)]
unsafe fn start_capture_for_item(
    d3d_winrt: &IDirect3DDevice,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    item: &GraphicsCaptureItem,
    staging: &Arc<StdMutex<Option<StagingTex>>>,
    start_time: &Arc<StdMutex<Option<i64>>>,
    frame_tx: &std_mpsc::Sender<VideoFrame>,
    out_w: u32,
    out_h: u32,
) -> Result<LiveCapture> {
    let size = item.Size().context("GraphicsCaptureItem::Size")?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        d3d_winrt,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )
    .context("Direct3D11CaptureFramePool::CreateFreeThreaded")?;

    let handler = TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new({
        let pool = pool.clone();
        let device = device.clone();
        let context = context.clone();
        let staging = staging.clone();
        let start_time = start_time.clone();
        let frame_tx = frame_tx.clone();
        move |_sender, _args| -> WinResult<()> {
            // Errors inside the callback are logged, not propagated, so a single
            // bad frame never tears the capture session down.
            if let Err(err) = on_frame(
                &pool,
                &device,
                &context,
                &staging,
                &start_time,
                &frame_tx,
                out_w,
                out_h,
            ) {
                warn!(error = %err, "dropping WGC frame");
            }
            Ok(())
        }
    });
    let token = pool
        .FrameArrived(&handler)
        .context("Direct3D11CaptureFramePool::FrameArrived")?;

    let session = pool
        .CreateCaptureSession(item)
        .context("Direct3D11CaptureFramePool::CreateCaptureSession")?;
    session
        .StartCapture()
        .context("GraphicsCaptureSession::StartCapture")?;

    Ok(LiveCapture {
        pool,
        session,
        handler,
        token,
    })
}

/// Fixed encoder dimensions for a capture of native size `w`×`h`: 720p tall with
/// the width following the aspect ratio (even, for the H.264 encoder). Mirrors
/// the macOS backend's fixed 720p capture config.
fn fixed_720p_dims(w: u32, h: u32) -> (u32, u32) {
    const OUT_H: u32 = 720;
    if h == 0 {
        return (1280, OUT_H);
    }
    let aspect = (w as f32 / h as f32).clamp(0.2, 5.0);
    let mut out_w = (OUT_H as f32 * aspect).round() as u32;
    out_w += out_w & 1; // even dimensions for the encoder
    (out_w.max(2), OUT_H)
}

/// Resolve the call app's main capture window from its bundle id: the largest
/// visible, non-minimized, titled top-level window owned by a process whose exe
/// matches one of the app's known executables. `None` if no such window exists
/// (then capture falls back to the primary monitor). Mirrors the macOS
/// `pick_app_window`.
fn find_app_window(bundle_id: &str) -> Option<HWND> {
    let targets = process_names_for_bundle_id(bundle_id);
    if targets.is_empty() {
        return None;
    }
    let mut search = WindowSearch {
        targets: &targets,
        best: None,
        best_area: 0,
    };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut search as *mut _ as isize));
    }
    search.best
}

/// Mutable state threaded through [`enum_proc`] via the `EnumWindows` `LPARAM`.
struct WindowSearch<'a> {
    targets: &'a [&'a str],
    best: Option<HWND>,
    best_area: i64,
}

/// `EnumWindows` callback: keep the largest visible, titled, non-minimized
/// top-level window whose owning process exe is in `targets`.
unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = &mut *(lparam.0 as *mut WindowSearch);
    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        return BOOL(1);
    }
    if GetWindowTextLengthW(hwnd) == 0 {
        return BOOL(1);
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 {
        return BOOL(1);
    }
    let Some(name) = process_basename(pid) else {
        return BOOL(1);
    };
    if !search.targets.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
        return BOOL(1);
    }
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_ok() {
        let area = (rect.right - rect.left) as i64 * (rect.bottom - rect.top) as i64;
        if area > search.best_area {
            search.best_area = area;
            search.best = Some(hwnd);
        }
    }
    BOOL(1)
}

/// Resolve a PID to its exe basename (lowercase comparison happens at the call
/// site). `None` if the process can't be opened or queried.
unsafe fn process_basename(pid: u32) -> Option<String> {
    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = [0u16; 260];
    let mut len = buf.len() as u32;
    let res = QueryFullProcessImageNameW(
        handle,
        PROCESS_NAME_WIN32,
        PWSTR(buf.as_mut_ptr()),
        &mut len,
    );
    let _ = CloseHandle(handle);
    res.ok()?;
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    Some(path.rsplit(['\\', '/']).next().unwrap_or(&path).to_string())
}

/// FrameArrived body: pull the next frame, copy its surface to a CPU-readable
/// staging texture (recreated when the source size changes), map it to packed
/// BGRA, scale it into the fixed `out_w`×`out_h` encoder size, and forward the
/// bytes + presentation time over `frame_tx`.
#[allow(clippy::too_many_arguments)]
unsafe fn on_frame(
    pool: &Direct3D11CaptureFramePool,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    staging: &Arc<StdMutex<Option<StagingTex>>>,
    start_time: &Arc<StdMutex<Option<i64>>>,
    frame_tx: &std_mpsc::Sender<VideoFrame>,
    out_w: u32,
    out_h: u32,
) -> Result<()> {
    let frame = pool.TryGetNextFrame().context("TryGetNextFrame")?;
    let surface = frame.Surface().context("Direct3D11CaptureFrame::Surface")?;
    let access: IDirect3DDxgiInterfaceAccess = surface
        .cast()
        .context("surface as IDirect3DDxgiInterfaceAccess")?;
    let texture: ID3D11Texture2D = access
        .GetInterface()
        .context("GetInterface ID3D11Texture2D")?;

    let mut desc = D3D11_TEXTURE2D_DESC::default();
    texture.GetDesc(&mut desc);
    let src_w = desc.Width;
    let src_h = desc.Height;

    // (Re)create the staging texture when the source size changes — a window
    // resize or a swap to a differently-sized window.
    {
        let mut guard = staging.lock().unwrap();
        let stale = guard
            .as_ref()
            .map(|s| s.width != src_w || s.height != src_h)
            .unwrap_or(true);
        if stale {
            let mut sdesc = desc;
            sdesc.Usage = D3D11_USAGE_STAGING;
            sdesc.BindFlags = D3D11_BIND_FLAG(0);
            sdesc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
            sdesc.MiscFlags = D3D11_RESOURCE_MISC_FLAG(0);
            let mut tex: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&sdesc, None, Some(&mut tex))
                .context("CreateTexture2D (staging)")?;
            *guard = tex.map(|tex| StagingTex {
                tex,
                width: src_w,
                height: src_h,
            });
        }
    }
    let staging_tex = staging
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.tex.clone())
        .ok_or_else(|| anyhow!("staging texture missing"))?;

    context.CopyResource(&staging_tex, &texture);

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    context
        .Map(&staging_tex, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        .context("ID3D11DeviceContext::Map")?;

    let row_bytes = (src_w * 4) as usize;
    let mut packed = vec![0u8; row_bytes * src_h as usize];
    let src = mapped.pData as *const u8;
    for row in 0..src_h as usize {
        let s = src.add(row * mapped.RowPitch as usize);
        let d = packed.as_mut_ptr().add(row * row_bytes);
        std::ptr::copy_nonoverlapping(s, d, row_bytes);
    }
    context.Unmap(&staging_tex, 0);

    // Scale the captured window into the fixed encoder size.
    let data = crate::scale_bgra(
        &packed,
        src_w as usize,
        src_h as usize,
        out_w as usize,
        out_h as usize,
    );

    let t = frame
        .SystemRelativeTime()
        .context("Direct3D11CaptureFrame::SystemRelativeTime")?
        .Duration;
    let sample_time = {
        let mut start = start_time.lock().unwrap();
        t - *start.get_or_insert(t)
    };

    let _ = frame.Close();
    let _ = frame_tx.send((data, sample_time));
    Ok(())
}

/// Build an `IMFSinkWriter` for `path` with an H.264 output stream fed from an
/// RGB32 (BGRA) input type. Returns the writer and its stream index, ready for
/// `WriteSample`.
unsafe fn create_sink_writer(path: &Path, width: u32, height: u32) -> Result<(IMFSinkWriter, u32)> {
    let url: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
    let writer = MFCreateSinkWriterFromURL(PCWSTR(url.as_ptr()), None, None)
        .context("MFCreateSinkWriterFromURL")?;

    let frame_size = pack_u32_pair(width, height);
    let frame_rate = pack_u32_pair(VIDEO_FPS, 1);
    let aspect_ratio = pack_u32_pair(1, 1);

    // H.264 output type.
    let output = MFCreateMediaType().context("MFCreateMediaType (output)")?;
    output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
    output.SetUINT32(&MF_MT_AVG_BITRATE, VIDEO_BITRATE)?;
    output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    output.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
    output.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)?;
    output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, aspect_ratio)?;
    let stream_index = writer
        .AddStream(&output)
        .context("IMFSinkWriter::AddStream")?;

    // RGB32 (BGRA, top-down) input type matching the WGC surface format.
    let input = MFCreateMediaType().context("MFCreateMediaType (input)")?;
    input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
    input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width * 4)?;
    input.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
    input.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)?;
    input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, aspect_ratio)?;
    writer
        .SetInputMediaType(stream_index, &input, None)
        .context("IMFSinkWriter::SetInputMediaType")?;

    writer
        .BeginWriting()
        .context("IMFSinkWriter::BeginWriting")?;
    Ok((writer, stream_index))
}

/// Wrap packed BGRA `data` in an MF sample and write it to `writer`.
unsafe fn write_video_sample(
    writer: &IMFSinkWriter,
    stream_index: u32,
    data: &[u8],
    time_100ns: i64,
) -> Result<()> {
    let buffer = MFCreateMemoryBuffer(data.len() as u32).context("MFCreateMemoryBuffer")?;
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut max_len = 0u32;
    buffer
        .Lock(&mut ptr, Some(&mut max_len), None)
        .context("IMFMediaBuffer::Lock")?;
    std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
    buffer.SetCurrentLength(data.len() as u32)?;
    buffer.Unlock()?;

    let sample = MFCreateSample().context("MFCreateSample")?;
    sample.AddBuffer(&buffer)?;
    sample.SetSampleTime(time_100ns)?;
    sample.SetSampleDuration(FRAME_DURATION_100NS)?;
    writer
        .WriteSample(stream_index, &sample)
        .context("IMFSinkWriter::WriteSample")?;
    Ok(())
}

/// Pack two `u32`s into the `hi << 32 | lo` `u64` Media Foundation uses for its
/// ratio/size attributes (frame size, frame rate, pixel aspect ratio).
fn pack_u32_pair(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

/// Mux the mono PCM `pcm` (at `rate` Hz) into the silent H.264 `video_path` as
/// an AAC track, producing a playable `.mp4`. Mirrors the macOS post-mux: the
/// video is copied through unchanged (no re-encode) while the audio is encoded
/// to AAC alongside it, then the muxed file replaces the silent one. Runs on its
/// own thread, so it owns its COM + Media Foundation lifetime. `rate` must be a
/// rate the MF AAC encoder accepts (44.1/48 kHz) — the caller passes the native
/// 48 kHz capture rate, not the 16 kHz sidecar rate.
fn mux_audio_into_video(video_path: &Path, pcm: &[i16], rate: u32) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .context("CoInitializeEx (mux)")?;
        MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup (mux)")?;
        let result = mux_inner(video_path, pcm, rate);
        let _ = MFShutdown();
        CoUninitialize();
        result
    }
}

unsafe fn mux_inner(video_path: &Path, pcm: &[i16], rate: u32) -> Result<()> {
    let out_path = video_path.with_extension("muxed.mp4");
    let _ = std::fs::remove_file(&out_path); // a stale file would fail the writer

    // Source reader for the silent H.264 video. Selecting the native media type
    // as the current type makes it deliver the encoded samples unchanged (no
    // decode), so the video is remuxed rather than re-encoded.
    let in_url: Vec<u16> = video_path
        .as_os_str()
        .encode_wide()
        .chain(once(0))
        .collect();
    let reader: IMFSourceReader = MFCreateSourceReaderFromURL(PCWSTR(in_url.as_ptr()), None)
        .context("MFCreateSourceReaderFromURL")?;
    let v_stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    reader
        .SetStreamSelection(v_stream, true)
        .context("SetStreamSelection (video)")?;
    let video_type = reader
        .GetNativeMediaType(v_stream, 0)
        .context("GetNativeMediaType")?;
    reader
        .SetCurrentMediaType(v_stream, None, &video_type)
        .context("SetCurrentMediaType (video passthrough)")?;

    // Sink writer: the same H.264 video stream plus an AAC audio stream fed from
    // 16-bit PCM at the capture rate.
    let out_url: Vec<u16> = out_path.as_os_str().encode_wide().chain(once(0)).collect();
    let writer = MFCreateSinkWriterFromURL(PCWSTR(out_url.as_ptr()), None, None)
        .context("MFCreateSinkWriterFromURL (mux)")?;

    let v_index = writer.AddStream(&video_type).context("AddStream (video)")?;
    writer
        .SetInputMediaType(v_index, &video_type, None)
        .context("SetInputMediaType (video passthrough)")?;

    let aac = MFCreateMediaType().context("MFCreateMediaType (aac)")?;
    aac.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    aac.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
    aac.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
    aac.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
    aac.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 1)?;
    aac.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 16_000)?; // ~128 kbps
    let a_index = writer.AddStream(&aac).context("AddStream (audio)")?;

    let pcm_type = MFCreateMediaType().context("MFCreateMediaType (pcm)")?;
    pcm_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    pcm_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
    pcm_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
    pcm_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
    pcm_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 1)?;
    pcm_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 2)?;
    pcm_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * 2)?;
    writer
        .SetInputMediaType(a_index, &pcm_type, None)
        .context("SetInputMediaType (pcm)")?;

    writer.BeginWriting().context("BeginWriting (mux)")?;

    // Copy every encoded video sample through unchanged.
    loop {
        let mut flags = 0u32;
        let mut sample: Option<IMFSample> = None;
        reader
            .ReadSample(
                v_stream,
                0,
                None,
                Some(&mut flags as *mut u32),
                None,
                Some(&mut sample as *mut Option<IMFSample>),
            )
            .context("ReadSample (video)")?;
        if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            break;
        }
        if let Some(sample) = sample {
            writer
                .WriteSample(v_index, &sample)
                .context("WriteSample (video)")?;
        }
    }

    // Encode the PCM to AAC in fixed chunks, timestamped from the sample index.
    const CHUNK: usize = 1024;
    let mut i = 0usize;
    while i < pcm.len() {
        let end = (i + CHUNK).min(pcm.len());
        let slice = &pcm[i..end];
        let byte_len = slice.len() * 2;
        let buffer =
            MFCreateMemoryBuffer(byte_len as u32).context("MFCreateMemoryBuffer (audio)")?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len = 0u32;
        buffer
            .Lock(&mut ptr, Some(&mut max_len), None)
            .context("IMFMediaBuffer::Lock (audio)")?;
        std::ptr::copy_nonoverlapping(slice.as_ptr() as *const u8, ptr, byte_len);
        buffer.SetCurrentLength(byte_len as u32)?;
        buffer.Unlock()?;

        let sample = MFCreateSample().context("MFCreateSample (audio)")?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime((i as i64) * 10_000_000 / rate as i64)?;
        sample.SetSampleDuration((slice.len() as i64) * 10_000_000 / rate as i64)?;
        writer
            .WriteSample(a_index, &sample)
            .context("WriteSample (audio)")?;
        i = end;
    }

    writer.Finalize().context("IMFSinkWriter::Finalize (mux)")?;
    std::fs::rename(&out_path, video_path).context("replace video with muxed mp4")?;
    Ok(())
}
