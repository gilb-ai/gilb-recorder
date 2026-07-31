//! SQL-backed query functions used by MCP tool handlers.
//!
//! All text fields are password-masked at the SQL layer via
//! `CASE WHEN password_flag = 1 THEN '[masked]' ELSE … END` — defense in depth
//! on top of the capture-time `password_flag`.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{Row, SqlitePool};

use crate::range::ResolvedRange;

// ---------- sessions ----------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct SessionRow {
    pub id: i64,
    pub started_at: String,
    pub stopped_at: Option<String>,
    pub gilb_version: String,
    pub host_os: String,
    pub stop_reason: Option<String>,
    pub action_count: i64,
}

pub async fn list_sessions(
    db: &SqlitePool,
    range: Option<ResolvedRange>,
    limit: i64,
) -> Result<Vec<SessionRow>> {
    let (from, to) = range.map(|r| r.as_strings()).unzip();
    let rows = sqlx::query(
        r#"
        SELECT
          s.id, s.started_at, s.stopped_at, s.gilb_version, s.host_os, s.stop_reason,
          (SELECT COUNT(*) FROM actions a WHERE a.session_id = s.id) AS action_count
        FROM sessions s
        WHERE (?1 IS NULL OR s.started_at >= ?1)
          AND (?2 IS NULL OR s.started_at <  ?2)
        ORDER BY s.started_at DESC
        LIMIT ?3
        "#,
    )
    .bind(from)
    .bind(to)
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| SessionRow {
            id: r.get("id"),
            started_at: r.get("started_at"),
            stopped_at: r.get("stopped_at"),
            gilb_version: r.get("gilb_version"),
            host_os: r.get("host_os"),
            stop_reason: r.get("stop_reason"),
            action_count: r.get("action_count"),
        })
        .collect())
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub row: SessionRow,
    pub per_kind: Vec<KindCount>,
    pub top_apps: Vec<AppRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct KindCount {
    pub kind: String,
    pub count: i64,
}

pub async fn get_session(db: &SqlitePool, session_id: i64) -> Result<Option<SessionDetail>> {
    let head = sqlx::query(
        r#"
        SELECT
          s.id, s.started_at, s.stopped_at, s.gilb_version, s.host_os, s.stop_reason,
          (SELECT COUNT(*) FROM actions a WHERE a.session_id = s.id) AS action_count
        FROM sessions s
        WHERE s.id = ?1
        "#,
    )
    .bind(session_id)
    .fetch_optional(db)
    .await?;
    let Some(head) = head else { return Ok(None) };

    // `kinds` and `top_apps` are independent reads; run them concurrently to
    // halve wall-clock latency. The read-only pool has 4 connections, so two
    // parallel queries comfortably fit.
    let kinds_fut = sqlx::query(
        r#"
        SELECT kind, COUNT(*) AS count
        FROM actions
        WHERE session_id = ?1
        GROUP BY kind
        ORDER BY count DESC
        "#,
    )
    .bind(session_id)
    .fetch_all(db);

    let top_apps_fut = sqlx::query(
        r#"
        SELECT
          app_bundle_id, app_name,
          COUNT(*) AS action_count,
          COUNT(DISTINCT window_title) AS distinct_windows,
          MIN(captured_at) AS first_seen,
          MAX(captured_at) AS last_seen
        FROM actions
        WHERE session_id = ?1 AND app_name IS NOT NULL
        GROUP BY app_bundle_id, app_name
        ORDER BY action_count DESC
        LIMIT 5
        "#,
    )
    .bind(session_id)
    .fetch_all(db);

    let (kinds_rows, top_apps_rows) = tokio::try_join!(kinds_fut, top_apps_fut)?;

    let kinds = kinds_rows
        .into_iter()
        .map(|r| KindCount {
            kind: r.get("kind"),
            count: r.get("count"),
        })
        .collect();
    let top_apps = top_apps_rows.into_iter().map(row_to_app).collect();

    Ok(Some(SessionDetail {
        row: SessionRow {
            id: head.get("id"),
            started_at: head.get("started_at"),
            stopped_at: head.get("stopped_at"),
            gilb_version: head.get("gilb_version"),
            host_os: head.get("host_os"),
            stop_reason: head.get("stop_reason"),
            action_count: head.get("action_count"),
        },
        per_kind: kinds,
        top_apps,
    }))
}

