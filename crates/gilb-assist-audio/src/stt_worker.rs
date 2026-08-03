//! Realtime STT worker (`docs/assist.md`): one long-lived task, one
//! model, both channels interleaved in a single queue.
//!
//! The queue drops the *oldest* segments when a backlog builds — the
//! conversation has moved on, and a suggestion about something said a minute
//! ago is worse than none. The transcriber sits behind a trait so the worker's
//! policy is host-testable without whisper; the real implementation wraps
//! `gilb_transcribe::LocalTranscriber::transcribe_buffer` and frees the model
//! in `unload` (~570 MB) when no meeting is feeding segments.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::segment::Segment;

/// Which capture channel a segment came from. Mic = the user ("Me"), System =
/// everyone else in the call ("Them").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SttChannel {
    Mic,
    System,
}

/// A recognized, pause-bounded utterance, ready for the assist engine.
#[derive(Debug, Clone)]
pub struct RecognizedUtterance {
    pub channel: SttChannel,
    pub text: String,
    pub start_secs: f64,
    pub end_secs: f64,
}

/// The model behind the worker. `transcribe` runs one segment; `unload` frees
/// whatever is expensive to keep warm (reloading lazily is the impl's job).
#[async_trait]
pub trait SegmentTranscriber: Send + 'static {
    /// Recognize a pause-bounded 16 kHz segment. The segment carries its
    /// detector's voiced mask so implementations filter with it instead of
    /// running a second VAD. Empty string = nothing was said.
    async fn transcribe(&mut self, segment: Segment) -> Result<String>;
    /// Called after [`SttWorkerConfig::idle_unload`] without work.
    fn unload(&mut self) {}
}

#[derive(Debug, Clone)]
pub struct SttWorkerConfig {
    /// Segments kept when the model falls behind; older ones are dropped.
    pub max_queue: usize,
    /// Idle time after which the transcriber is asked to unload.
    pub idle_unload: Duration,
}

impl Default for SttWorkerConfig {
    fn default() -> Self {
        Self {
            max_queue: 4,
            idle_unload: Duration::from_secs(300),
        }
    }
}

/// Feeds segments to the worker. Cheap to clone; dropping every handle stops
/// the worker once its queue drains.
#[derive(Clone)]
pub struct SttWorkerHandle {
    tx: mpsc::UnboundedSender<(u64, SttChannel, Segment)>,
    keep_model: Arc<AtomicBool>,
    /// Bumped by [`reset`](Self::reset); segments tagged with an older epoch
    /// are dropped by the worker instead of transcribed.
    epoch: Arc<AtomicU64>,
}

impl SttWorkerHandle {
    pub fn push(&self, channel: SttChannel, segment: Segment) {
        let _ = self
            .tx
            .send((self.epoch.load(Ordering::SeqCst), channel, segment));
    }

    /// Hold the model in memory regardless of the idle timer. Set while a
    /// meeting is being recorded: a long pause in the conversation is normal,
    /// and paying a ~570 MB reload for the first word after it would put the
    /// suggestion seconds behind the discussion.
    pub fn hold_model(&self, hold: bool) {
        self.keep_model.store(hold, Ordering::SeqCst);
    }

    /// Drop every queued segment and the result of any in-flight
    /// transcription. Called at a meeting boundary: stale work belongs to the
    /// previous conversation (and its stream clock), not the one just
    /// starting — and the conversation it would have landed in is already
    /// reset by then.
    pub fn reset(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }
}

/// Spawn the worker task; recognized utterances arrive on the returned
/// receiver in processing order. The returned [`JoinHandle`] lets the owner
/// abort the worker promptly (without it the task outlives its handles,
/// draining a queue nobody reads — possibly through a cold ~570 MB model
/// load).
pub fn spawn_stt_worker<T: SegmentTranscriber>(
    transcriber: T,
    config: SttWorkerConfig,
) -> (
    SttWorkerHandle,
    mpsc::UnboundedReceiver<RecognizedUtterance>,
    JoinHandle<()>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let keep_model = Arc::new(AtomicBool::new(false));
    let epoch = Arc::new(AtomicU64::new(0));
    let task = tokio::spawn(run(
        transcriber,
        config,
        rx,
        out_tx,
        keep_model.clone(),
        epoch.clone(),
    ));
    (
        SttWorkerHandle {
            tx,
            keep_model,
            epoch,
        },
        out_rx,
        task,
    )
}

