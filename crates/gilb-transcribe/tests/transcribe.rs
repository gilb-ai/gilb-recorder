//! Tests for gilb-transcribe: the pure energy VAD and the two-channel
//! `transcribe_meeting` merge/persist flow driven by a mock [`Transcriber`] over
//! a temp sqlite — no model, no GPU.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use gilb_db::meetings::insert_meeting;
use gilb_db::open_db;
use gilb_db::transcripts::get_transcript;
use gilb_transcribe::{
    suppress_mic_echoes, transcribe_meeting, voiced_fraction, voiced_mask, voiced_secs, Channel,
    Segment, Transcriber, Utterance, MODEL,
};
use uuid::Uuid;

fn temp_db_path() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("gilb-transcribe-test-{}.sqlite", Uuid::new_v4()));
    p
}

fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}

/// A temp meeting dir with empty `mic.wav`/`system.wav` (the mock ignores their
/// content — `transcribe_channels` only checks existence). Returns the derived
/// `audio.wav` path the flow keys off, plus the dir to clean up.
fn temp_meeting_audio() -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("gilb-meeting-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("mic.wav"), b"").expect("mic");
    std::fs::write(dir.join("system.wav"), b"").expect("system");
    (dir.join("audio.wav"), dir)
}

// ----- energy VAD (pure) --------------------------------------------------

#[test]
fn voiced_mask_marks_silence_unvoiced() {
    let silence = vec![0.0_f32; 16_000];
    let mask = voiced_mask(&silence);
    assert!(mask.iter().all(|&v| !v), "pure silence must be unvoiced");
    assert_eq!(voiced_secs(&mask), 0.0);
}

#[test]
fn voiced_mask_splits_loud_from_quiet() {
    // 1 s: first half loud (alternating ±0.5 ⇒ RMS 0.5), second half silent.
    let mut s = Vec::with_capacity(16_000);
    for i in 0..8_000 {
        s.push(if i % 2 == 0 { 0.5 } else { -0.5 });
    }
    s.resize(16_000, 0.0); // pad second half with silence
    let mask = voiced_mask(&s);

    let secs = voiced_secs(&mask);
    assert!((0.3..0.7).contains(&secs), "≈0.5 s voiced, got {secs}");
    assert!(
        voiced_fraction(&mask, 0.0, 0.4) > 0.8,
        "loud half is voiced"
    );
    assert!(
        voiced_fraction(&mask, 0.6, 1.0) < 0.2,
        "silent half is unvoiced"
    );
}

// ----- mocks --------------------------------------------------------------

/// Returns one utterance per channel, keyed off the file name, so a test can see
/// merge order and channel tagging.
struct ChannelMock;

#[async_trait]
impl Transcriber for ChannelMock {
    async fn transcribe(&self, audio: &Path) -> anyhow::Result<Vec<Utterance>> {
        let name = audio.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let u = match name {
            "mic.wav" => Utterance {
                t0: 0.0,
                t1: 2.0,
                text: "hello from mic".into(),
            },
            "system.wav" => Utterance {
                t0: 1.0,
                t1: 3.0,
                text: "reply from call".into(),
            },
            _ => return Ok(vec![]),
        };
        Ok(vec![u])
    }
}

/// Always fails, counting calls.
struct FailMock {
    calls: AtomicUsize,
}

#[async_trait]
impl Transcriber for FailMock {
    async fn transcribe(&self, _audio: &Path) -> anyhow::Result<Vec<Utterance>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(anyhow::anyhow!("simulated whisper failure"))
    }
}

// ----- transcribe_meeting flow -------------------------------------------

