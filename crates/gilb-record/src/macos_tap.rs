//! System-audio capture via a Core Audio **process tap** (macOS 14.2+).
//!
//! The SCK route to system audio runs through `replayd`, and that service is
//! where recordings have actually died: the `-3818` start failure was a race
//! inside `replayd`'s audio-session bookkeeping, and a mid-call stream death
//! there cost an hour of far-end audio. A process tap talks to `coreaudiod`
//! directly — `replayd` is simply not in the path. Every local-first recorder
//! we surveyed that ships serious audio (OpenOats, prismical, meetily) captures
//! system audio this way; SCK remains our fallback, not the primary.
//!
//! The capture pattern is Apple's documented API flow: describe the tap
//! ([`CATapDescription`], mono mixdown of all processes), create it, wrap it in
//! a private aggregate device whose tap list references the tap's UUID, attach
//! an IO proc to that device, and start it. Teardown runs in reverse.
//!
//! **Availability.** The two tap functions are macOS 14.2+, and the app's
//! deployment target is lower — linking them directly would abort launch on
//! older systems before `main`. They are therefore resolved with `dlsym` at
//! runtime ([`TapFns::resolve`]); on systems without them [`SystemAudioTap::start`]
//! fails fast and the caller falls back to SCK. The `CATapDescription` class is
//! only touched after that check succeeds (objc2 looks classes up lazily, so
//! merely compiling it in is safe).
//!
//! **Permission.** Taps require the Audio Recording TCC grant
//! (`NSAudioCaptureUsageDescription`), which is distinct from Screen Recording.
//! The first tap creation prompts the user; a denial surfaces as a create error
//! and, again, the SCK fallback takes over.

use std::ffi::c_void;
use std::ptr::NonNull;

use anyhow::{anyhow, Result};
use block2::RcBlock;
use core_foundation::array::CFArray;
use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::AllocAnyThread;
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey,
    AudioDeviceCreateIOProcIDWithBlock, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareDestroyAggregateDevice, AudioObjectGetPropertyData, AudioObjectID,
    AudioObjectPropertyAddress, CATapDescription,
};
use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp, AudioTimeStampFlags};
use objc2_core_foundation::CFDictionary as CFDictionaryObjc2;
use objc2_foundation::{NSArray, NSString};
use tracing::{info, warn};

/// `kAudioObjectUnknown` — the null audio object id.
const UNKNOWN_OBJECT: AudioObjectID = 0;

/// `kAudioDevicePropertyNominalSampleRate` ('nsrt') — not surfaced by the
/// bindings crate, so spelled as its FourCC.
const NOMINAL_SAMPLE_RATE: u32 = u32::from_be_bytes(*b"nsrt");
/// `kAudioObjectPropertyScopeGlobal` ('glob').
const SCOPE_GLOBAL: u32 = u32::from_be_bytes(*b"glob");
/// `kAudioObjectPropertyElementMain`.
const ELEMENT_MAIN: u32 = 0;

/// The two tap entry points, resolved at runtime (see module docs).
struct TapFns {
    create: unsafe extern "C-unwind" fn(*const c_void, *mut AudioObjectID) -> i32,
    destroy: unsafe extern "C-unwind" fn(AudioObjectID) -> i32,
}

impl TapFns {
    fn resolve() -> Option<Self> {
        // CoreAudio is already loaded (cpal and the rest of this crate use it),
        // so a global-namespace lookup finds the symbols when they exist.
        let load =
            |name: &std::ffi::CStr| unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
        let create = load(c"AudioHardwareCreateProcessTap");
        let destroy = load(c"AudioHardwareDestroyProcessTap");
        if create.is_null() || destroy.is_null() {
            return None;
        }
        // SAFETY: the symbols come from CoreAudio itself; the signatures are the
        // published C prototypes.
        unsafe {
            Some(Self {
                create: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C-unwind" fn(*const c_void, *mut AudioObjectID) -> i32,
                >(create),
                destroy: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C-unwind" fn(AudioObjectID) -> i32,
                >(destroy),
            })
        }
    }
}

