//! `meeting_transcripts` table — batch Whisper transcripts, 1:1 with
//! `meetings`.
//!
//! gilb-transcribe owns these rows: on a successful upload it [`upsert_transcript`]s
//! the text + segments; after retries are exhausted it records the failure via
//! [`set_transcript_error`]. `created_at` is epoch-ms, matching the rest of the
//! schema. See `0005_meeting_transcripts.sql`.

use anyhow::Result;
use chrono::Utc;

use crate::Db;

/// A `meeting_transcripts` row. Mirrors the columns in
/// `0005_meeting_transcripts.sql`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Transcript {
    pub meeting_id: i64,
    pub text: Option<String>,
    pub segments_json: Option<String>,
    pub model: Option<String>,
    pub created_at: i64,
    pub error: Option<String>,
}

/// Store a successful transcription for `meeting_id`, replacing any prior row
/// (including a previous error). `created_at` is stamped now (epoch-ms) and
/// `error` is cleared.
pub async fn upsert_transcript(
    db: &Db,
    meeting_id: i64,
    text: &str,
    segments_json: &str,
    model: &str,
) -> Result<()> {
    let now = Utc::now().timestamp_millis();
    sqlx::query(
        "INSERT INTO meeting_transcripts \
             (meeting_id, text, segments_json, model, created_at, error) \
         VALUES (?, ?, ?, ?, ?, NULL) \
         ON CONFLICT(meeting_id) DO UPDATE SET \
             text = excluded.text, \
             segments_json = excluded.segments_json, \
             model = excluded.model, \
             created_at = excluded.created_at, \
             error = NULL",
    )
    .bind(meeting_id)
    .bind(text)
    .bind(segments_json)
    .bind(model)
    .bind(now)
    .execute(db)
    .await?;
    Ok(())
}

/// Record a persistent transcription failure for `meeting_id` (after retries are
/// exhausted), replacing any prior row. `created_at` is stamped now (epoch-ms).
pub async fn set_transcript_error(db: &Db, meeting_id: i64, error: &str) -> Result<()> {
    let now = Utc::now().timestamp_millis();
    sqlx::query(
        "INSERT INTO meeting_transcripts \
             (meeting_id, text, segments_json, model, created_at, error) \
         VALUES (?, NULL, NULL, NULL, ?, ?) \
         ON CONFLICT(meeting_id) DO UPDATE SET \
             text = NULL, \
             segments_json = NULL, \
             model = NULL, \
             created_at = excluded.created_at, \
             error = excluded.error",
    )
    .bind(meeting_id)
    .bind(now)
    .bind(error)
    .execute(db)
    .await?;
    Ok(())
}

/// Fetch a meeting's transcript, or `None` if it has not been transcribed yet.
pub async fn get_transcript(db: &Db, meeting_id: i64) -> Result<Option<Transcript>> {
    let row = sqlx::query_as::<_, Transcript>(
        "SELECT meeting_id, text, segments_json, model, created_at, error \
         FROM meeting_transcripts WHERE meeting_id = ?",
    )
    .bind(meeting_id)
    .fetch_optional(db)
    .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meetings::insert_meeting;
    use crate::open_db;
    use std::env;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn temp_db_path() -> PathBuf {
        let mut p = env::temp_dir();
        p.push(format!("gilb-transcripts-test-{}.sqlite", Uuid::new_v4()));
        p
    }

    fn cleanup(path: &PathBuf) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    }

    #[tokio::test]
    async fn upsert_then_get_roundtrips() {
        let path = temp_db_path();
        let db = open_db(&path).await.expect("open_db");
        let mid = insert_meeting(&db, 0, "us.zoom.xos").await.expect("insert");

        assert!(get_transcript(&db, mid).await.expect("get").is_none());

        upsert_transcript(&db, mid, "hello world", "[]", "whisper-1")
            .await
            .expect("upsert");
        let t = get_transcript(&db, mid).await.expect("get").expect("row");
        assert_eq!(t.text.as_deref(), Some("hello world"));
        assert_eq!(t.segments_json.as_deref(), Some("[]"));
        assert_eq!(t.model.as_deref(), Some("whisper-1"));
        assert!(t.error.is_none());

        db.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn error_then_upsert_clears_error() {
        let path = temp_db_path();
        let db = open_db(&path).await.expect("open_db");
        let mid = insert_meeting(&db, 0, "us.zoom.xos").await.expect("insert");

        set_transcript_error(&db, mid, "boom").await.expect("err");
        let t = get_transcript(&db, mid).await.expect("get").expect("row");
        assert_eq!(t.error.as_deref(), Some("boom"));
        assert!(t.text.is_none());

        // A later success overwrites the error row.
        upsert_transcript(&db, mid, "ok", "[]", "whisper-1")
            .await
            .expect("upsert");
        let t = get_transcript(&db, mid).await.expect("get").expect("row");
        assert_eq!(t.text.as_deref(), Some("ok"));
        assert!(t.error.is_none());

        db.close().await;
        cleanup(&path);
    }
}
