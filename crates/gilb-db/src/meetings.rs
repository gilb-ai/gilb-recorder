//! `meetings` table — video/audio call sessions recorded alongside a11y
//! capture.
//!
//! Row creation (the `recording` row + the actions back-link) is the engine's
//! job; this module only owns the recording fields that the recorder updates:
//! the on-disk paths on capture start, and `ended_at` + the terminal `status`
//! on stop. Timestamps are epoch-ms (INTEGER), matching `0004_meetings.sql`.

use anyhow::Result;

use crate::Db;

/// A `meetings` row. Mirrors the columns in `0004_meetings.sql`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Meeting {
    pub id: i64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub app: String,
    pub video_path: Option<String>,
    pub audio_path: Option<String>,
    pub status: String,
}

/// Point a meeting at its on-disk recordings (set once capture has started).
pub async fn set_recording_paths(
    db: &Db,
    id: i64,
    video_path: &str,
    audio_path: &str,
) -> Result<()> {
    sqlx::query("UPDATE meetings SET video_path = ?, audio_path = ? WHERE id = ?")
        .bind(video_path)
        .bind(audio_path)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Close out a meeting: stamp `ended_at` (epoch-ms) and set the terminal
/// `status` — one of `completed`, `cancelled`, `failed` (the CHECK constraint
/// rejects anything else).
pub async fn finish_meeting(db: &Db, id: i64, ended_at_ms: i64, status: &str) -> Result<()> {
    sqlx::query("UPDATE meetings SET ended_at = ?, status = ? WHERE id = ?")
        .bind(ended_at_ms)
        .bind(status)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Insert a fresh `meetings` row in the default `recording` state, returning
/// its id. Production creates rows in the engine; this exists for tests and
/// bring-up of the recorder.
pub async fn insert_meeting(db: &Db, started_at_ms: i64, app: &str) -> Result<i64> {
    let res = sqlx::query("INSERT INTO meetings (started_at, app) VALUES (?, ?)")
        .bind(started_at_ms)
        .bind(app)
        .execute(db)
        .await?;
    Ok(res.last_insert_rowid())
}

/// Meeting ids that still need transcribing: completed, with an audio path, and
/// lacking a successful transcript (no `meeting_transcripts` row, or one whose
/// `text` is NULL — i.e. a prior failure). Oldest first, for FIFO catch-up. Used
/// by the transcription worker's launch/after-download sweep.
pub async fn pending_transcriptions(db: &Db) -> Result<Vec<i64>> {
    let rows = sqlx::query_scalar::<_, i64>(
        "SELECT m.id FROM meetings m \
         LEFT JOIN meeting_transcripts t ON t.meeting_id = m.id \
         WHERE m.status = 'completed' \
           AND m.audio_path IS NOT NULL \
           AND (t.meeting_id IS NULL OR t.text IS NULL) \
         ORDER BY m.started_at ASC",
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Fetch a meeting by id, or `None` if it does not exist.
pub async fn get_meeting(db: &Db, id: i64) -> Result<Option<Meeting>> {
    let row = sqlx::query_as::<_, Meeting>(
        "SELECT id, started_at, ended_at, app, video_path, audio_path, status \
         FROM meetings WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_db;
    use std::env;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn temp_db_path() -> PathBuf {
        let mut p = env::temp_dir();
        p.push(format!("gilb-meetings-test-{}.sqlite", Uuid::new_v4()));
        p
    }

    fn cleanup(path: &PathBuf) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    }

    #[tokio::test]
    async fn insert_then_paths_then_finish() {
        let path = temp_db_path();
        let db = open_db(&path).await.expect("open_db");

        let id = insert_meeting(&db, 1_000, "us.zoom.xos")
            .await
            .expect("insert_meeting");

        let m = get_meeting(&db, id).await.expect("get").expect("present");
        assert_eq!(m.status, "recording");
        assert_eq!(m.app, "us.zoom.xos");
        assert_eq!(m.started_at, 1_000);
        assert!(m.ended_at.is_none());
        assert!(m.video_path.is_none());

        set_recording_paths(&db, id, "/m/1.mp4", "/m/1.audio.wav")
            .await
            .expect("set_recording_paths");
        let m = get_meeting(&db, id).await.expect("get").expect("present");
        assert_eq!(m.video_path.as_deref(), Some("/m/1.mp4"));
        assert_eq!(m.audio_path.as_deref(), Some("/m/1.audio.wav"));
        assert_eq!(m.status, "recording");

        finish_meeting(&db, id, 5_000, "completed")
            .await
            .expect("finish_meeting");
        let m = get_meeting(&db, id).await.expect("get").expect("present");
        assert_eq!(m.ended_at, Some(5_000));
        assert_eq!(m.status, "completed");

        db.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn finish_cancelled_status_is_accepted() {
        let path = temp_db_path();
        let db = open_db(&path).await.expect("open_db");

        let id = insert_meeting(&db, 0, "com.tinyspeck.slackmacgap")
            .await
            .expect("insert_meeting");
        finish_meeting(&db, id, 42, "cancelled")
            .await
            .expect("finish_meeting cancelled");
        let m = get_meeting(&db, id).await.expect("get").expect("present");
        assert_eq!(m.status, "cancelled");
        assert_eq!(m.ended_at, Some(42));

        db.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn get_missing_meeting_is_none() {
        let path = temp_db_path();
        let db = open_db(&path).await.expect("open_db");
        assert!(get_meeting(&db, 999).await.expect("get").is_none());
        db.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn pending_transcriptions_filters_correctly() {
        let path = temp_db_path();
        let db = open_db(&path).await.expect("open_db");

        // A: completed + audio, no transcript → pending.
        let a = insert_meeting(&db, 1, "z").await.unwrap();
        set_recording_paths(&db, a, "/v.mp4", "/a.wav")
            .await
            .unwrap();
        finish_meeting(&db, a, 2, "completed").await.unwrap();

        // B: still recording (not completed) → not pending.
        let b = insert_meeting(&db, 3, "z").await.unwrap();
        set_recording_paths(&db, b, "/v.mp4", "/a.wav")
            .await
            .unwrap();

        // C: completed with a successful transcript → not pending.
        let c = insert_meeting(&db, 4, "z").await.unwrap();
        set_recording_paths(&db, c, "/v.mp4", "/a.wav")
            .await
            .unwrap();
        finish_meeting(&db, c, 5, "completed").await.unwrap();
        crate::transcripts::upsert_transcript(&db, c, "hi", "[]", "m")
            .await
            .unwrap();

        // D: completed but transcription errored (text NULL) → pending.
        let d = insert_meeting(&db, 6, "z").await.unwrap();
        set_recording_paths(&db, d, "/v.mp4", "/a.wav")
            .await
            .unwrap();
        finish_meeting(&db, d, 7, "completed").await.unwrap();
        crate::transcripts::set_transcript_error(&db, d, "boom")
            .await
            .unwrap();

        let pending = pending_transcriptions(&db).await.unwrap();
        assert_eq!(pending, vec![a, d], "only A and D pending, oldest first");
        let _ = b;

        db.close().await;
        cleanup(&path);
    }
}