/// Gap bookkeeping for the IO block.
///
/// A tap delivers buffers **only while some process is rendering audio** —
/// measured directly: a continuous tone yields wall-clock-complete samples, but
/// total silence yields *zero* callbacks. Appending chunks naively would splice
/// every silent stretch out of the recording and shift the system channel's
/// timeline ahead of the mic by the length of all silence so far — wrecking
/// speaker attribution. So silence is reconstructed: the leading gap from the
/// wall clock (device sample time has no epoch we can anchor before the first
/// callback), every later gap sample-accurately from the device's own
/// `mSampleTime`.
struct TapClock {
    started: std::time::Instant,
    sample_rate: f64,
    /// Device sample time right after the previous chunk, once one arrived.
    next_expected_sample: Option<f64>,
}

/// Pad `gap_frames` of silence into `sink` in bounded chunks (a long silent
/// stretch would otherwise materialize as one enormous allocation).
fn pad_silence(sink: &impl Fn(&[f32]), gap_frames: usize) {
    const CHUNK: usize = 48_000;
    let zeros = [0.0f32; CHUNK];
    let mut remaining = gap_frames;
    while remaining > 0 {
        let n = remaining.min(CHUNK);
        sink(&zeros[..n]);
        remaining -= n;
    }
}

/// A running system-audio tap: tap object + private aggregate device + IO proc.
/// Mono `f32` sample chunks are delivered to the sink closure from a dedicated
/// dispatch queue. Dropping stops and destroys everything in reverse order.
pub struct SystemAudioTap {
    fns: TapFns,
    tap: AudioObjectID,
    aggregate: AudioObjectID,
    io_proc: AudioDeviceIOProcID,
    sample_rate: u32,
    /// Keeps the IO block (and the sink it owns) alive for the device's sake.
    #[allow(clippy::type_complexity)]
    _block: RcBlock<
        dyn Fn(
            NonNull<AudioTimeStamp>,
            NonNull<AudioBufferList>,
            NonNull<AudioTimeStamp>,
            NonNull<AudioBufferList>,
            NonNull<AudioTimeStamp>,
        ),
    >,
    /// The queue the IO block runs on; must outlive the IO proc.
    _queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: all control-plane access (start/stop/drop) is serialized behind the
// capturer's session mutex, exactly like `VideoWriter`. The IO block itself is
// only ever invoked by Core Audio on `_queue`.
unsafe impl Send for SystemAudioTap {}

impl SystemAudioTap {
    /// Whether this system has the tap API at all (macOS 14.2+).
    pub fn is_supported() -> bool {
        TapFns::resolve().is_some()
    }

