//! `SegmentTranscriber` over whisper.cpp (gilb-transcribe's `LocalTranscriber`).
//!
//! The model (~570 MB) is loaded lazily on the first segment and shared with
//! whoever else in this process wants it — post-meeting transcription, above
//! all. Two private copies is what a machine gets otherwise, guaranteed, on
//! every meeting that ends while suggestions are still warm
//! (`gilb_transcribe::SharedModel`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use gilb_transcribe::{LocalTranscriber, SharedModel, VoicedMask, DEFAULT_IDLE_UNLOAD};
use tracing::info;

use crate::{Segment, SegmentTranscriber};

pub struct WhisperTranscriber {
    model_path: PathBuf,
    /// "auto" | "ru" | "en" — passed through to whisper.
    language: String,
    shared: Arc<SharedModel<LocalTranscriber>>,
}

impl WhisperTranscriber {
    /// `model_path` must point at a downloaded ggml model (its presence gates
    /// the feature, mirroring gilb's transcription).
    pub fn new(model_path: PathBuf, language: impl Into<String>) -> Self {
        Self::with_shared(model_path, language, SharedModel::new(DEFAULT_IDLE_UNLOAD))
    }

    /// Share one loaded model with another worker (post-meeting transcription).
    /// The app owns the [`SharedModel`] and hands the same handle to both.
    pub fn with_shared(
        model_path: PathBuf,
        language: impl Into<String>,
        shared: Arc<SharedModel<LocalTranscriber>>,
    ) -> Self {
        Self {
            model_path,
            language: language.into(),
            shared,
        }
    }

    async fn model(&mut self) -> Result<Arc<LocalTranscriber>> {
        let path = self.model_path.clone();
        let language = self.language.clone();
        // The language keys the shared cache: a model loaded for another
        // consumer's language must never serve these segments.
        let key = language.clone();
        self.shared
            .get(&key, move || {
                info!(model = %path.display(), "loading whisper model");
                LocalTranscriber::new(&path, language).context("load whisper model")
            })
            .await
    }
}

#[async_trait]
impl SegmentTranscriber for WhisperTranscriber {
    async fn transcribe(&mut self, segment: Segment) -> Result<String> {
        let model = self.model().await?;
        // Single VAD pass: the segmenter's frame decisions drive the
        // anti-hallucination filters — no re-detection here.
        let mask = VoicedMask {
            frame_size: segment.vad_frame_size,
            frames: segment.voiced,
        };
        let utterances = model
            .transcribe_buffer_masked(segment.samples, mask)
            .await?;
        Ok(utterances
            .into_iter()
            .map(|u| u.text)
            .collect::<Vec<_>>()
            .join(" "))
    }

    /// Idle here means "the suggestion worker has nothing to do", which is not
    /// the same as "nobody needs the model": post-meeting transcription may be
    /// running right now. So this asks the shared owner, which drops its
    /// reference only if its own idle window has passed — and the memory
    /// returns when the last holder lets go.
    fn unload(&mut self) {
        self.shared.unload_if_idle();
    }
}
