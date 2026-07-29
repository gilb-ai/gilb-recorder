//! `SegmentTranscriber` over whisper.cpp (gilb-transcribe's `LocalTranscriber`).
//!
//! The model (~570 MB) is loaded lazily on the first segment — loading happens
//! on the blocking pool, since it's a large synchronous read — kept warm while
//! segments flow, and dropped again when the worker reports idle (the
//! [`crate::SttWorkerConfig::idle_unload`] timer). Between meetings the memory
//! is free.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use gilb_transcribe::{LocalTranscriber, VoicedMask};
use tracing::info;

use crate::{Segment, SegmentTranscriber};

pub struct WhisperTranscriber {
    model_path: PathBuf,
    /// "auto" | "ru" | "en" — passed through to whisper.
    language: String,
    model: Option<Arc<LocalTranscriber>>,
}

impl WhisperTranscriber {
    /// `model_path` must point at a downloaded ggml model (its presence gates
    /// the feature, mirroring gilb's transcription).
    pub fn new(model_path: PathBuf, language: impl Into<String>) -> Self {
        Self {
            model_path,
            language: language.into(),
            model: None,
        }
    }

    async fn model(&mut self) -> Result<Arc<LocalTranscriber>> {
        if let Some(model) = &self.model {
            return Ok(model.clone());
        }
        let path = self.model_path.clone();
        let language = self.language.clone();
        info!(model = %path.display(), "loading whisper model for realtime STT");
        let loaded = tokio::task::spawn_blocking(move || LocalTranscriber::new(&path, language))
            .await
            .context("whisper load join")??;
        let model = Arc::new(loaded);
        self.model = Some(model.clone());
        Ok(model)
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
        let utterances = model.transcribe_buffer_masked(segment.samples, mask).await?;
        Ok(utterances
            .into_iter()
            .map(|u| u.text)
            .collect::<Vec<_>>()
            .join(" "))
    }

    fn unload(&mut self) {
        if self.model.take().is_some() {
            info!("unloading idle whisper model");
        }
    }
}