    /// Create and start a global mono tap, delivering samples to `sink`.
    ///
    /// The tap is global (all processes' output) rather than scoped to the call
    /// app: per-process scoping keys on the app's process set, and that set
    /// churning mid-call is precisely the fragility this migration removes.
    /// During a meeting, "everything the machine plays" and "the call" are the
    /// same thing for transcription purposes.
    pub fn start(sink: impl Fn(&[f32]) + Send + 'static) -> Result<Self> {
        let fns = TapFns::resolve()
            .ok_or_else(|| anyhow!("process-tap API unavailable (needs macOS 14.2+)"))?;

        // --- Tap: mono mixdown of every process, excluding none. Our own
        // process plays nothing, so there is nothing to exclude.
        let desc = unsafe {
            CATapDescription::initMonoGlobalTapButExcludeProcesses(
                CATapDescription::alloc(),
                &NSArray::new(),
            )
        };
        unsafe { desc.setName(&NSString::from_str("gilb system-audio tap")) };
        let tap_uuid = unsafe { desc.UUID().UUIDString().to_string() };

        let mut tap: AudioObjectID = UNKNOWN_OBJECT;
        let status = unsafe {
            (fns.create)(
                Retained::as_ptr(&desc) as *const c_void,
                &mut tap as *mut AudioObjectID,
            )
        };
        if status != 0 || tap == UNKNOWN_OBJECT {
            return Err(anyhow!("AudioHardwareCreateProcessTap failed: {status}"));
        }
        // From here on, every failure must destroy what exists so far.
        let fail = |fns: &TapFns, tap, aggregate: Option<AudioObjectID>, err: anyhow::Error| {
            if let Some(agg) = aggregate {
                unsafe { AudioHardwareDestroyAggregateDevice(agg) };
            }
            unsafe { (fns.destroy)(tap) };
            Err(err)
        };

        // --- Private aggregate device wrapping the tap. Private keeps it out
        // of the user's device lists; drift compensation lets coreaudiod own
        // clock differences between the tap and the device clock.
        let cf_str = |s: &std::ffi::CStr| CFString::new(s.to_str().expect("ascii key"));
        let sub_tap = CFDictionary::from_CFType_pairs(&[
            (
                cf_str(kAudioSubTapUIDKey).as_CFType(),
                CFString::new(&tap_uuid).as_CFType(),
            ),
            (
                cf_str(kAudioSubTapDriftCompensationKey).as_CFType(),
                CFNumber::from(1i32).as_CFType(),
            ),
        ]);
        let description = CFDictionary::from_CFType_pairs(&[
            (
                cf_str(kAudioAggregateDeviceUIDKey).as_CFType(),
                CFString::new(&format!("app.farol.gilb.tap.{tap_uuid}")).as_CFType(),
            ),
            (
                cf_str(kAudioAggregateDeviceNameKey).as_CFType(),
                CFString::new("gilb system audio").as_CFType(),
            ),
            (
                cf_str(kAudioAggregateDeviceIsPrivateKey).as_CFType(),
                CFNumber::from(1i32).as_CFType(),
            ),
            (
                cf_str(kAudioAggregateDeviceTapAutoStartKey).as_CFType(),
                CFNumber::from(1i32).as_CFType(),
            ),
            (
                cf_str(kAudioAggregateDeviceTapListKey).as_CFType(),
                CFArray::from_CFTypes(&[sub_tap.as_CFType()]).as_CFType(),
            ),
        ]);

        let mut aggregate: AudioObjectID = UNKNOWN_OBJECT;
        let status = unsafe {
            AudioHardwareCreateAggregateDevice(
                // The bindings crate names the same CFDictionary type from
                // objc2-core-foundation; the underlying object is identical.
                &*description
                    .as_concrete_TypeRef()
                    .cast::<CFDictionaryObjc2>(),
                NonNull::from(&mut aggregate),
            )
        };
        if status != 0 || aggregate == UNKNOWN_OBJECT {
            return fail(
                &fns,
                tap,
                None,
                anyhow!("AudioHardwareCreateAggregateDevice failed: {status}"),
            );
        }

        let sample_rate = read_sample_rate(aggregate).unwrap_or_else(|err| {
            warn!(error = %err, "cannot read tap sample rate; assuming 48kHz");
            48_000
        });

        // --- IO proc: pull mono f32 out of the input buffer list, padding any
        // silence gap first (see `TapClock`). The tap description guarantees
        // mono float, but downmix defensively.
        // Mutex only to satisfy the block's `Fn` bound; the serial queue means
        // it is never contended.
        let clock = std::sync::Mutex::new(TapClock {
            started: std::time::Instant::now(),
            sample_rate: sample_rate as f64,
            next_expected_sample: None,
        });
        let block = RcBlock::new(
            move |_now: NonNull<AudioTimeStamp>,
                  in_data: NonNull<AudioBufferList>,
                  in_time: NonNull<AudioTimeStamp>,
                  _out_data: NonNull<AudioBufferList>,
                  _out_time: NonNull<AudioTimeStamp>| {
                let Ok(mut clock) = clock.lock() else {
                    return;
                };
                let ts = unsafe { in_time.as_ref() };
                let sample_time = ts
                    .mFlags
                    .contains(AudioTimeStampFlags::SampleTimeValid)
                    .then_some(ts.mSampleTime);

                // Reconstruct the silence this chunk arrives after. Guarded to
                // a day so a clock discontinuity can't spin the pad loop.
                const MAX_GAP_FRAMES: f64 = 48_000.0 * 60.0 * 60.0 * 24.0;
                let gap_frames = match (clock.next_expected_sample, sample_time) {
                    // Mid-stream: sample-accurate from the device clock.
                    (Some(expected), Some(now)) => now - expected,
                    // First chunk: whatever wall time elapsed before any
                    // process rendered audio was pure silence.
                    (None, _) => clock.started.elapsed().as_secs_f64() * clock.sample_rate,
                    // Device clock unreadable: append as-is.
                    (Some(_), None) => 0.0,
                };
                if (1.0..=MAX_GAP_FRAMES).contains(&gap_frames) {
                    pad_silence(&sink, gap_frames as usize);
                } else if gap_frames > MAX_GAP_FRAMES {
                    warn!(gap_frames, "implausible tap clock jump; not padding");
                }

                let list = unsafe { in_data.as_ref() };
                let buffers = unsafe {
                    std::slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize)
                };
                let mut frames_delivered = 0usize;
                for buf in buffers {
                    if buf.mData.is_null() || buf.mDataByteSize == 0 {
                        continue;
                    }
                    let samples = unsafe {
                        std::slice::from_raw_parts(
                            buf.mData as *const f32,
                            buf.mDataByteSize as usize / std::mem::size_of::<f32>(),
                        )
                    };
                    let channels = (buf.mNumberChannels as usize).max(1);
                    if channels == 1 {
                        sink(samples);
                        frames_delivered += samples.len();
                    } else {
                        let mono: Vec<f32> = samples
                            .chunks(channels)
                            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
                            .collect();
                        sink(&mono);
                        frames_delivered += mono.len();
                    }
                }
                if let Some(now) = sample_time {
                    clock.next_expected_sample = Some(now + frames_delivered as f64);
                }
            },
        );

