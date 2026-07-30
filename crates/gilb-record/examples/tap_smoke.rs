//! Live check of the Core Audio process-tap capture (`macos_tap`).
//!
//! The tap path never runs in CI (macOS-only, needs the Audio Recording TCC
//! grant), so this example is the way to prove it on a real host: it captures
//! N seconds of system audio and reports the delivered sample rate, sample
//! count, and level statistics. Play something (`afplay`, music, a call) while
//! it runs — a working tap shows a healthy peak/RMS; silence-only means the
//! tap is not hearing the system output.
//!
//! ```text
//! cargo run -p gilb-record --example tap_smoke -- [seconds]
//! ```

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use std::sync::{Arc, Mutex};

    use gilb_record::macos_tap::SystemAudioTap;

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(6);

    println!("tap supported: {}", SystemAudioTap::is_supported());

    let samples: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let samples = samples.clone();
        move |chunk: &[f32]| {
            if let Ok(mut buf) = samples.lock() {
                buf.extend_from_slice(chunk);
            }
        }
    };

    let tap = SystemAudioTap::start(sink)?;
    println!("tap running at {} Hz for {secs}s...", tap.sample_rate());
    std::thread::sleep(std::time::Duration::from_secs(secs));
    let rate = tap.sample_rate();
    drop(tap);

    let buf = samples.lock().expect("samples");
    let peak = buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
    let rms = (buf.iter().map(|s| s * s).sum::<f32>() / buf.len().max(1) as f32).sqrt();
    let captured_secs = buf.len() as f32 / rate.max(1) as f32;
    println!(
        "captured {} samples = {captured_secs:.2}s  peak={peak:.4} rms={rms:.4}",
        buf.len()
    );
    // Zero samples = nothing rendered audio for the whole window: a tap only
    // fires while some process plays, and with no first callback there is no
    // anchor to pad from. The recorder's mix pads that case at the tail.
    if buf.is_empty() {
        println!("no callbacks — was anything playing?");
        return Ok(());
    }
    anyhow::ensure!(
        (captured_secs - secs as f32).abs() < 1.0,
        "captured duration far from wall clock — gap padding is broken"
    );
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macOS-only example");
}
