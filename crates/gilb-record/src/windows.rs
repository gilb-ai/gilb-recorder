//! Windows capture backend for [`ScreenAudioCapturer`].
//!
//! Like `macos.rs`, this module is compiled only on its target OS and is
//! **not** exercised by CI (the workspace is built/tested on Linux). It is
//! the thin, impure edge of the recorder:
//!
//! - **Video** — Windows.Graphics.Capture (WGC) drives the primary monitor
//!   into a `Direct3D11CaptureFramePool`; each frame is copied to a CPU
//!   staging texture and fed to a Media Foundation `SinkWriter` that encodes
//!   hardware/software H.264 into `<id>.mp4`.
//! - **Audio** — two WASAPI capture clients (the default render endpoint in
//!   loopback mode for *system* audio, the default capture endpoint for the
//!   *mic*), both forced to mono 16-bit-float at [`CAPTURE_SAMPLE_RATE`] via
//!   `AUTOCONVERTPCM`, are accumulated and, on stop, mixed to a 16 kHz mono
//!   WAV through the host-tested helpers in [`crate::mix_to_mono_16k`] /
//!   [`crate::write_wav_16k_mono`] — byte-compatible with the macOS sidecar.
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
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tracing::{info, warn};

use windows::core::{factory, IInspectable, Interface, Result as WinResult, PCWSTR};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HMODULE, POINT};
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
    IMFSinkWriter, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFCreateSinkWriterFromURL, MFMediaType_Video, MFShutdown, MFStartup, MFVideoFormat_H264,
    MFVideoFormat_RGB32, MFVideoInterlace_Progressive, MFSTARTUP_FULL, MF_MT_AVG_BITRATE,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::{mix_to_mono_16k, write_wav_16k_mono, ScreenAudioCapturer};

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
    fn start(&self, video_path: &Path, audio_path: &Path) -> Result<()> {
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
        let video_thread = spawn_video_thread(video_path.to_path_buf(), running.clone());

        *guard = Some(Session {
            running,
            threads: vec![system_thread, mic_thread, video_thread],
            audio,
            audio_path: audio_path.to_path_buf(),
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
        let mixed = mix_to_mono_16k(&buffers.mic, &buffers.system, CAPTURE_SAMPLE_RATE);
        write_wav_16k_mono(&session.audio_path, &mixed).context("write meeting audio")?;

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
fn spawn_video_thread(video_path: PathBuf, running: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::spawn(move || {
        if let Err(err) = capture_video(&video_path, &running) {
            warn!(error = %err, "WGC/Media Foundation video thread failed");
        }
    })
}

/// One H.264 frame ready to encode: tightly packed BGRA bytes plus its
/// presentation time in 100 ns units (relative to the first frame).
type VideoFrame = (Vec<u8>, i64);

/// Set up WGC + Media Foundation, encode frames until `running` clears, then
/// finalize the `.mp4`. COM is initialized for the lifetime of this thread.
fn capture_video(video_path: &Path, running: &AtomicBool) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .context("CoInitializeEx (video)")?;
        MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")?;

        let result = capture_video_inner(video_path, running);

        let _ = MFShutdown();
        CoUninitialize();
        result
    }
}

unsafe fn capture_video_inner(video_path: &Path, running: &AtomicBool) -> Result<()> {
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

    // Capture item for the primary monitor, via the Win32 interop factory.
    let hmonitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
    let interop: IGraphicsCaptureItemInterop =
        factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .context("GraphicsCaptureItem interop factory")?;
    let item: GraphicsCaptureItem = interop
        .CreateForMonitor(hmonitor)
        .context("IGraphicsCaptureItemInterop::CreateForMonitor")?;
    let size = item.Size().context("GraphicsCaptureItem::Size")?;
    let width = size.Width.max(0) as u32;
    let height = size.Height.max(0) as u32;
    if width == 0 || height == 0 {
        return Err(anyhow!(
            "primary monitor reported a zero-sized capture item"
        ));
    }

    let (writer, stream_index) = create_sink_writer(video_path, width, height)?;

    // Free-threaded frame pool: FrameArrived fires on a pool-owned MTA thread.
    // The handler does the GPU copy + CPU map and forwards packed BGRA frames
    // over a channel to this thread, which owns and drives the SinkWriter.
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &d3d_winrt,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )
    .context("Direct3D11CaptureFramePool::CreateFreeThreaded")?;

    let (frame_tx, frame_rx) = std_mpsc::channel::<VideoFrame>();
    let staging: Arc<StdMutex<Option<ID3D11Texture2D>>> = Arc::new(StdMutex::new(None));
    let start_time: Arc<StdMutex<Option<i64>>> = Arc::new(StdMutex::new(None));

    let handler = TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new({
        let pool = pool.clone();
        let device = device.clone();
        let context = context.clone();
        let staging = staging.clone();
        let start_time = start_time.clone();
        let frame_tx = frame_tx.clone();
        move |_sender, _args| -> WinResult<()> {
            // Errors inside the callback are logged, not propagated, so a
            // single bad frame never tears the capture session down.
            if let Err(err) = on_frame(
                &pool,
                &device,
                &context,
                &staging,
                &start_time,
                &frame_tx,
                width,
                height,
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
        .CreateCaptureSession(&item)
        .context("Direct3D11CaptureFramePool::CreateCaptureSession")?;
    session
        .StartCapture()
        .context("GraphicsCaptureSession::StartCapture")?;

    // Drop our spare sender so the channel closes once the handler's clone is
    // released at teardown.
    drop(frame_tx);

    // Drain frames and write them as they arrive, polling `running` between.
    while running.load(Ordering::Relaxed) {
        match frame_rx.recv_timeout(Duration::from_millis(200)) {
            Ok((data, time_100ns)) => {
                write_video_sample(&writer, stream_index, &data, time_100ns)?;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Stop capture, unhook the handler (releasing its sender), then flush any
    // frames already queued before finalizing the file.
    session
        .StopCapture()
        .context("GraphicsCaptureSession::StopCapture")?;
    let _ = pool.RemoveFrameArrived(token);
    drop(handler);
    while let Ok((data, time_100ns)) = frame_rx.try_recv() {
        write_video_sample(&writer, stream_index, &data, time_100ns)?;
    }
    writer.Finalize().context("IMFSinkWriter::Finalize")?;
    let _ = session.Close();
    let _ = pool.Close();
    Ok(())
}

/// FrameArrived body: pull the next frame, copy its surface texture to a
/// CPU-readable staging texture, map it to tightly packed BGRA, and forward
/// the bytes + presentation time over `frame_tx`.
#[allow(clippy::too_many_arguments)]
unsafe fn on_frame(
    pool: &Direct3D11CaptureFramePool,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    staging: &Arc<StdMutex<Option<ID3D11Texture2D>>>,
    start_time: &Arc<StdMutex<Option<i64>>>,
    frame_tx: &std_mpsc::Sender<VideoFrame>,
    width: u32,
    height: u32,
) -> Result<()> {
    let frame = pool.TryGetNextFrame().context("TryGetNextFrame")?;
    let surface = frame.Surface().context("Direct3D11CaptureFrame::Surface")?;
    let access: IDirect3DDxgiInterfaceAccess = surface
        .cast()
        .context("surface as IDirect3DDxgiInterfaceAccess")?;
    let texture: ID3D11Texture2D = access
        .GetInterface()
        .context("GetInterface ID3D11Texture2D")?;

    // Lazily create the staging texture from the source description.
    {
        let mut guard = staging.lock().unwrap();
        if guard.is_none() {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = D3D11_BIND_FLAG(0);
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
            desc.MiscFlags = D3D11_RESOURCE_MISC_FLAG(0);
            let mut tex: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut tex))
                .context("CreateTexture2D (staging)")?;
            *guard = tex;
        }
    }
    let staging_tex = staging
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| anyhow!("staging texture missing"))?;

    context.CopyResource(&staging_tex, &texture);

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    context
        .Map(&staging_tex, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        .context("ID3D11DeviceContext::Map")?;

    let row_bytes = (width * 4) as usize;
    let mut data = vec![0u8; row_bytes * height as usize];
    let src = mapped.pData as *const u8;
    for row in 0..height as usize {
        let s = src.add(row * mapped.RowPitch as usize);
        let d = data.as_mut_ptr().add(row * row_bytes);
        std::ptr::copy_nonoverlapping(s, d, row_bytes);
    }
    context.Unmap(&staging_tex, 0);

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