// ---------- apps --------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct AppRow {
    pub app_bundle_id: Option<String>,
    pub app_name: Option<String>,
    pub action_count: i64,
    pub distinct_windows: i64,
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
}

fn row_to_app(r: sqlx::sqlite::SqliteRow) -> AppRow {
    AppRow {
        app_bundle_id: r.try_get("app_bundle_id").ok(),
        app_name: r.try_get("app_name").ok(),
        action_count: r.get("action_count"),
        distinct_windows: r.get("distinct_windows"),
        first_seen: r.try_get("first_seen").ok(),
        last_seen: r.try_get("last_seen").ok(),
    }
}

pub async fn list_apps(db: &SqlitePool, range: ResolvedRange, limit: i64) -> Result<Vec<AppRow>> {
    let (from, to) = range.as_strings();
    let rows = sqlx::query(
        r#"
        SELECT
          app_bundle_id, app_name,
          COUNT(*) AS action_count,
          COUNT(DISTINCT window_title) AS distinct_windows,
          MIN(captured_at) AS first_seen,
          MAX(captured_at) AS last_seen
        FROM actions
        WHERE captured_at >= ?1 AND captured_at < ?2
          AND app_name IS NOT NULL
        GROUP BY app_bundle_id, app_name
        ORDER BY action_count DESC
        LIMIT ?3
        "#,
    )
    .bind(from)
    .bind(to)
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(rows.into_iter().map(row_to_app).collect())
}

// ---------- actions -----------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ActionRow {
    pub id: i64,
    pub session_id: i64,
    pub captured_at: String,
    pub kind: String,
    pub app_name: Option<String>,
    pub app_bundle_id: Option<String>,
    pub window_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_url: Option<String>,
    pub element_role: Option<String>,
    pub element_name: Option<String>,
    pub element_value: Option<String>,
    pub text_content: Option<String>,
    pub password_flag: bool,
    pub extra: Option<serde_json::Value>,
}

fn row_to_action(r: sqlx::sqlite::SqliteRow) -> ActionRow {
    let extra: Option<String> = r.try_get("extra_json").ok();
    let extra = extra.and_then(|s| serde_json::from_str(&s).ok());
    let pwd: i64 = r.get("password_flag");
    ActionRow {
        id: r.get("id"),
        session_id: r.get("session_id"),
        captured_at: r.get("captured_at"),
        kind: r.get("kind"),
        app_name: r.try_get("app_name").ok(),
        app_bundle_id: r.try_get("app_bundle_id").ok(),
        window_title: r.try_get("window_title").ok(),
        browser_url: r.try_get("browser_url").ok(),
        element_role: r.try_get("element_role").ok(),
        element_name: r.try_get("element_name").ok(),
        element_value: r.try_get("element_value").ok(),
        text_content: r.try_get("text_content").ok(),
        password_flag: pwd != 0,
        extra,
    }
}

/// SQL fragment that masks `element_value` and `text_content` when
/// `password_flag = 1`. The masking happens column-side, so even ill-behaved
/// callers can't read the underlying text.
const ACTION_COLUMNS: &str = r#"
    id, session_id, captured_at, kind, app_bundle_id, app_name, window_title, browser_url,
    element_role, element_name,
    CASE WHEN password_flag = 1 THEN '[masked]' ELSE element_value END AS element_value,
    CASE WHEN password_flag = 1 THEN '[masked]' ELSE text_content  END AS text_content,
    password_flag, extra_json
"#;

pub async fn recent_actions(
    db: &SqlitePool,
    range: ResolvedRange,
    limit: i64,
) -> Result<Vec<ActionRow>> {
    let (from, to) = range.as_strings();
    let sql = format!(
        r#"
        SELECT {cols}
        FROM actions
        WHERE captured_at >= ?1 AND captured_at < ?2
        ORDER BY captured_at DESC
        LIMIT ?3
        "#,
        cols = ACTION_COLUMNS
    );
    let rows = sqlx::query(&sql)
        .bind(from)
        .bind(to)
        .bind(limit)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(row_to_action).collect())
}

