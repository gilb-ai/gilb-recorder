//! The meeting's own record of what the assistant said and was asked.
//!
//! `assist.md` next to `video.mp4` and `audio.wav`: wall-clock-stamped
//! questions and suggestions, opened when a recording arms and closed when it
//! ends, so a suggestion between meetings is never filed under the previous
//! one. Best-effort and synchronous — one short line on a local disk, and a
//! failure here must never cost the user a suggestion: the panel is the
//! product, the file is a record of it. Markdown because a meeting folder is
//! something people open.

use std::io::Write;

use tauri::AppHandle;
use tracing::{info, warn};

use super::state;

pub(super) fn open_journal(app: &AppHandle, meeting_id: i64) {
    let Some(state) = state(app) else { return };
    let path = state.host.journal_path(meeting_id);
    if let Some(path) = &path {
        info!(path = %path.display(), "assist journal");
    }
    *state.journal.lock() = path;
}

pub(super) fn close_journal(app: &AppHandle) {
    if let Some(state) = state(app) {
        *state.journal.lock() = None;
    }
}

pub(super) fn journal(app: &AppHandle, heading: &str, body: &str) {
    let Some(path) = state(app).and_then(|s| s.journal.lock().clone()) else {
        return;
    };
    let stamp = chrono::Local::now().format("%H:%M:%S");
    let entry = format!("\n## {stamp} · {heading}\n\n{}\n", body.trim());
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);
    match opened {
        Ok(mut file) => {
            if let Err(err) = file.write_all(entry.as_bytes()) {
                warn!(error = %err, path = %path.display(), "assist journal write failed");
            }
        }
        Err(err) => warn!(error = %err, path = %path.display(), "assist journal open failed"),
    }
}
