//! Host-testable core of the recorder: path derivation, the audio mix/resample,
//! the WAV writer, the meetings-row update, and the Armed/Cancelled state
//! machine driven through a no-op / counting capturer. The macOS capture
//! backend (`macos.rs`) is cfg-gated and exercised only on a macOS host.

use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use gilb_db::{meetings, open_db, Db};
use gilb_events::{EventBus, RecordingEvent};
use gilb_record::{
    meeting_paths, mix_to_mono_16k, write_wav_16k_mono, NoopCapturer, Recorder, RecordingOutcome,
    ScreenAudioCapturer, TARGET_SAMPLE_RATE,
};
use uuid::Uuid;

fn temp_dir() -> PathBuf {
    let mut p = env::temp_dir();
    p.push(format!("gilb-record-test-{}", Uuid::new_v4()));
    p
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

async fn temp_db(dir: &Path) -> Db {
    let path = dir.join("db.sqlite");
    open_db(&path).await.expect("open_db")
}

#[test]
fn meeting_paths_layout() {
    let (video, audio) = meeting_paths(Path::new("/data"), 7);
    assert_eq!(video, PathBuf::from("/data/meetings/7.mp4"));
    assert_eq!(audio, PathBuf::from("/data/meetings/7.audio.wav"));
}

#[test]
fn mix_passthrough_and_scaling() {
    // Full-scale mic, silent system, same rate: scales to i16::MAX.
    let mixed = mix_to_mono_16k(&[1.0, 0.0, -1.0], &[0.0, 0.0, 0.0], TARGET_SAMPLE_RATE);
    assert_eq!(mixed, vec![i16::MAX, 0, -i16::MAX]);
}

#[test]
fn mix_sums_and_clamps() {
    // mic + system would exceed full scale; the sum clamps to [-1, 1].
    let mixed = mix_to_mono_16k(&[0.8, 0.8], &[0.8, -0.8], TARGET_SAMPLE_RATE);
    assert_eq!(mixed[0], i16::MAX); // 1.6 -> clamp 1.0
    assert_eq!(mixed[1], 0); // 0.8 - 0.8 = 0.0
}

#[test]
fn mix_pads_shorter_channel_with_silence() {
    let mixed = mix_to_mono_16k(&[1.0], &[0.0, 0.0, 0.0], TARGET_SAMPLE_RATE);
    assert_eq!(mixed.len(), 3);
    assert_eq!(mixed[0], i16::MAX);
    assert_eq!(mixed[1], 0);
    assert_eq!(mixed[2], 0);
}

#[test]
fn mix_resamples_down_to_16k() {
    // 32 kHz in -> 16 kHz out halves the sample count.
    let src: Vec<f32> = (0..32).map(|_| 0.0).collect();
    let mixed = mix_to_mono_16k(&src, &[], 32_000);
    assert_eq!(mixed.len(), 16);
}

#[test]
fn mix_empty_is_empty() {
    assert!(mix_to_mono_16k(&[], &[], TARGET_SAMPLE_RATE).is_empty());
}

#[test]
fn wav_roundtrips_through_hound() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("a.wav");
    let samples = vec![0i16, 100, -100, i16::MAX, i16::MIN];
    write_wav_16k_mono(&path, &samples).expect("write wav");

    let mut reader = hound::WavReader::open(&path).expect("open wav");
    assert_eq!(reader.spec().sample_rate, TARGET_SAMPLE_RATE);
    assert_eq!(reader.spec().channels, 1);
    let read: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
    assert_eq!(read, samples);
    cleanup(&dir);
}

/// Capturer that counts start/stop calls and remembers the last paths.
#[derive(Default)]
struct CountingCapturer {
    starts: AtomicUsize,
    stops: AtomicUsize,
}

