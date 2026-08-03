//! The realtime path with the **real** model, end to end.
//!
//! `pipeline.rs` proves the plumbing with a stub transcriber: audio in, turns
//! out. What it cannot prove is the part the user actually judges — that
//! whisper, fed by *this* segmenter at *this* sample rate, returns words rather
//! than empty strings or hallucinated silence. Those are different failures
//! with different fixes, and only one of them shows up here.
//!
//! Needs the ~570 MB model, so it skips itself when there is none rather than
//! failing a machine that never downloaded one. To run it:
//!
//! ```sh
//! cargo test -p gilb-assist-audio --features whisper,silero --test realtime_whisper -- --nocapture
//! ```

#![cfg(feature = "whisper")]

use std::time::Duration;

use gilb_assist::{AssistEvent, EngineParams};
use gilb_assist_audio::{
    spawn_assist_pipeline, AssistPipelineConfig, LocalTranscriber, SharedModel, WhisperTranscriber,
};
use gilb_record::AudioTap;

/// Conversational speech at 16 kHz — the same fixture the segmenter tests use.
const SPEECH: &[u8] = include_bytes!("fixtures/speech_16k.wav");

fn speech_samples() -> Vec<f32> {
    let mut reader = hound::WavReader::new(std::io::Cursor::new(SPEECH)).unwrap();
    assert_eq!(reader.spec().sample_rate, 16_000);
    reader
        .samples::<i16>()
        .map(|s| f32::from(s.unwrap()) / 32768.0)
        .collect()
}

fn quiet(len: usize) -> Vec<f32> {
    // Not pure zeros: a real room has a noise floor, and the VAD should be
    // closing the segment on *speech* ending rather than on digital silence.
    (0..len)
        .map(|i| ((i as f32 * 0.37).sin()) * 0.0005)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn speech_through_the_tap_comes_back_as_words() {
    let Ok(model) = gilb_config::transcribe_model_path() else {
        eprintln!("no data dir; skipping");
        return;
    };
    if !model.exists() {
        eprintln!(
            "skipping: no whisper model at {} — download it from the app first",
            model.display()
        );
        return;
    }

    let tap = AudioTap::new(4096);
    let (assist, mut events) = gilb_assist::spawn(
        gilb_assist::echo::StaticConfig::default(),
        gilb_assist::echo::EchoBackend,
        EngineParams {
            min_analysis_interval: Duration::from_millis(1),
        },
    );
    // The shared model, as the app wires it — so this also covers the handoff
    // that had both paths loading their own copy.
    let shared: std::sync::Arc<SharedModel<LocalTranscriber>> =
        SharedModel::new(Duration::from_secs(60));
    let _pipeline = spawn_assist_pipeline(
        &tap,
        WhisperTranscriber::with_shared(model, "en", shared.clone()),
        assist,
        AssistPipelineConfig::default(),
        None,
    );
    tokio::task::yield_now().await;

    // Speech, then enough quiet to close the segment.
    let mut stream = speech_samples();
    stream.extend(quiet(24_000));
    for chunk in stream.chunks(1_600) {
        tap.send_system(chunk, 16_000);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    // Generous: a cold model load is seconds, and CI machines are slower than
    // this one. The failure this guards against is "nothing, ever".
    let text = tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            match events.recv().await.expect("assist engine closed") {
                AssistEvent::Update(text) => break text,
                _ => continue,
            }
        }
    })
    .await
    .expect("whisper produced no transcript from real speech in 180s");

    // The echo backend replays what the engine sent it, so the suggestion text
    // carries the transcript — including the `them:` attribution, which is the
    // half that decides whether a suggestion answers the right person.
    assert!(
        text.contains("them:"),
        "system audio must arrive attributed to the other side: {text}"
    );
    let spoken = text.trim_start_matches("them:").trim();
    assert!(
        spoken.chars().any(char::is_alphabetic),
        "expected words, got {text:?}"
    );
    assert!(
        shared.is_loaded(),
        "the transcriber should have loaded the shared model, not a private one"
    );
    println!("transcript: {spoken}");
}
