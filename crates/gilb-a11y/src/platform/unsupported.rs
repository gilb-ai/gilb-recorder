//! Fallback when the host OS has no gilb capture backend.

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use crate::{CapturePlatform, Permissions, RunningCapture, StartContext};

pub struct UnsupportedPlatform;

#[async_trait]
impl CapturePlatform for UnsupportedPlatform {
    fn name(&self) -> &'static str {
        "unsupported"
    }

    async fn permissions(&self) -> Permissions {
        Permissions::default()
    }

    async fn start(&self, _ctx: StartContext) -> Result<RunningCapture> {
        Err(anyhow!(
            "gilb capture is only supported on macOS and Windows"
        ))
    }
}
