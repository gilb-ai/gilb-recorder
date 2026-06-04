//! Shannon CLI. Reads recent recorded activity (read-only), reduces + redacts
//! it into a slice, and prints it (`--dry-run`, the only mode today). Uploading
//! the slice to `gilb-web /analyze` lands when that endpoint exists; the
//! credentials contract is `gilb_config::load_credentials()`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use sqlx::Row;

use gilb_analyzer::{redact, ActionRow};

#[derive(Parser, Debug)]
#[command(
    name = "gilb-analyzer",
    about = "Shannon — reduce + redact recorded activity into a de-identified slice"
)]
struct Cli {
    /// DB path (default: ~/.gilb/db.sqlite).
    #[arg(long)]
    db: Option<PathBuf>,
    /// Only actions at/after this ISO8601 time (default: last 1h).
    #[arg(long)]
    since: Option<String>,
    /// Pretty-print the slice JSON.
    #[arg(long)]
    pretty: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let db_path = match cli.db {
        Some(p) => p,
        None => gilb_config::db_path().context("resolve db path")?,
    };
    let db = gilb_db::open_db_read_only(&db_path)
        .await
        .with_context(|| format!("open db at {}", db_path.display()))?;

    let since = cli
        .since
        .unwrap_or_else(|| (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339());

    let rows = load_rows(&db, &since).await?;
    let slice = redact(&rows);

    let json = if cli.pretty {
        serde_json::to_string_pretty(&slice)?
    } else {
        serde_json::to_string(&slice)?
    };
    println!("{json}");
    Ok(())
}

async fn load_rows(db: &sqlx::SqlitePool, since: &str) -> Result<Vec<ActionRow>> {
    let rows = sqlx::query(
        r#"
        SELECT captured_at, kind, app_name, app_bundle_id, window_title,
               browser_url, element_role, element_name, element_value,
               text_content, password_flag, extra_json
        FROM actions
        WHERE captured_at >= ?1
        ORDER BY captured_at ASC
        "#,
    )
    .bind(since)
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