pub async fn search_actions(
    db: &SqlitePool,
    q: &str,
    app: Option<&str>,
    kind: Option<&str>,
    range: ResolvedRange,
    limit: i64,
) -> Result<Vec<ActionRow>> {
    let (from, to) = range.as_strings();
    let like = format!("%{}%", q.replace('%', "\\%"));
    let sql = format!(
        r#"
        SELECT {cols}
        FROM actions
        WHERE captured_at >= ?1 AND captured_at < ?2
          AND (?3 IS NULL OR app_name = ?3 OR app_bundle_id = ?3)
          AND (?4 IS NULL OR kind = ?4)
          AND password_flag = 0
          AND (
            (text_content   IS NOT NULL AND text_content   LIKE ?5 ESCAPE '\') OR
            (element_name   IS NOT NULL AND element_name   LIKE ?5 ESCAPE '\') OR
            (element_value  IS NOT NULL AND element_value  LIKE ?5 ESCAPE '\') OR
            (window_title   IS NOT NULL AND window_title   LIKE ?5 ESCAPE '\')
          )
        ORDER BY captured_at DESC
        LIMIT ?6
        "#,
        cols = ACTION_COLUMNS
    );
    let rows = sqlx::query(&sql)
        .bind(from)
        .bind(to)
        .bind(app)
        .bind(kind)
        .bind(like)
        .bind(limit)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(row_to_action).collect())
}

// ---------- activity summary -------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ActivitySummary {
    pub range: SummaryRange,
    pub total_actions: i64,
    pub per_kind: Vec<KindCount>,
    pub apps: Vec<AppRow>,
    pub top_text_snippets: Vec<TextSnippet>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SummaryRange {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextSnippet {
    pub captured_at: String,
    pub app_name: Option<String>,
    pub window_title: Option<String>,
    pub text: String,
}

pub async fn activity_summary(db: &SqlitePool, range: ResolvedRange) -> Result<ActivitySummary> {
    let (from, to) = range.as_strings();

    // Three independent range scans — fire them concurrently to keep
    // wall-clock close to the slowest of the three rather than their sum.
    // `total_actions` is derived from `per_kind`, so we don't issue a 4th
    // `SELECT COUNT(*)` round-trip.
    let per_kind_fut = async {
        sqlx::query(
            r#"
            SELECT kind, COUNT(*) AS count
            FROM actions
            WHERE captured_at >= ?1 AND captured_at < ?2
            GROUP BY kind
            ORDER BY count DESC
            "#,
        )
        .bind(&from)
        .bind(&to)
        .fetch_all(db)
        .await
        .map_err(anyhow::Error::from)
    };

    let apps_fut = list_apps(db, range, 20);

    let snippets_fut = async {
        sqlx::query(
            r#"
            SELECT captured_at, app_name, window_title, text_content
            FROM actions
            WHERE captured_at >= ?1 AND captured_at < ?2
              AND kind = 'text'
              AND password_flag = 0
              AND text_content IS NOT NULL
              AND length(text_content) >= 4
            ORDER BY length(text_content) DESC, captured_at DESC
            LIMIT 10
            "#,
        )
        .bind(&from)
        .bind(&to)
        .fetch_all(db)
        .await
        .map_err(anyhow::Error::from)
    };

    let (per_kind_rows, apps, snippet_rows) =
        tokio::try_join!(per_kind_fut, apps_fut, snippets_fut)?;

    let per_kind: Vec<KindCount> = per_kind_rows
        .into_iter()
        .map(|r| KindCount {
            kind: r.get("kind"),
            count: r.get("count"),
        })
        .collect();
    let total: i64 = per_kind.iter().map(|k| k.count).sum();

    let snippets = snippet_rows
        .into_iter()
        .map(|r| TextSnippet {
            captured_at: r.get("captured_at"),
            app_name: r.try_get("app_name").ok(),
            window_title: r.try_get("window_title").ok(),
            text: r.get::<String, _>("text_content"),
        })
        .collect();

    Ok(ActivitySummary {
        range: SummaryRange { from, to },
        total_actions: total,
        per_kind,
        apps,
        top_text_snippets: snippets,
    })
}

// ---------- tree snapshots ---------------------------------------------

/// Lightweight `tree_snapshots` row — everything except `root_json`. Use
/// `get_tree_snapshot` to fetch the full tree for a single id.
#[derive(Debug, Clone, Serialize)]
pub struct TreeSnapshotMeta {
    pub id: i64,
    pub session_id: i64,
    pub captured_at: String,
    pub app_bundle_id: Option<String>,
    pub app_name: Option<String>,
    pub window_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_url: Option<String>,
    pub simhash: i64,
    pub json_bytes: i64,
}

fn row_to_snapshot_meta(r: sqlx::sqlite::SqliteRow) -> TreeSnapshotMeta {
    TreeSnapshotMeta {
        id: r.get("id"),
        session_id: r.get("session_id"),
        captured_at: r.get("captured_at"),
        app_bundle_id: r.try_get("app_bundle_id").ok(),
        app_name: r.try_get("app_name").ok(),
        window_title: r.try_get("window_title").ok(),
        browser_url: r.try_get("browser_url").ok(),
        simhash: r.get("simhash"),
        json_bytes: r.get("json_bytes"),
    }
}

pub async fn list_tree_snapshots(
    db: &SqlitePool,
    range: ResolvedRange,
    app: Option<&str>,
    limit: i64,
) -> Result<Vec<TreeSnapshotMeta>> {
    let (from, to) = range.as_strings();
    let rows = sqlx::query(
        r#"
        SELECT
          id, session_id, captured_at, app_bundle_id, app_name,
          window_title, browser_url, simhash,
          length(root_json) AS json_bytes
        FROM tree_snapshots
        WHERE captured_at >= ?1 AND captured_at < ?2
          AND (?3 IS NULL OR app_name = ?3 OR app_bundle_id = ?3)
        ORDER BY captured_at DESC
        LIMIT ?4
        "#,
    )
    .bind(from)
    .bind(to)
    .bind(app)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(row_to_snapshot_meta).collect())
}

/// Full snapshot with `root_json` already parsed into a JSON value so the
/// LLM doesn't have to re-parse a stringified blob.
#[derive(Debug, Clone, Serialize)]
pub struct TreeSnapshotFull {
    #[serde(flatten)]
    pub meta: TreeSnapshotMeta,
    pub root: serde_json::Value,
}

pub async fn get_tree_snapshot(db: &SqlitePool, id: i64) -> Result<Option<TreeSnapshotFull>> {
    let row = sqlx::query(
        r#"
        SELECT
          id, session_id, captured_at, app_bundle_id, app_name,
          window_title, browser_url, simhash,
          length(root_json) AS json_bytes,
          root_json
        FROM tree_snapshots
        WHERE id = ?1
        "#,
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    let Some(row) = row else { return Ok(None) };

    let root_text: String = row.get("root_json");
    let root: serde_json::Value =
        serde_json::from_str(&root_text).unwrap_or(serde_json::Value::String(root_text));
    Ok(Some(TreeSnapshotFull {
        meta: row_to_snapshot_meta(row),
        root,
    }))
}

// ---------- health events ----------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct HealthEventRow {
    pub id: i64,
    pub captured_at: String,
    pub kind: String,
    pub details: Option<serde_json::Value>,
}

pub async fn list_health_events(
    db: &SqlitePool,
    range: Option<ResolvedRange>,
    limit: i64,
) -> Result<Vec<HealthEventRow>> {
    let (from, to) = range.map(|r| r.as_strings()).unzip();
    let rows = sqlx::query(
        r#"
        SELECT id, captured_at, kind, details_json
        FROM health_events
        WHERE (?1 IS NULL OR captured_at >= ?1)
          AND (?2 IS NULL OR captured_at <  ?2)
        ORDER BY captured_at DESC
        LIMIT ?3
        "#,
    )
    .bind(from)
    .bind(to)
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let details: Option<String> = r.try_get("details_json").ok();
            HealthEventRow {
                id: r.get("id"),
                captured_at: r.get("captured_at"),
                kind: r.get("kind"),
                details: details.and_then(|s| serde_json::from_str(&s).ok()),
            }
        })
        .collect())
}

