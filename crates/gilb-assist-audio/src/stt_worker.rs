//! Realtime STT worker (§6.5 of REALTIME_ASSIST.md): one long-lived task, one
//! model, both channels interleaved in a single queue.
//!
//! The queue drops the *oldest* segments when a backlog builds — the
//! conversation has moved on, and a suggestion about something said a minute
//! ago is worse than none. The transcriber sits behind a trait so the worker's
//! policy is host-testable without whisper; the real implementation wraps
//! `gilb_transcribe::LocalTranscriber::transcribe_buffer` and frees the model
//! in `unload` (~570 MB) when no meeting is feeding segments.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
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
    tx: mpsc::UnboundedSender<(SttChannel, Segment)>,
}

impl SttWorkerHandle {
    pub fn push(&self, channel: SttChannel, segment: Segment) {
        let _ = self.tx.send((channel, segment));
    }
}

/// Spawn the worker task; recognized utterances arrive on the returned
/// receiver in processing order.
pub fn spawn_stt_worker<T: SegmentTranscriber>(
    transcriber: T,
    config: SttWorkerConfig,
) -> (SttWorkerHandle, mpsc::UnboundedReceiver<RecognizedUtterance>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    tokio::spawn(run(transcriber, config, rx, out_tx));
    (SttWorkerHandle { tx }, out_rx)
}

async fn run<T: SegmentTranscriber>(
    mut transcriber: T,
    config: SttWorkerConfig,
    mut rx: mpsc::UnboundedReceiver<(SttChannel, Segment)>,
    out: mpsc::UnboundedSender<RecognizedUtterance>,
) {
    let mut queue: std::collections::VecDeque<(SttChannel, Segment)> =
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

        if let Some((channel, segment)) = queue.pop_front() {
            let (start_secs, end_secs) = (segment.start_secs, segment.end_secs);
            match transcriber.transcribe(segment).await {
                Ok(text) if !text.trim().is_empty() => {
                    let _ = out.send(RecognizedUtterance {
                        channel,
                        text,
                        start_secs,
                        end_secs,
                    });
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
            _ = tokio::time::sleep(config.idle_unload) => transcriber.unload(),
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
        let mock = GatedMock { gate, unloaded: Arc::new(AtomicBool::new(false)) };
        let (handle, mut rx) = spawn_stt_worker(mock, SttWorkerConfig::default());

        handle.push(SttChannel::Mic, seg(1));
        handle.push(SttChannel::System, seg(2));
        drop(handle);

        let a = rx.recv().await.unwrap();
        let b = rx.recv().await.unwrap();
        assert_eq!((a.channel, a.text.as_str()), (SttChannel::Mic, "seg1"));
        assert_eq!((b.channel, b.text.as_str()), (SttChannel::System, "seg2"));
        assert_eq!(a.start_secs, 1.0);
        assert!(rx.recv().await.is_none(), "worker must stop when handles drop");
    }

    /// While one segment is in flight and the queue overflows, the *oldest*
    /// queued segments are dropped — the conversation has moved on.
    #[tokio::test(start_paused = true)]
    async fn backlog_drops_oldest() {
        let gate = Arc::new(Semaphore::new(1)); // only seg1 may proceed
        let mock = GatedMock { gate: gate.clone(), unloaded: Arc::new(AtomicBool::new(false)) };
        let config = SttWorkerConfig { max_queue: 2, ..Default::default() };
        let (handle, mut rx) = spawn_stt_worker(mock, config);

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
        assert_eq!(texts, ["seg1", "seg5", "seg6"], "middle of the backlog must be dropped");
    }

    #[tokio::test(start_paused = true)]
    async fn unloads_after_idle() {
        let unloaded = Arc::new(AtomicBool::new(false));
        let mock = GatedMock { gate: Arc::new(Semaphore::new(100)), unloaded: unloaded.clone() };
        let config = SttWorkerConfig { idle_unload: Duration::from_secs(300), ..Default::default() };
        let (handle, mut rx) = spawn_stt_worker(mock, config);

        handle.push(SttChannel::Mic, seg(1));
        assert_eq!(rx.recv().await.unwrap().text, "seg1");
        assert!(!unloaded.load(Ordering::SeqCst));

        tokio::task::yield_now().await; // let the worker reach its idle select
        tokio::time::advance(Duration::from_secs(301)).await;
        tokio::task::yield_now().await; // let the fired timer run unload()
        assert!(unloaded.load(Ordering::SeqCst), "idle must unload the model");
        drop(handle);
    }
}
