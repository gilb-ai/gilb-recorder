//! Read-only DB access for Shannon: load action rows for a window and compute
//! the cheap "volume available in the window" counts (form A in the run-record
//! `input` block). All read-only; the live capture DB is never mutated.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::{Row, SqlitePool};

/// One row of the `actions` table, as the analyzer reads it for volume counts.
#[derive(Debug, Clone, Default)]
pub struct ActionRow {
    pub captured_at: String,
    pub kind: String,
    pub app_name: Option<String>,
    pub app_bundle_id: Option<String>,
    pub window_title: Option<String>,
    pub browser_url: Option<String>,
    pub element_role: Option<String>,
    pub element_name: Option<String>,
    pub element_value: Option<String>,
    pub text_content: Option<String>,
    pub password_flag: bool,
    pub extra_json: Option<serde_json::Value>,
}

/// Counts describing how much was available to analyze in the window — the
/// denominator for "tokens spent per finding". Sizes/counts only, never content,
/// so it is safe to report to gilb-web.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SourceCounts {
    pub actions_total: i64,
    pub actions_by_kind: BTreeMap<String, i64>,
    pub apps_distinct: i64,
    pub tree_snapshots: i64,
    pub approx_raw_bytes: i64,
}

/// Load action rows in `[from, to)` ordered by time.
pub async fn load_rows(db: &SqlitePool, from: &str, to: &str) -> Result<Vec<ActionRow>> {
    let rows = sqlx::query(
        r#"
        SELECT captured_at, kind, app_name, app_bundle_id, window_title,
               browser_url, element_role, element_name, element_value,
               text_content, password_flag, extra_json
        FROM actions
        WHERE captured_at >= ?1 AND captured_at < ?2
        ORDER BY captured_at ASC
        "#,
    )
    .bind(from)
    .bind(to)
    .fetch_all(db)
    .await
    .context("query actions")?;

    Ok(rows
        .into_iter()
        .map(|r| ActionRow {
            captured_at: r.get("captured_at"),
            kind: r.get("kind"),
            app_name: r.try_get("app_name").unwrap_or(None),
            app_bundle_id: r.try_get("app_bundle_id").unwrap_or(None),
            window_title: r.try_get("window_title").unwrap_or(None),
            browser_url: r.try_get("browser_url").unwrap_or(None),
            element_role: r.try_get("element_role").unwrap_or(None),
            element_name: r.try_get("element_name").unwrap_or(None),
            element_value: r.try_get("element_value").unwrap_or(None),
            text_content: r.try_get("text_content").unwrap_or(None),
            password_flag: r.try_get::<i64, _>("password_flag").unwrap_or(0) != 0,
            extra_json: r
                .try_get::<Option<String>, _>("extra_json")
                .unwrap_or(None)
                .and_then(|s| serde_json::from_str(&s).ok()),
        })
        .collect())
}

/// Count tree snapshots in `[from, to)`.
pub async fn count_tree_snapshots(db: &SqlitePool, from: &str, to: &str) -> Result<i64> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS n FROM tree_snapshots WHERE captured_at >= ?1 AND captured_at < ?2",
    )
    .bind(from)
    .bind(to)
    .fetch_one(db)
    .await
    .context("count tree_snapshots")?;
    Ok(row.try_get::<i64, _>("n").unwrap_or(0))
}

/// Compute the window's source counts from already-loaded rows + the
/// tree-snapshot count. Pure (no IO) for testing.
pub fn source_counts(rows: &[ActionRow], tree_snapshots: i64) -> SourceCounts {
    let mut by_kind: BTreeMap<String, i64> = BTreeMap::new();
    let mut apps: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut approx_raw_bytes: i64 = 0;

    for r in rows {
        *by_kind.entry(r.kind.clone()).or_insert(0) += 1;
        if let Some(app) = r.app_name.as_deref() {
            apps.insert(app);
        }
        approx_raw_bytes += r.text_content.as_ref().map(|s| s.len()).unwrap_or(0) as i64;
        approx_raw_bytes += r.element_value.as_ref().map(|s| s.len()).unwrap_or(0) as i64;
        approx_raw_bytes += r.window_title.as_ref().map(|s| s.len()).unwrap_or(0) as i64;
    }

    SourceCounts {
        actions_total: rows.len() as i64,
        actions_by_kind: by_kind,
        apps_distinct: apps.len() as i64,
        tree_snapshots,
        approx_raw_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, app: Option<&str>, text: Option<&str>) -> ActionRow {
        ActionRow {
            captured_at: "2026-06-06T00:00:00Z".to_string(),
            kind: kind.to_string(),
            app_name: app.map(str::to_string),
            text_content: text.map(str::to_string),
            ..ActionRow::default()
        }
    }

    #[test]
    fn counts_tally_kinds_apps_and_bytes() {
        let rows = vec![
            row("click", Some("Chrome"), None),
            row("text", Some("Chrome"), Some("hello")),
            row("text", Some("Notion"), Some("worldwide")),
            row("focus_change", None, None),
        ];
        let c = source_counts(&rows, 5);
        assert_eq!(c.actions_total, 4);
        assert_eq!(c.actions_by_kind.get("text"), Some(&2));
        assert_eq!(c.actions_by_kind.get("click"), Some(&1));
        assert_eq!(c.apps_distinct, 2);
        assert_eq!(c.tree_snapshots, 5);
        assert_eq!(
            c.approx_raw_bytes,
            ("hello".len() + "worldwide".len()) as i64
        );
    }
}