// ---------- meetings & transcripts --------------------------------------

/// Convert an epoch-millis timestamp (how `meetings`/`meeting_transcripts`
/// store time) to an RFC3339 string, matching the ISO strings used elsewhere.
fn ms_to_iso(ms: Option<i64>) -> Option<String> {
    ms.and_then(DateTime::<Utc>::from_timestamp_millis)
        .map(|dt| dt.to_rfc3339())
}

/// Map the stored neutral channel to the human speaker label.
fn speaker_of(channel: &str) -> &str {
    match channel {
        "mic" => "Me",
        "system" => "Others",
        other => other,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MeetingRow {
    pub id: i64,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub app: String,
    pub status: String,
    /// Whether a successful transcript exists (text present).
    pub has_transcript: bool,
    /// Last transcription error, if the attempt failed.
    pub transcript_error: Option<String>,
}

/// Recent meetings (newest first), each flagged with whether it has a transcript.
pub async fn list_meetings(db: &SqlitePool, limit: i64) -> Result<Vec<MeetingRow>> {
    let rows = sqlx::query(
        r#"
        SELECT m.id, m.started_at, m.ended_at, m.app, m.status,
               (t.text IS NOT NULL) AS has_transcript,
               t.error AS transcript_error
        FROM meetings m
        LEFT JOIN meeting_transcripts t ON t.meeting_id = m.id
        ORDER BY m.started_at DESC
        LIMIT ?1
        "#,
    )
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| MeetingRow {
            id: r.get("id"),
            started_at: ms_to_iso(Some(r.get::<i64, _>("started_at"))),
            ended_at: ms_to_iso(r.get::<Option<i64>, _>("ended_at")),
            app: r.get("app"),
            status: r.get("status"),
            has_transcript: r.get::<i64, _>("has_transcript") != 0,
            transcript_error: r.get("transcript_error"),
        })
        .collect())
}

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptSegment {
    pub start: f64,
    pub end: f64,
    /// "Me" (local mic) or "Others" (remote call audio).
    pub speaker: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeetingTranscript {
    pub meeting_id: i64,
    pub app: String,
    pub started_at: Option<String>,
    pub model: Option<String>,
    pub created_at: Option<String>,
    /// Flat transcript text (no speaker labels); `None` until transcribed.
    pub text: Option<String>,
    /// Per-utterance segments with speaker + timings.
    pub segments: Vec<TranscriptSegment>,
    /// Last transcription error, if any.
    pub error: Option<String>,
}

/// One meeting's transcript (header + segments). `None` if the meeting id is
/// unknown; a known meeting with no transcript yet returns empty `segments`.
pub async fn get_meeting_transcript(
    db: &SqlitePool,
    meeting_id: i64,
) -> Result<Option<MeetingTranscript>> {
    let row = sqlx::query(
        r#"
        SELECT m.app, m.started_at,
               t.model, t.created_at, t.text, t.segments_json, t.error
        FROM meetings m
        LEFT JOIN meeting_transcripts t ON t.meeting_id = m.id
        WHERE m.id = ?1
        "#,
    )
    .bind(meeting_id)
    .fetch_optional(db)
    .await?;

    let Some(r) = row else { return Ok(None) };

    let segments = r
        .get::<Option<String>, _>("segments_json")
        .and_then(|s| serde_json::from_str::<Vec<serde_json::Value>>(&s).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|v| TranscriptSegment {
            start: v.get("t0").and_then(|x| x.as_f64()).unwrap_or(0.0),
            end: v.get("t1").and_then(|x| x.as_f64()).unwrap_or(0.0),
            speaker: speaker_of(v.get("channel").and_then(|x| x.as_str()).unwrap_or(""))
                .to_string(),
            text: v
                .get("text")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        })
        .collect();

    Ok(Some(MeetingTranscript {
        meeting_id,
        app: r.get("app"),
        started_at: ms_to_iso(Some(r.get::<i64, _>("started_at"))),
        model: r.get("model"),
        created_at: ms_to_iso(r.get::<Option<i64>, _>("created_at")),
        text: r.get("text"),
        segments,
        error: r.get("error"),
    }))
}
