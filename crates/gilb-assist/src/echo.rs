//! Stub backend that echoes its input back (Ш5 of the plan): lets the whole
//! pipeline — STT → engine → overlay — run end-to-end before a real provider
//! exists. Also handy in the overlay UI's development loop.

use anyhow::Result;
use async_trait::async_trait;

use crate::{AssistBackend, AssistConfig, AssistSession};

pub struct EchoBackend;

#[async_trait]
impl AssistBackend for EchoBackend {
    async fn begin(&self, system_prompt: &str) -> Result<Box<dyn AssistSession>> {
        Ok(Box::new(EchoSession { turns_seen: 0, prompt_len: system_prompt.len() }))
    }
}

struct EchoSession {
    turns_seen: usize,
    prompt_len: usize,
}

#[async_trait]
impl AssistSession for EchoSession {
    async fn send(&mut self, input: &str) -> Result<Option<String>> {
        self.turns_seen += 1;
        Ok(Some(format!(
            "**echo #{}** (prompt {} chars)\n\n{input}",
            self.turns_seen, self.prompt_len
        )))
    }
}

/// Fixed config for running the stub without any server or file.
pub struct StaticConfig {
    pub system_prompt: String,
    pub enabled: bool,
    pub turns_before_analysis: u32,
}

impl Default for StaticConfig {
    fn default() -> Self {
        Self {
            system_prompt: "echo".into(),
            enabled: true,
            turns_before_analysis: 1,
        }
    }
}

#[async_trait]
impl AssistConfig for StaticConfig {
    async fn system_prompt(&self) -> Result<String> {
        Ok(self.system_prompt.clone())
    }

    async fn enabled(&self) -> bool {
        self.enabled
    }

    async fn turns_before_analysis(&self) -> u32 {
        self.turns_before_analysis
    }
}
