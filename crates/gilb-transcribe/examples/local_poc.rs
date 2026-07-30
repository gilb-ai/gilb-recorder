//! End-to-end check of the real shipping path: drives `gilb_transcribe::
//! LocalTranscriber` (model load → `Transcriber` trait → energy VAD → segment
//! filtering) over a meeting's two channels and prints the merged transcript.
//!
//! Run:
//!   cargo run -p gilb-transcribe --example local_poc --features local-whisper -- \
//!     <meeting_dir> <model.bin> [auto|ru|en]

use std::path::Path;
use std::time::Instant;

use gilb_transcribe::{voiced_mask, voiced_secs, Channel, LocalTranscriber, Transcriber};

/// Read a 16 kHz mono i16 wav as f32 in [-1, 1] and print level diagnostics:
/// duration, peak/RMS amplitude, and the VAD's voiced-seconds verdict. This is
/// what gates transcription — if `voiced=0.0s` the channel is dropped as silence
/// before Whisper ever runs.
fn dump_levels(name: &str, path: &std::path::Path) {
    let mut reader = match hound::WavReader::open(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("  {name:<6} (no wav: {e})");
            return;
        }
    };
    let spec = reader.spec();
    let samples: Vec<f32> = reader
        .samples::<i16>()
        .map(|s| s.unwrap_or(0) as f32 / 32768.0)
        .collect();
    let peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt();
    let secs = samples.len() as f32 / spec.sample_rate as f32;
    let voiced = voiced_secs(&voiced_mask(&samples));
    eprintln!(
        "  {name:<6} {secs:5.1}s  peak={peak:.4} rms={rms:.4}  voiced={voiced:.1}s  ({} Hz, {} ch)",
        spec.sample_rate, spec.channels
    );
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args
        .next()
        .expect("usage: local_poc <meeting_dir> <model> [lang]");
    let model = args
        .next()
        .expect("usage: local_poc <meeting_dir> <model> [lang]");
    let lang = args.next().unwrap_or_else(|| "auto".to_string());
    let dir = Path::new(&dir);

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .init();

    let load = Instant::now();
    let t = LocalTranscriber::new(Path::new(&model), lang).expect("load model");
    eprintln!("model loaded in {:.1}s", load.elapsed().as_secs_f32());

    let mut segs = Vec::new();
    for (name, channel) in [("mic.wav", Channel::Mic), ("system.wav", Channel::System)] {
        let path = dir.join(name);
        dump_levels(channel.as_str(), &path);
        let run = Instant::now();
        let utts = t.transcribe(&path).await.expect("transcribe");
        eprintln!(
            "  {:<6} {} utterances in {:.1}s",
            channel.as_str(),
            utts.len(),
            run.elapsed().as_secs_f32()
        );
        segs.extend(utts.into_iter().map(|u| (u.t0, u.t1, channel, u.text)));
    }
    segs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

    eprintln!("\nmerged transcript:");
    for (t0, t1, channel, text) in &segs {
        let speaker = if *channel == Channel::Mic {
            "Me"
        } else {
            "Others"
        };
        println!("[{t0:6.2}-{t1:6.2}] ({speaker:<6}) {text}");
    }
}
