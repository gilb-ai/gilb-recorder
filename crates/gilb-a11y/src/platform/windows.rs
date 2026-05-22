//! Windows backend stub. Real implementation lands in Phase 6
//! (SetWindowsHookEx + UI Automation).

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use crate::{CapturePlatform, Permissions, RunningCapture, StartContext};

pub struct WindowsPlatform;

impl WindowsPlatform {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl CapturePlatform for WindowsPlatform {
    fn name(&self) -> &'static str {
        "windows-stub"
    }

    async fn permissions(&self) -> Permissions {
        Permissions::default()
    }

    async fn start(&self, _ctx: StartContext) -> Result<RunningCapture> {
        Err(anyhow!(
            "Windows capture backend is not implemented yet (Phase 6)"
        ))
    }
}
