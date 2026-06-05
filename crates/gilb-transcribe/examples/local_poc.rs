//! End-to-end check of the real shipping path: drives `gilb_transcribe::
//! LocalTranscriber` (model load → `Transcriber` trait → energy VAD → segment
//! filtering) over a meeting's two channels and prints the merged transcript.
//!
//! Run:
//!   cargo run -p gilb-transcribe --example local_poc --features local-whisper -- \
//!     <meeting_dir> <model.bin> [auto|ru|en]

use std::path::Path;
use std::time::Instant;

use gilb_transcribe::{Channel, LocalTranscriber, Transcriber};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: local_poc <meeting_dir> <model> [lang]");
    let model = args.next().expect("usage: local_poc <meeting_dir> <model> [lang]");
    let lang = args.next().unwrap_or_else(|| "auto".to_string());
    let dir = Path::new(&dir);

    let load = Instant::now();
    let t = LocalTranscriber::new(Path::new(&model), lang).expect("load model");
    eprintln!("model loaded in {:.1}s", load.elapsed().as_secs_f32());

    let mut segs = Vec::new();
    for (name, channel) in [("mic.wav", Channel::Mic), ("system.wav", Channel::System)] {
        let path = dir.join(name);
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
        let speaker = if *channel == Channel::Mic { "Me" } else { "Others" };
        println!("[{t0:6.2}-{t1:6.2}] ({speaker:<6}) {text}");
    }
}
