//! Downloading the local whisper model.
//!
//! One implementation, two callers: post-meeting transcription downloads it
//! from the settings screen, and real-time suggestions download it when the
//! feature is switched on. They used to have a copy each — same `.part` +
//! rename, same cancel flag, same throttled progress — with the URL written out
//! twice, which is exactly how one of them ends up a version behind.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::AsyncWriteExt;

/// Progress is reported at most this often, in bytes downloaded. A UI cannot
/// use more than that and every event crosses the webview boundary.
const REPORT_EVERY_BYTES: u64 = 4_000_000;

/// A stuck connection must error out, not hang: with no timeout a stalled
/// socket leaves the download task alive forever, and the cancel flag — only
/// checked between chunks — never gets read, so the feature cannot even be
/// toggled off. The read timeout bounds each socket read, not the whole
/// transfer, so a slow-but-moving 570 MB download still finishes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

pub enum Downloaded {
    /// Bytes written; the file is in place.
    Completed(u64),
    /// `cancel` was raised. The partial file is gone.
    Cancelled,
}

/// Stream `url` into `final_path` through a `.part` sibling, renamed only on
/// success — a partial or abandoned download must never be mistaken for a
/// usable model. `cancel` is checked between chunks, so turning the feature off
/// stops paying for ~570 MB immediately.
pub async fn download(
    url: &str,
    final_path: &Path,
    cancel: &AtomicBool,
    on_progress: impl Fn(u64, u64) + Send,
) -> Result<Downloaded> {
    let part_path = final_path.with_extension("part");
    let result: Result<Downloaded> = async {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .context("build model download client")?;
        let mut resp = client
            .get(url)
            .send()
            .await
            .context("start model download")?;
        if !resp.status().is_success() {
            bail!("model download failed: HTTP {}", resp.status());
        }
        let total = resp.content_length().unwrap_or(0);
        let mut file = tokio::fs::File::create(&part_path)
            .await
            .with_context(|| format!("create {}", part_path.display()))?;
        let mut downloaded = 0u64;
        let mut last_report = 0u64;
        while let Some(chunk) = resp.chunk().await.context("read download chunk")? {
            if cancel.load(Ordering::SeqCst) {
                return Ok(Downloaded::Cancelled);
            }
            file.write_all(&chunk)
                .await
                .context("write download chunk")?;
            downloaded += chunk.len() as u64;
            if downloaded - last_report >= REPORT_EVERY_BYTES {
                last_report = downloaded;
                on_progress(downloaded, total);
            }
        }
        file.flush().await.context("flush download")?;
        drop(file);
        tokio::fs::rename(&part_path, final_path)
            .await
            .context("move downloaded model into place")?;
        Ok(Downloaded::Completed(downloaded))
    }
    .await;

    if !matches!(result, Ok(Downloaded::Completed(_))) {
        let _ = tokio::fs::remove_file(&part_path).await;
    }
    result
}
