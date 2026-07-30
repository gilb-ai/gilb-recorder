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
    meeting_paths, mix_to_mono_16k, scale_bgra, write_wav_16k_mono, NoopCapturer, Recorder,
    RecordingOutcome, ScreenAudioCapturer, TARGET_SAMPLE_RATE,
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
    let (video, audio) = meeting_paths(Path::new("/data"), "2026-06-05_16-57-03");
    assert_eq!(
        video,
        PathBuf::from("/data/meetings/2026-06-05_16-57-03/video.mp4")
    );
    assert_eq!(
        audio,
        PathBuf::from("/data/meetings/2026-06-05_16-57-03/audio.wav")
    );
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

#[test]
fn scale_bgra_identity_is_a_copy() {
    // 2x2 BGRA, same dst size → byte-for-byte copy.
    let src = vec![0, 0, 0, 255, 100, 0, 0, 255, 200, 0, 0, 255, 40, 0, 0, 255];
    assert_eq!(scale_bgra(&src, 2, 2, 2, 2), src);
}

#[test]
fn scale_bgra_downscale_2x2_to_1x1_blends_all_four() {
    // Centre sample of a 2x2 → 1x1 is the bilinear blend of the four pixels.
    // B channel: 0,100,200,40 → top=50, bot=120, mid=85. Alpha all 255.
    let src = vec![0, 0, 0, 255, 100, 0, 0, 255, 200, 0, 0, 255, 40, 0, 0, 255];
    assert_eq!(scale_bgra(&src, 2, 2, 1, 1), vec![85, 0, 0, 255]);
}

#[test]
fn scale_bgra_upscale_1x1_fills_with_source_pixel() {
    let src = vec![10, 20, 30, 40];
    let out = scale_bgra(&src, 1, 1, 2, 2);
    assert_eq!(
        out,
        vec![10, 20, 30, 40, 10, 20, 30, 40, 10, 20, 30, 40, 10, 20, 30, 40]
    );
}

#[test]
fn scale_bgra_degenerate_inputs_are_zeroed() {
    // Too-short source → zeroed output of the destination size.
    assert_eq!(scale_bgra(&[], 2, 2, 2, 2), vec![0u8; 16]);
    // Zero destination → empty.
    assert!(scale_bgra(&[1, 2, 3, 4], 1, 1, 0, 0).is_empty());
}

/// Capturer that counts start/stop calls and remembers the last paths.
#[derive(Default)]
struct CountingCapturer {
    starts: AtomicUsize,
    stops: AtomicUsize,
}

