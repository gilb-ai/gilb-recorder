//! Realtime audio front-end for the assist feature ([RDK] real-time подсказки,
//! `docs/REALTIME_ASSIST.md`).
//!
//! The recording pipeline captures two mono channels: *mic* (Me) and *system*
//! (Them). They are separate sources, but when the user is on speakers the mic
//! acoustically picks up the remote side — without echo cancellation the Me
//! channel would transcribe Them's speech. [`EchoCanceller`] cleans the mic
//! stream using the system stream as the far-end reference.
//!
//! This crate only serves the assist path. The recording itself must never go
//! through it: the invariant is that recording quality is untouched by the
//! assist feature.

mod aec;
mod pipeline;
mod resample;
mod segment;
#[cfg(feature = "silero")]
mod silero;
mod stt_worker;
#[cfg(feature = "whisper")]
mod whisper;

pub use aec::{EchoCanceller, EchoCancellerConfig};
pub use pipeline::{spawn_assist_pipeline, AssistPipeline, AssistPipelineConfig};
pub use resample::StreamResampler;
pub use segment::{EnergyVad, FrameVad, Segment, Segmenter, SegmenterConfig};
#[cfg(feature = "silero")]
pub use silero::SileroVad;
pub use stt_worker::{
    spawn_stt_worker, RecognizedUtterance, SegmentTranscriber, SttChannel, SttWorkerConfig,
    SttWorkerHandle,
};
#[cfg(feature = "whisper")]
pub use whisper::WhisperTranscriber;