impl ScreenAudioCapturer for CountingCapturer {
    fn start(&self, _video: &Path, _audio: &Path) -> Result<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn stop(&self) -> Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn arm_sets_paths_then_stop_completes() {
    let dir = temp_dir();
    let db = temp_db(&dir).await;
    let id = meetings::insert_meeting(&db, 0, "us.zoom.xos")
        .await
        .expect("insert");

    let recorder = Recorder::new(db.clone(), dir.clone(), CountingCapturer::default());
    recorder.arm(id).await.expect("arm");

    let (video, audio) = meeting_paths(&dir, id);
    let m = meetings::get_meeting(&db, id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(
        m.video_path.as_deref(),
        Some(video.to_string_lossy().as_ref())
    );
    assert_eq!(
        m.audio_path.as_deref(),
        Some(audio.to_string_lossy().as_ref())
    );
    assert_eq!(m.status, "recording");

    recorder
        .stop(RecordingOutcome::Completed)
        .await
        .expect("stop");
    let m = meetings::get_meeting(&db, id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(m.status, "completed");
    assert!(m.ended_at.is_some());

    db.close().await;
    cleanup(&dir);
}

#[tokio::test]
async fn double_arm_is_ignored_until_stop() {
    let dir = temp_dir();
    let db = temp_db(&dir).await;
    let id = meetings::insert_meeting(&db, 0, "app")
        .await
        .expect("insert");

    let recorder = Arc::new(Recorder::new(
        db.clone(),
        dir.clone(),
        CountingCapturer::default(),
    ));
    recorder.arm(id).await.expect("arm");
    recorder.arm(id).await.expect("second arm is a no-op");

    // stop with no active capture after the first stop is also a no-op.
    recorder
        .stop(RecordingOutcome::Completed)
        .await
        .expect("stop");
    recorder
        .stop(RecordingOutcome::Completed)
        .await
        .expect("second stop no-op");

    db.close().await;
    cleanup(&dir);
}

#[tokio::test]
async fn stop_without_arm_is_noop() {
    let dir = temp_dir();
    let db = temp_db(&dir).await;
    let recorder = Recorder::new(db.clone(), dir.clone(), NoopCapturer);
    recorder
        .stop(RecordingOutcome::Cancelled)
        .await
        .expect("stop without arm is ok");
    db.close().await;
    cleanup(&dir);
}

#[tokio::test]
async fn bus_armed_then_cancelled_drives_state_machine() {
    let dir = temp_dir();
    let db = temp_db(&dir).await;
    let id = meetings::insert_meeting(&db, 0, "app")
        .await
        .expect("insert");

    let bus = EventBus::new();
    let recorder = Arc::new(Recorder::new(db.clone(), dir.clone(), NoopCapturer));
    tokio::spawn(recorder.clone().run(bus.clone()));

    // Re-publish until the subscriber (the spawned `run` loop) has caught up:
    // a broadcast send before `run` subscribes would otherwise be lost.
    publish_until(&bus, RecordingEvent::Armed { meeting_id: id }, || {
        let db = db.clone();
        async move {
            meetings::get_meeting(&db, id)
                .await
                .ok()
                .flatten()
                .map(|m| m.video_path.is_some())
                .unwrap_or(false)
        }
    })
    .await;

    publish_until(&bus, RecordingEvent::Cancelled { meeting_id: id }, || {
        let db = db.clone();
        async move {
            meetings::get_meeting(&db, id)
                .await
                .ok()
                .flatten()
                .map(|m| m.status == "cancelled")
                .unwrap_or(false)
        }
    })
    .await;

    db.close().await;
    cleanup(&dir);
}

/// Publish `event` once per poll until `cond` holds (or ~2s elapses), so the
/// test doesn't race the spawned subscriber. Re-sending is safe: a duplicate
/// `Armed`/`Cancelled` is a no-op once the state machine has moved.
async fn publish_until<F, Fut>(bus: &EventBus, event: RecordingEvent, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if cond().await {
            return;
        }
        bus.publish_recording(event.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("condition not met within timeout");
}
