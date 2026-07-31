//! Does the microphone deliver anything at all?
//!
//! Splits "gilb is not recording the mic" into its two very different causes:
//! the OS handing us silence (permission, or a device that is not really
//! there), and our own capture code dropping what it gets. Opens the default
//! input exactly the way the recorder does and reports what arrives.
//!
//! ```sh
//! cargo run -p gilb-record --example mic_probe
//! ```

/// `kAudioHardwarePropertyRunLoop = NULL` — let CoreAudio own the thread that
/// delivers HAL notifications, instead of whichever thread happened to make the
/// first call.
#[cfg(target_os = "macos")]
fn unbind_hal_run_loop() {
    use objc2_core_audio::{
        kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
        AudioObjectPropertyAddress, AudioObjectSetPropertyData,
    };
    // 'glob' = kAudioHardwarePropertyRunLoop
    const RUN_LOOP: u32 = u32::from_be_bytes(*b"glob");
    let address = AudioObjectPropertyAddress {
        mSelector: RUN_LOOP,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let null: *const std::ffi::c_void = std::ptr::null();
    // SAFETY: system object, documented property, one pointer-sized value.
    let status = unsafe {
        AudioObjectSetPropertyData(
            1, // kAudioObjectSystemObject
            std::ptr::NonNull::from(&address),
            0,
            std::ptr::null(),
            std::mem::size_of::<*const std::ffi::c_void>() as u32,
            std::ptr::NonNull::from(&null).cast(),
        )
    };
    if status != 0 {
        println!("setting the run loop failed: OSStatus {status}");
    }
}

fn main() {
    #[cfg(not(target_os = "macos"))]
    println!("macOS only");

    #[cfg(target_os = "macos")]
    {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        use std::sync::atomic::{AtomicU64, AtomicU64 as Peak, Ordering};
        use std::sync::Arc;

        // Candidate fix: hand the HAL its own notification thread. Creating the
        // tap binds CoreAudio's run loop to whichever thread got there first;
        // an AudioUnit created on a *different* thread then waits on a run loop
        // nobody runs. Setting the property to NULL is Apple's documented way
        // of saying "manage it yourself".
        if std::env::args().any(|a| a == "unbind-runloop") {
            unbind_hal_run_loop();
            println!("HAL run loop released to CoreAudio");
        }

        // The recorder opens the mic *after* installing a system-audio process
        // tap. If that ordering is what silences the microphone, this argument
        // reproduces it in isolation.
        let _tap = if std::env::args().any(|a| a == "with-tap") {
            match gilb_record::macos_tap::SystemAudioTap::start(|_| {}) {
                Ok(tap) => {
                    println!("system-audio process tap installed first");
                    Some(tap)
                }
                Err(e) => {
                    println!("could not install the tap: {e}");
                    None
                }
            }
        } else {
            None
        };

        let host = cpal::default_host();
        let Some(device) = host.default_input_device() else {
            println!("no default input device");
            return;
        };
        println!("device: {}", device.name().unwrap_or_else(|_| "?".into()));

        let config = match device.default_input_config() {
            Ok(c) => c,
            Err(e) => {
                println!("default_input_config failed: {e}");
                return;
            }
        };
        println!(
            "config: {} Hz, {} ch, {:?}",
            config.sample_rate().0,
            config.channels(),
            config.sample_format()
        );

        let samples = Arc::new(AtomicU64::new(0));
        // Loudest sample seen, scaled — an all-zero stream is the OS politely
        // handing us silence, which looks nothing like a quiet room.
        let peak: Arc<Peak> = Arc::new(AtomicU64::new(0));
        let (s, p) = (samples.clone(), peak.clone());

        // The recorder builds the stream on a spawned thread that then parks on
        // a channel. Reproduce that shape too — a stream is not `Send`, so
        // where it is created is where it lives.
        // Does the *order* decide it? Start the mic thread first, install the
        // tap second — the reverse of what the recorder does.
        if std::env::args().any(|a| a == "mic-first") {
            println!("mic thread first, tap second");
            let (s2, p2) = (samples.clone(), peak.clone());
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let cfg = config.clone();
            let handle = std::thread::spawn(move || {
                let stream = device
                    .build_input_stream(
                        &cfg.into(),
                        move |data: &[f32], _: &cpal::InputCallbackInfo| {
                            s2.fetch_add(data.len() as u64, Ordering::Relaxed);
                            let loudest = data.iter().fold(0.0f32, |m, x| m.max(x.abs()));
                            p2.fetch_max((loudest * 10_000.0) as u64, Ordering::Relaxed);
                        },
                        |e| println!("stream error: {e}"),
                        None,
                    )
                    .expect("build on thread");
                stream.play().expect("play on thread");
                let _ = rx.recv();
                drop(stream);
            });
            std::thread::sleep(std::time::Duration::from_millis(300));
            let tap = gilb_record::macos_tap::SystemAudioTap::start(|_| {});
            println!(
                "tap after mic: {}",
                if tap.is_ok() { "installed" } else { "failed" }
            );
            std::thread::sleep(std::time::Duration::from_secs(5));
            let _ = tx.send(());
            let _ = handle.join();
            let n = samples.load(Ordering::Relaxed);
            println!("samples: {n}");
            println!(
                "{}",
                if n == 0 {
                    "STILL NOTHING"
                } else {
                    "AUDIO OK — order is the fix"
                }
            );
            return;
        }

        if std::env::args().any(|a| a == "on-thread") {
            println!("building the stream on a spawned thread, as the recorder does");
            let (s2, p2) = (samples.clone(), peak.clone());
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let cfg = config.clone();
            let handle = std::thread::spawn(move || {
                let stream = device
                    .build_input_stream(
                        &cfg.into(),
                        move |data: &[f32], _: &cpal::InputCallbackInfo| {
                            s2.fetch_add(data.len() as u64, Ordering::Relaxed);
                            let loudest = data.iter().fold(0.0f32, |m, x| m.max(x.abs()));
                            p2.fetch_max((loudest * 10_000.0) as u64, Ordering::Relaxed);
                        },
                        |e| println!("stream error: {e}"),
                        None,
                    )
                    .expect("build on thread");
                stream.play().expect("play on thread");
                let _ = rx.recv();
                drop(stream);
            });
            std::thread::sleep(std::time::Duration::from_secs(5));
            let _ = tx.send(());
            let _ = handle.join();
            let n = samples.load(Ordering::Relaxed);
            let loudest = peak.load(Ordering::Relaxed) as f32 / 10_000.0;
            println!("samples: {n}, peak: {loudest:.4}");
            println!(
                "{}",
                if n == 0 {
                    "NOTHING ARRIVED on the spawned thread"
                } else {
                    "AUDIO OK on the spawned thread"
                }
            );
            return;
        }

        let stream = device.build_input_stream(
            &config.clone().into(),
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                s.fetch_add(data.len() as u64, Ordering::Relaxed);
                let loudest = data.iter().fold(0.0f32, |m, x| m.max(x.abs()));
                let scaled = (loudest * 10_000.0) as u64;
                p.fetch_max(scaled, Ordering::Relaxed);
            },
            |e| println!("stream error: {e}"),
            None,
        );
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                println!("build_input_stream failed: {e}");
                return;
            }
        };
        if let Err(e) = stream.play() {
            println!("play failed: {e}");
            return;
        }

        println!("listening for 5s — say something");
        std::thread::sleep(std::time::Duration::from_secs(5));
        let n = samples.load(Ordering::Relaxed);
        let loudest = peak.load(Ordering::Relaxed) as f32 / 10_000.0;
        println!("samples: {n}, peak: {loudest:.4}");
        println!(
            "{}",
            match (n, loudest) {
                (0, _) => "NOTHING ARRIVED — the stream started and delivered no callbacks",
                (_, p) if p < 0.0005 =>
                    "SILENCE — callbacks arrive, every sample is ~zero \
                     (this is what a denied microphone looks like on macOS)",
                _ => "AUDIO OK — the microphone reaches this process",
            }
        );
    }
}

// Second half: the same microphone, but opened *after* this process created a
// Core Audio process tap — which is the order the recorder uses. Run with:
//
//     cargo run -p gilb-record --example mic_probe -- with-tap