async fn run<T: SegmentTranscriber>(
    mut transcriber: T,
    config: SttWorkerConfig,
    mut rx: mpsc::UnboundedReceiver<(u64, SttChannel, Segment)>,
    out: mpsc::UnboundedSender<RecognizedUtterance>,
    keep_model: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
) {
    let mut queue: std::collections::VecDeque<(u64, SttChannel, Segment)> =
        std::collections::VecDeque::new();
    loop {
        // Take everything already waiting, so the drop-oldest policy sees the
        // whole backlog before the next (slow) inference starts.
        while let Ok(item) = rx.try_recv() {
            queue.push_back(item);
        }
        if queue.len() > config.max_queue {
            let dropped = queue.len() - config.max_queue;
            queue.drain(..dropped);
            warn!(dropped, "stt backlog: dropped oldest segments");
        }

        if let Some((seg_epoch, channel, segment)) = queue.pop_front() {
            if seg_epoch != epoch.load(Ordering::SeqCst) {
                continue; // reset while it sat in the queue — stale
            }
            let (start_secs, end_secs) = (segment.start_secs, segment.end_secs);
            match transcriber.transcribe(segment).await {
                // A reset during the inference retires the result too: it
                // belongs to the previous meeting's conversation and clock.
                Ok(text) if !text.trim().is_empty() => {
                    if seg_epoch == epoch.load(Ordering::SeqCst) {
                        let _ = out.send(RecognizedUtterance {
                            channel,
                            text,
                            start_secs,
                            end_secs,
                        });
                    }
                }
                Ok(_) => {} // silence / filtered out
                Err(e) => warn!(error = %e, "stt segment failed; skipping"),
            }
            continue;
        }

        tokio::select! {
            item = rx.recv() => match item {
                Some(item) => queue.push_back(item),
                None => break, // all handles dropped, queue drained
            },
            _ = tokio::time::sleep(config.idle_unload) => {
                if !keep_model.load(Ordering::SeqCst) {
                    transcriber.unload();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    /// Transcriber that must be granted one permit per segment; lets tests
    /// hold the worker mid-inference to build a backlog deterministically.
    struct GatedMock {
        gate: Arc<Semaphore>,
        unloaded: Arc<AtomicBool>,
    }

    #[async_trait]
    impl SegmentTranscriber for GatedMock {
        async fn transcribe(&mut self, segment: Segment) -> Result<String> {
            self.gate.acquire().await.unwrap().forget();
            // Identify the segment by its first sample so tests can tell
            // which ones survived the queue.
            Ok(format!("seg{}", segment.samples[0] as i64))
        }

        fn unload(&mut self) {
            self.unloaded.store(true, Ordering::SeqCst);
        }
    }

    fn seg(id: i64) -> Segment {
        Segment {
            samples: vec![id as f32; 16],
            start_secs: id as f64,
            end_secs: id as f64 + 1.0,
            voiced: vec![true],
            vad_frame_size: 16,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn transcribes_in_order_with_channels() {
        let gate = Arc::new(Semaphore::new(100));
        let mock = GatedMock {
            gate,
            unloaded: Arc::new(AtomicBool::new(false)),
        };
        let (handle, mut rx, _task) = spawn_stt_worker(mock, SttWorkerConfig::default());

        handle.push(SttChannel::Mic, seg(1));
        handle.push(SttChannel::System, seg(2));
        drop(handle);

        let a = rx.recv().await.unwrap();
        let b = rx.recv().await.unwrap();
        assert_eq!((a.channel, a.text.as_str()), (SttChannel::Mic, "seg1"));
        assert_eq!((b.channel, b.text.as_str()), (SttChannel::System, "seg2"));
        assert_eq!(a.start_secs, 1.0);
        assert!(
            rx.recv().await.is_none(),
            "worker must stop when handles drop"
        );
    }

    /// While one segment is in flight and the queue overflows, the *oldest*
    /// queued segments are dropped — the conversation has moved on.
    #[tokio::test(start_paused = true)]
    async fn backlog_drops_oldest() {
        let gate = Arc::new(Semaphore::new(1)); // only seg1 may proceed
        let mock = GatedMock {
            gate: gate.clone(),
            unloaded: Arc::new(AtomicBool::new(false)),
        };
        let config = SttWorkerConfig {
            max_queue: 2,
            ..Default::default()
        };
        let (handle, mut rx, _task) = spawn_stt_worker(mock, config);

        handle.push(SttChannel::Mic, seg(1));
        tokio::task::yield_now().await; // worker takes seg1, blocks on the gate
        for id in 2..=6 {
            handle.push(SttChannel::System, seg(id));
        }
        drop(handle);
        gate.add_permits(100);

        let mut texts = Vec::new();
        while let Some(u) = rx.recv().await {
            texts.push(u.text);
        }
        assert_eq!(
            texts,
            ["seg1", "seg5", "seg6"],
            "middle of the backlog must be dropped"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unloads_after_idle() {
        let unloaded = Arc::new(AtomicBool::new(false));
        let mock = GatedMock {
            gate: Arc::new(Semaphore::new(100)),
            unloaded: unloaded.clone(),
        };
        let config = SttWorkerConfig {
            idle_unload: Duration::from_secs(300),
            ..Default::default()
        };
        let (handle, mut rx, _task) = spawn_stt_worker(mock, config);

        handle.push(SttChannel::Mic, seg(1));
        assert_eq!(rx.recv().await.unwrap().text, "seg1");
        assert!(!unloaded.load(Ordering::SeqCst));

        tokio::task::yield_now().await; // let the worker reach its idle select
        tokio::time::advance(Duration::from_secs(301)).await;
        tokio::task::yield_now().await; // let the fired timer run unload()
        assert!(
            unloaded.load(Ordering::SeqCst),
            "idle must unload the model"
        );
        drop(handle);
    }

    /// While a meeting holds the model, an idle stretch must not unload it.
    #[tokio::test(start_paused = true)]
    async fn hold_keeps_the_model_through_idle() {
        let unloaded = Arc::new(AtomicBool::new(false));
        let mock = GatedMock {
            gate: Arc::new(Semaphore::new(100)),
            unloaded: unloaded.clone(),
        };
        let (handle, mut rx, _task) = spawn_stt_worker(mock, SttWorkerConfig::default());
        handle.hold_model(true);

        handle.push(SttChannel::Mic, seg(1));
        assert_eq!(rx.recv().await.unwrap().text, "seg1");

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3_600)).await;
        tokio::task::yield_now().await;
        assert!(
            !unloaded.load(Ordering::SeqCst),
            "a held model must stay loaded"
        );

        // Releasing the hold lets the next idle window unload it.
        handle.hold_model(false);
        tokio::time::advance(Duration::from_secs(301)).await;
        tokio::task::yield_now().await;
        assert!(
            unloaded.load(Ordering::SeqCst),
            "released model must unload when idle"
        );
        drop(handle);
    }

    /// A meeting boundary retires everything from the previous stream: the
    /// queued backlog and the result of the segment mid-inference. Only work
    /// pushed after the reset may reach the (freshly reset) conversation.
    #[tokio::test(start_paused = true)]
    async fn reset_drops_queued_and_in_flight() {
        let gate = Arc::new(Semaphore::new(0)); // seg1 blocks mid-inference
        let mock = GatedMock {
            gate: gate.clone(),
            unloaded: Arc::new(AtomicBool::new(false)),
        };
        let (handle, mut rx, _task) = spawn_stt_worker(mock, SttWorkerConfig::default());

        handle.push(SttChannel::Mic, seg(1));
        tokio::task::yield_now().await; // worker takes seg1, blocks on the gate
        handle.push(SttChannel::System, seg(2));

        handle.reset();
        gate.add_permits(100); // seg1's inference now finishes — into the void
        handle.push(SttChannel::Mic, seg(3));
        drop(handle);

        let mut texts = Vec::new();
        while let Some(u) = rx.recv().await {
            texts.push(u.text);
        }
        assert_eq!(
            texts,
            ["seg3"],
            "pre-reset work, queued or in flight, must not surface"
        );
    }

    /// Aborting the returned task handle stops the worker at once — it must
    /// not keep transcribing a backlog nobody will read.
    #[tokio::test(start_paused = true)]
    async fn aborting_the_task_stops_the_worker() {
        let gate = Arc::new(Semaphore::new(0)); // nothing may proceed
        let mock = GatedMock {
            gate: gate.clone(),
            unloaded: Arc::new(AtomicBool::new(false)),
        };
        let (handle, mut rx, task) = spawn_stt_worker(mock, SttWorkerConfig::default());

        handle.push(SttChannel::Mic, seg(1));
        task.abort();
        assert!(
            task.await.unwrap_err().is_cancelled(),
            "the worker must stop with its aborted task"
        );
        drop(handle);
        assert!(rx.recv().await.is_none(), "no output after the abort");
        assert_eq!(
            gate.available_permits(),
            0,
            "no inference may start after the abort"
        );
    }
}