#[tokio::test]
async fn merges_channels_sorted_with_tags() {
    let dbp = temp_db_path();
    let db = open_db(&dbp).await.expect("open_db");
    let mid = insert_meeting(&db, 0, "us.zoom.xos").await.expect("insert");
    let (audio, dir) = temp_meeting_audio();

    transcribe_meeting(&db, mid, &audio, &ChannelMock)
        .await
        .expect("transcribe_meeting");

    let t = get_transcript(&db, mid).await.expect("get").expect("row");
    // mic (t0=0.0) sorts before system (t0=1.0).
    assert_eq!(t.text.as_deref(), Some("hello from mic reply from call"));
    let segs = t.segments_json.as_deref().unwrap_or("");
    assert!(segs.contains(r#""channel":"mic""#), "mic tag: {segs}");
    assert!(segs.contains(r#""channel":"system""#), "system tag: {segs}");
    assert!(
        segs.find("mic").unwrap() < segs.find("system").unwrap(),
        "mic segment first"
    );
    assert_eq!(t.model.as_deref(), Some(MODEL));
    assert!(t.error.is_none());

    // Sibling transcript files are written with Me/Others speaker labels.
    let txt = std::fs::read_to_string(dir.join("transcript.txt")).expect("transcript.txt");
    assert!(txt.contains("Me: hello from mic"), "txt: {txt}");
    assert!(txt.contains("Others: reply from call"), "txt: {txt}");
    let js = std::fs::read_to_string(dir.join("transcript.json")).expect("transcript.json");
    assert!(js.contains(r#""speaker": "Me""#), "json: {js}");
    assert!(js.contains(r#""speaker": "Others""#), "json: {js}");

    db.close().await;
    cleanup(&dbp);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn hard_failure_records_error() {
    let dbp = temp_db_path();
    let db = open_db(&dbp).await.expect("open_db");
    let mid = insert_meeting(&db, 0, "us.zoom.xos").await.expect("insert");
    let (audio, dir) = temp_meeting_audio();

    let mock = FailMock {
        calls: AtomicUsize::new(0),
    };
    transcribe_meeting(&db, mid, &audio, &mock)
        .await
        .expect("persists error, not Err");

    // First channel (mic.wav) fails ⇒ flow stops and records it.
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    let t = get_transcript(&db, mid).await.expect("get").expect("row");
    let err = t.error.as_deref().expect("error recorded");
    assert!(
        err.contains("simulated whisper failure"),
        "source in msg: {err}"
    );
    assert!(err.contains("mic.wav"), "names channel: {err}");
    assert!(t.text.is_none());

    db.close().await;
    cleanup(&dbp);
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Mic-echo suppression (`suppress_mic_echoes`).
// ---------------------------------------------------------------------------

fn seg(channel: Channel, t0: f32, text: &str) -> Segment {
    Segment {
        t0,
        t1: t0 + 2.0,
        channel,
        text: text.to_string(),
    }
}

#[test]
fn mic_echo_of_a_system_line_is_dropped() {
    let (kept, dropped) = suppress_mic_echoes(vec![
        seg(Channel::System, 10.0, "Да Катя, десять минут будет."),
        seg(Channel::Mic, 10.4, "Да Катя десять минут будет"),
    ]);
    assert_eq!(dropped, 1);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].channel, Channel::System, "the system copy survives");
}

#[test]
fn simultaneous_speech_in_different_words_is_kept() {
    let (kept, dropped) = suppress_mic_echoes(vec![
        seg(
            Channel::System,
            10.0,
            "мы посмотрим что будет разворачиваться дальше",
        ),
        seg(Channel::Mic, 10.3, "я пока проверю логи на своей стороне"),
    ]);
    assert_eq!(dropped, 0);
    assert_eq!(kept.len(), 2, "different words are never an echo");
}

#[test]
fn short_backchannel_lines_are_ineligible() {
    // "да"/"угу" recur constantly on both channels; too short to match on.
    let (kept, dropped) = suppress_mic_echoes(vec![
        seg(Channel::System, 5.0, "Да."),
        seg(Channel::Mic, 5.5, "Да."),
    ]);
    assert_eq!(dropped, 0);
    assert_eq!(kept.len(), 2);
}

#[test]
fn echo_window_is_bounded_in_both_directions() {
    let line = "наш разговор который сейчас был я проанализировал";
    // Outside the window on either side: not an echo.
    let (_, dropped) = suppress_mic_echoes(vec![
        seg(Channel::System, 10.0, line),
        seg(Channel::Mic, 13.0, line),
    ]);
    assert_eq!(dropped, 0, "3s later is not an echo");
    let (_, dropped) = suppress_mic_echoes(vec![
        seg(Channel::Mic, 7.0, line),
        seg(Channel::System, 10.0, line),
    ]);
    assert_eq!(dropped, 0, "3s earlier is not an echo");
    // Within the window it is — in both directions, because whisper's segment
    // boundaries and the channels' clocks jitter more than the acoustic delay.
    let (_, dropped) = suppress_mic_echoes(vec![
        seg(Channel::System, 10.0, line),
        seg(Channel::Mic, 11.0, line),
    ]);
    assert_eq!(dropped, 1, "mic shortly after");
    let (_, dropped) = suppress_mic_echoes(vec![
        seg(Channel::Mic, 9.5, line),
        seg(Channel::System, 10.0, line),
    ]);
    assert_eq!(dropped, 1, "mic shortly before (timestamp jitter)");
}

#[test]
fn containment_matches_differently_segmented_echo() {
    // Whisper often splits the echo differently: the mic copy carries a strict
    // subset of the system line's words. Containment catches what Jaccard misses.
    let (kept, dropped) = suppress_mic_echoes(vec![
        seg(
            Channel::System,
            20.0,
            "ну наш разговор который сейчас был я его у себя проанализировал",
        ),
        seg(Channel::Mic, 20.9, "наш разговор который сейчас был"),
    ]);
    assert_eq!(dropped, 1);
    assert_eq!(kept[0].channel, Channel::System);
}

#[test]
fn eligibility_counts_characters_not_bytes() {
    // Two Cyrillic words, 14 characters — under both the 4-word and the
    // 20-character floor, so never a match. Byte length (27 in UTF-8) would
    // wrongly make this eligible; a two-word reply like this can genuinely be
    // said by both sides, so it must stay.
    let (kept, dropped) = suppress_mic_echoes(vec![
        seg(Channel::System, 10.0, "Давай выключим."),
        seg(Channel::Mic, 10.4, "Давай выключим."),
    ]);
    assert_eq!(dropped, 0);
    assert_eq!(kept.len(), 2);
}

#[test]
fn punctuation_and_case_do_not_defeat_the_match() {
    let (_, dropped) = suppress_mic_echoes(vec![
        seg(
            Channel::System,
            30.0,
            "Там сорок девять процентов недельной квоты уже!",
        ),
        seg(
            Channel::Mic,
            30.6,
            "там сорок девять процентов недельной квоты уже",
        ),
    ]);
    assert_eq!(dropped, 1);
}