impl ScreenAudioCapturer for CountingCapturer {
    fn start(&self, _video: &Path, _audio: &Path, _app_bundle_id: Option<&str>) -> Result<()> {
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

    let m = meetings::get_meeting(&db, id)
        .await
        .expect("get")
        .expect("present");
    // The names are time-stamped (set inside `arm`), so assert structure rather
    // than an exact path: both live under `<dir>/meetings/` with the right ext.
    let meetings_dir = dir.join("meetings");
    let video = m.video_path.as_deref().expect("video path set");
    let audio = m.audio_path.as_deref().expect("audio path set");
    assert!(video.ends_with(".mp4"), "video should be .mp4: {video}");
    assert!(audio.ends_with(".wav"), "audio should be .wav: {audio}");
    assert!(Path::new(video).starts_with(&meetings_dir));
    assert!(Path::new(audio).starts_with(&meetings_dir));
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

/// Capturer that models the real backends' failure shape: it *claims* its output
/// file up front (as AVAssetWriter does on `startWriting`) and refuses outright if
/// that file already exists (`AVErrorFileAlreadyExists`). The first `fail_first`
/// attempts then fail after claiming.
///
/// This is what makes the retry test meaningful: unless `arm` clears the partial
/// output between tries, every attempt past the first dies on the leftover rather
/// than on the original, transient problem.
#[derive(Default)]
struct FileClaimingCapturer {
    starts: AtomicUsize,
    stops: AtomicUsize,
    fail_first: usize,
}

impl ScreenAudioCapturer for FileClaimingCapturer {
    fn start(&self, video: &Path, _audio: &Path, _app_bundle_id: Option<&str>) -> Result<()> {
        if video.exists() {
            anyhow::bail!("output already exists (AVErrorFileAlreadyExists)");
        }
        std::fs::write(video, b"")?;
        let n = self.starts.fetch_add(1, Ordering::SeqCst) + 1;
        if n <= self.fail_first {
            anyhow::bail!("transient start failure #{n}");
        }
        Ok(())
    }
    fn stop(&self) -> Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn arm_retries_a_transient_start_failure() {
    let dir = temp_dir();
    let db = temp_db(&dir).await;
    let id = meetings::insert_meeting(&db, 0, "us.zoom.xos")
        .await
        .expect("insert");

    let recorder = Recorder::new(
        db.clone(),
        dir.clone(),
        FileClaimingCapturer {
            fail_first: 2,
            ..Default::default()
        },
    );
    // Pause only now: the sqlx pool must be established on real time, or its
    // acquire timeout fires the instant the clock is auto-advanced.
    tokio::time::pause();
    recorder
        .arm(id)
        .await
        .expect("arm should survive two refusals");

    assert_eq!(recorder_starts(&recorder), 3, "two failures then a success");
    let m = meetings::get_meeting(&db, id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(m.status, "recording");
    assert!(m.video_path.is_some(), "paths are set once capture is up");

    db.close().await;
    cleanup(&dir);
}

#[tokio::test]
async fn arm_gives_up_after_exhausting_attempts() {
    let dir = temp_dir();
    let db = temp_db(&dir).await;
    let id = meetings::insert_meeting(&db, 0, "us.zoom.xos")
        .await
        .expect("insert");

    let recorder = Recorder::new(
        db.clone(),
        dir.clone(),
        FileClaimingCapturer {
            fail_first: usize::MAX,
            ..Default::default()
        },
    );
    tokio::time::pause();
    let err = recorder.arm(id).await.expect_err("every attempt fails");
    assert!(
        err.to_string().contains("start screen/audio capture"),
        "unexpected error: {err:#}"
    );
    assert_eq!(
        recorder_starts(&recorder),
        gilb_record::START_ATTEMPTS,
        "the whole retry budget is spent before giving up"
    );

    let m = meetings::get_meeting(&db, id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(m.status, "failed");
    assert!(m.ended_at.is_some());
    assert!(
        m.video_path.is_none(),
        "a meeting that never captured keeps no paths"
    );
    // The last attempt's claimed file is cleaned up rather than orphaned on disk.
    let stamp_dirs: Vec<_> = std::fs::read_dir(dir.join("meetings"))
        .expect("meetings dir")
        .filter_map(|e| e.ok())
        .collect();
    for entry in stamp_dirs {
        let leftover = entry.path().join("video.mp4");
        assert!(
            !leftover.exists(),
            "partial output left behind: {}",
            leftover.display()
        );
    }

    db.close().await;
    cleanup(&dir);
}

#[tokio::test]
async fn arm_abandons_its_retry_when_stop_arrives() {
    let dir = temp_dir();
    let db = temp_db(&dir).await;
    let id = meetings::insert_meeting(&db, 0, "us.zoom.xos")
        .await
        .expect("insert");

    let recorder = Arc::new(Recorder::new(
        db.clone(),
        dir.clone(),
        FileClaimingCapturer {
            fail_first: usize::MAX,
            ..Default::default()
        },
    ));

    // Real time here on purpose: the point is that a `stop` landing *during* a
    // backoff cuts the retry loop short instead of bringing a stream up minutes
    // after everyone hung up.
    let armed = tokio::spawn({
        let recorder = recorder.clone();
        async move { recorder.arm(id).await }
    });

    // Wait for the first attempt to have failed, i.e. the loop is now backing off.
    for _ in 0..200 {
        if recorder_starts(&recorder) >= 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(recorder_starts(&recorder), 1, "first attempt has run");

    recorder
        .stop(RecordingOutcome::Completed)
        .await
        .expect("stop while arming is a no-op for capture");

    let result = armed.await.expect("join");
    assert!(result.is_err(), "arm gives up instead of arming late");
    assert_eq!(
        recorder_starts(&recorder),
        1,
        "no further attempts after being abandoned"
    );

    let m = meetings::get_meeting(&db, id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(m.status, "failed");

    db.close().await;
    cleanup(&dir);
}

fn recorder_starts(recorder: &Recorder<FileClaimingCapturer>) -> usize {
    recorder.capturer().starts.load(Ordering::SeqCst)
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
