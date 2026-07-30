//! Smoke-test the real capture backend end to end.
//!
//! `macos.rs` / `windows.rs` are the one part of the recorder CI can't touch: the
//! workspace is built and tested on Linux, so the SCK/AVAssetWriter path only
//! ever runs in production. This example exercises it directly, which is enough
//! to catch the things unit tests structurally cannot — whether both streams
//! actually come up, and whether each of the three audio sidecars receives
//! samples.
//!
//! ```text
//! cargo run -p gilb-record --example capture_smoke -- [seconds] [bundle-id]
//! ```
//!
//! With a bundle id the audio stream is scoped to that application and the video
//! stream targets its window; without one it falls back to system-wide audio and
//! a display capture. Prints the output paths so a caller can inspect durations
//! and levels (e.g. `ffprobe`, `ffmpeg -af volumedetect`).
//!
//! The host needs Screen Recording permission for whatever process runs this.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use gilb_record::{PlatformCapturer, ScreenAudioCapturer};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .init();

    let mut args = std::env::args().skip(1);
    let secs: u64 = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(8);
    let bundle_id = args.next();

    let dir = std::env::temp_dir().join("gilb-capture-smoke");
    std::fs::create_dir_all(&dir)?;
    let video: PathBuf = dir.join("video.mp4");
    let audio: PathBuf = dir.join("audio.wav");

    println!("capturing {secs}s -> {}", dir.display());
    if let Some(bid) = &bundle_id {
        println!("scoped to {bid}");
    } else {
        println!("no bundle id: system-wide audio + display video");
    }

    let capturer = PlatformCapturer::default();
    let started = Instant::now();
    capturer.start(&video, &audio, bundle_id.as_deref())?;
    println!("capture started in {:?}", started.elapsed());

    std::thread::sleep(Duration::from_secs(secs));

    let stopping = Instant::now();
    capturer.stop()?;
    println!("capture stopped in {:?} (includes the audio mux)", stopping.elapsed());

    for name in ["video.mp4", "audio.wav", "mic.wav", "system.wav"] {
        let path = dir.join(name);
        match std::fs::metadata(&path) {
            Ok(m) => println!("  {name:<12} {:>10} bytes", m.len()),
            Err(e) => println!("  {name:<12} MISSING ({e})"),
        }
    }
    Ok(())
}
