//! Silero VAD behaviour (D12) that the energy heuristic cannot deliver:
//! real speech in, segments out; loud non-speech in, nothing out.
#![cfg(feature = "silero")]

use gilb_assist_audio::{Segmenter, SegmenterConfig, SileroVad};

/// 6 s of real conversational speech, 16 kHz mono PCM16 (from the silero-vad
/// repo's test corpus, `aepyx.wav`).
const FIXTURE: &[u8] = include_bytes!("fixtures/speech_16k.wav");

fn fixture_samples() -> Vec<f32> {
    let mut reader = hound::WavReader::new(std::io::Cursor::new(FIXTURE)).unwrap();
    assert_eq!(reader.spec().sample_rate, 16_000);
    reader
        .samples::<i16>()
        .map(|s| f32::from(s.unwrap()) / 32768.0)
        .collect()
}

/// Deterministic white noise in [-amp, amp].
fn noise(len: usize, amp: f32, mut seed: u64) -> Vec<f32> {
    (0..len)
        .map(|_| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (((seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5) * 2.0 * amp
        })
        .collect()
}

fn silero_segmenter() -> Segmenter {
    Segmenter::with_vad(
        SegmenterConfig::default(),
        Box::new(SileroVad::new().expect("bundled silero model must load")),
    )
}

fn run(seg: &mut Segmenter, stream: &[f32]) -> Vec<gilb_assist_audio::Segment> {
    let mut got = Vec::new();
    for chunk in stream.chunks(333) {
        got.extend(seg.push(chunk));
    }
    got.extend(seg.flush());
    got
}

#[test]
fn detects_real_speech() {
    let mut stream = fixture_samples();
    stream.extend(vec![0.0; 16_000]); // trailing pause closes the last segment

    let got = run(&mut silero_segmenter(), &stream);

    assert!(!got.is_empty(), "real speech must produce segments");
    let voiced: f64 = got.iter().map(|s| s.end_secs - s.start_secs).sum();
    assert!(
        voiced >= 2.0,
        "expected >= 2 s of speech segments, got {voiced:.2} s"
    );
}

/// Loud white noise reads as "voiced" to any energy detector — this is
/// exactly the false-positive class (keyboards, notification sounds) that
/// justified the neural detector. Silero must stay silent on it.
#[test]
fn ignores_loud_non_speech() {
    let stream = noise(16_000 * 5, 0.2, 7);

    let energy = run(&mut Segmenter::new(SegmenterConfig::default()), &stream);
    assert!(
        !energy.is_empty(),
        "premise check: the energy vad is fooled by loud noise"
    );

    let silero = run(&mut silero_segmenter(), &stream);
    assert!(
        silero.is_empty(),
        "silero must not segment non-speech noise"
    );
}

#[test]
fn silence_yields_nothing() {
    let got = run(&mut silero_segmenter(), &noise(16_000 * 5, 0.001, 9));
    assert!(got.is_empty());
}
