//! The meeting's own record of what the assistant was asked and said.
//!
//! Two files in the meeting folder, next to `video.mp4` and `transcript.json`:
//! `assist.json` for anything that wants to read it back, `assist.txt` for a
//! person opening the folder. Deliberately not the database — a meeting is a
//! folder on disk, and its record should survive being copied, mailed or
//! opened on a machine that has never run this app.
//!
//! Best-effort throughout: a failure here must never cost the user a
//! suggestion. The panel is the product; these files are a record of it.
//!
//! ## Why the path is resolved late
//!
//! The meeting *id* is known the moment a recording arms; its *folder* is not.
//! The recorder writes the paths into the meetings row from its own subscriber
//! to the same `Armed` event, so a journal that resolved the folder on arrival
//! raced it — and lost, every time, silently filing every meeting's
//! suggestions into `None`. The id is what arming actually gives us, so that
//! is what is kept, and the folder is looked up on the first entry (by which
//! point a suggestion has taken seconds to arrive) and then cached.

use std::path::PathBuf;

use tauri::AppHandle;
use tracing::{info, warn};

use super::state;

/// What produced an entry. Ordered as a reader meets them: the question first.
#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Kind {
    /// The user typed or spoke it into the panel.
    Question,
    /// The assistant offered it.
    Suggestion,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Question => "Question",
            Kind::Suggestion => "Suggestion",
        }
    }
}

#[derive(Clone, serde::Serialize)]
struct Entry {
    /// Local wall clock, RFC 3339 with offset — the same clock the folder name
    /// is stamped in, so an entry can be lined up with the recording by eye.
    at: String,
    kind: Kind,
    text: String,
}

/// The journal for the meeting now recording. Empty between meetings, so a
/// suggestion that arrives after one ends is never filed under it.
#[derive(Default)]
pub(super) struct Journal {
    meeting_id: Option<i64>,
    /// Resolved on the first entry, then reused (see the module docs).
    dir: Option<PathBuf>,
    /// Every entry so far. Held because both files are rewritten whole on each
    /// one: appending to JSON means rewriting it anyway, and rewriting both
    /// from one list is what keeps them from disagreeing.
    entries: Vec<Entry>,
}

pub(super) fn open_journal(app: &AppHandle, meeting_id: i64) {
    if let Some(state) = state(app) {
        *state.journal.lock() = Journal {
            meeting_id: Some(meeting_id),
            dir: None,
            entries: Vec::new(),
        };
    }
}

pub(super) fn close_journal(app: &AppHandle) {
    if let Some(state) = state(app) {
        *state.journal.lock() = Journal::default();
    }
}

/// Record one entry and rewrite both files.
///
/// Stamped here, written on the blocking pool: callers are the engine's event
/// loop and a command handler, and resolving the folder reaches the database
/// while writing reaches the disk. Neither belongs on an async worker, and a
/// journal must never be what makes a suggestion late.
pub(super) fn journal(app: &AppHandle, kind: Kind, text: &str) {
    let app = app.clone();
    let entry = Entry {
        at: chrono::Local::now().to_rfc3339(),
        kind,
        text: text.trim().to_string(),
    };
    tauri::async_runtime::spawn_blocking(move || record(&app, entry));
}

fn record(app: &AppHandle, entry: Entry) {
    let Some(state) = state(app) else { return };
    let mut journal = state.journal.lock();
    let Some(meeting_id) = journal.meeting_id else {
        return; // nothing is recording — not this meeting's business
    };
    if journal.dir.is_none() {
        journal.dir = state.host.journal_dir(meeting_id);
        match &journal.dir {
            Some(dir) => info!(meeting_id, dir = %dir.display(), "assist journal"),
            // Worth a line: the feature is working and its record is not.
            None => warn!(meeting_id, "assist journal: no folder for this meeting"),
        }
    }
    let Some(dir) = journal.dir.clone() else {
        return;
    };

    journal.entries.push(entry);
    // Two entries can reach the pool out of order; the stamp is taken at the
    // call, so ordering by it restores what the user saw.
    journal.entries.sort_by(|a, b| a.at.cmp(&b.at));

    let json = serde_json::json!({ "meeting_id": meeting_id, "entries": journal.entries });
    write(&dir.join("assist.json"), &format!("{json:#}\n"));
    write(&dir.join("assist.txt"), &plain_text(&journal.entries));
}

/// The human-readable half: stamp, what it was, the text.
fn plain_text(entries: &[Entry]) -> String {
    let mut out = String::new();
    for entry in entries {
        // Time of day only — the date is the folder's name.
        let stamp = entry.at.get(11..19).unwrap_or(&entry.at);
        out.push_str(&format!(
            "[{stamp}] {}\n{}\n\n",
            entry.kind.label(),
            entry.text
        ));
    }
    out
}

/// Write via a temporary file and rename, so a crash mid-write leaves the
/// previous entries intact rather than a truncated file. Whole-file rewrites
/// are affordable here: a meeting produces a handful of entries, not a stream.
fn write(path: &std::path::Path, contents: &str) {
    let tmp = path.with_extension("tmp");
    if let Err(err) = std::fs::write(&tmp, contents) {
        warn!(error = %err, path = %tmp.display(), "assist journal write failed");
        return;
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        warn!(error = %err, path = %path.display(), "assist journal rename failed");
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(at: &str, kind: Kind, text: &str) -> Entry {
        Entry {
            at: at.to_string(),
            kind,
            text: text.to_string(),
        }
    }

    #[test]
    fn plain_text_reads_as_a_record_of_the_meeting() {
        let out = plain_text(&[
            entry(
                "2026-07-31T14:10:47+03:00",
                Kind::Question,
                "When was Pushkin born?",
            ),
            entry("2026-07-31T14:10:52+03:00", Kind::Suggestion, "1799."),
        ]);
        assert_eq!(
            out,
            "[14:10:47] Question\nWhen was Pushkin born?\n\n[14:10:52] Suggestion\n1799.\n\n"
        );
    }

    /// A malformed stamp must still produce a line: the record is best-effort,
    /// and dropping an entry to keep a format tidy is the wrong trade.
    #[test]
    fn plain_text_survives_a_stamp_it_cannot_slice() {
        let out = plain_text(&[entry("?", Kind::Suggestion, "still written")]);
        assert!(out.contains("still written"), "got {out:?}");
    }

    #[test]
    fn entries_serialize_with_a_readable_kind() {
        let json = serde_json::to_string(&entry("t", Kind::Question, "q")).expect("serialize");
        assert!(json.contains(r#""kind":"question""#), "got {json}");
    }
}