        // `None` attribute = a serial queue.
        let queue = DispatchQueue::new("app.farol.gilb.system-tap", None);
        let mut io_proc: AudioDeviceIOProcID = None;
        let status = unsafe {
            AudioDeviceCreateIOProcIDWithBlock(
                NonNull::from(&mut io_proc),
                aggregate,
                Some(&queue),
                RcBlock::as_ptr(&block) as _,
            )
        };
        if status != 0 || io_proc.is_none() {
            return fail(
                &fns,
                tap,
                Some(aggregate),
                anyhow!("AudioDeviceCreateIOProcIDWithBlock failed: {status}"),
            );
        }
        let status = unsafe { AudioDeviceStart(aggregate, io_proc) };
        if status != 0 {
            unsafe { AudioDeviceDestroyIOProcID(aggregate, io_proc) };
            return fail(
                &fns,
                tap,
                Some(aggregate),
                anyhow!("AudioDeviceStart failed: {status}"),
            );
        }

        info!(
            tap,
            aggregate, sample_rate, "system-audio process tap started"
        );
        Ok(Self {
            fns,
            tap,
            aggregate,
            io_proc,
            sample_rate,
            _block: block,
            _queue: queue,
        })
    }

    /// Nominal sample rate the tap delivers at.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

impl Drop for SystemAudioTap {
    fn drop(&mut self) {
        unsafe {
            AudioDeviceStop(self.aggregate, self.io_proc);
            AudioDeviceDestroyIOProcID(self.aggregate, self.io_proc);
            AudioHardwareDestroyAggregateDevice(self.aggregate);
            (self.fns.destroy)(self.tap);
        }
    }
}

/// Nominal sample rate of `device`, via `AudioObjectGetPropertyData`.
fn read_sample_rate(device: AudioObjectID) -> Result<u32> {
    let address = AudioObjectPropertyAddress {
        mSelector: NOMINAL_SAMPLE_RATE,
        mScope: SCOPE_GLOBAL,
        mElement: ELEMENT_MAIN,
    };
    let mut rate: f64 = 0.0;
    let mut size = std::mem::size_of::<f64>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut rate).cast(),
        )
    };
    if status != 0 || rate <= 0.0 {
        return Err(anyhow!("nominal sample rate query failed: {status}"));
    }
    Ok(rate as u32)
}
