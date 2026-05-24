//! MCP `ServerHandler` implementation. Holds a read-only `SqlitePool` and
//! routes tool calls into `queries::*`.

use std::sync::Arc;

use chrono::Duration;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::queries;
use crate::range::TimeRange;

const HELP_MD: &str = include_str!("../help.md");

#[derive(Clone)]
pub struct GilbService {
    db: Arc<SqlitePool>,
    #[allow(dead_code)]
    tool_router: ToolRouter<GilbService>,
}

// ---------- tool argument structs --------------------------------------

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListSessionsArgs {
    /// Optional time-window filter for `sessions.started_at`.
    #[serde(default)]
    pub range: Option<TimeRange>,
    /// Maximum rows to return (default 20).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GetSessionArgs {
    pub session_id: i64,
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListAppsArgs {
    /// Time-window filter on `actions.captured_at`. Default: last 24h.
    #[serde(default)]
    pub range: Option<TimeRange>,
    /// Maximum apps to return (default 20).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RecentActionsArgs {
    /// Time-window filter. Default: last 10 minutes.
    #[serde(default)]
    pub range: Option<TimeRange>,
    /// Maximum rows to return (default 200, cap 1000).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SearchActionsArgs {
    /// Substring to search for in text_content / element_name / element_value /
    /// window_title. LIKE-based; FTS5 lands in a later phase.
    pub q: String,
    /// Optional app filter (matches `app_name` or `app_bundle_id`).
    #[serde(default)]
    pub app: Option<String>,
    /// Optional action-kind filter
    /// (`click | text | key | scroll | clipboard | focus_change`).
    #[serde(default)]
    pub kind: Option<String>,
    /// Time-window filter. Default: last 24h.
    #[serde(default)]
    pub range: Option<TimeRange>,
    /// Maximum rows (default 100, cap 500).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ActivitySummaryArgs {
    /// Time-window over which to aggregate.
    pub range: TimeRange,
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListTreeSnapshotsArgs {
    /// Time-window filter on `captured_at`. Default: last 24h.
    #[serde(default)]
    pub range: Option<TimeRange>,
    /// Optional app filter (matches `app_name` or `app_bundle_id`).
    #[serde(default)]
    pub app: Option<String>,
    /// Maximum rows (default 50, cap 500). `root_json` is NOT included —
    /// each row carries `json_bytes` so callers can budget before fetching.
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GetTreeSnapshotArgs {
    /// `tree_snapshots.id` from a prior `gilb_list_tree_snapshots` call.
    pub id: i64,
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListHealthEventsArgs {
    /// Optional time-window filter on `captured_at`.
    #[serde(default)]
    pub range: Option<TimeRange>,
    /// Maximum rows (default 100, cap 1000).
    #[serde(default)]
    pub limit: Option<i64>,
}

// ---------- service impl ------------------------------------------------

fn clamp(v: Option<i64>, default: i64, max: i64) -> i64 {
    v.unwrap_or(default).clamp(1, max)
}

fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

fn db_err(err: anyhow::Error) -> McpError {
    McpError::internal_error(format!("db: {err}"), None)
}

#[tool_router]
impl GilbService {
    pub fn new(db: SqlitePool) -> Self {
        Self {
            db: Arc::new(db),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "\
        Markdown reference for gilb's MCP surface: tables, available tools, and example calls. \
        Call this first if you're unsure how to query gilb data.")]
    async fn gilb_help(&self) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![Content::text(HELP_MD)]))
    }

    #[tool(description = "\
        List recording sessions (each Start → Stop is one session). \
        Includes action_count per session. Use `gilb_get_session` for per-kind \
        breakdown and top apps.")]
    async fn gilb_list_sessions(
        &self,
        Parameters(args): Parameters<ListSessionsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = clamp(args.limit, 20, 200);
        let range = args.range.map(|r| r.resolve(Duration::days(30)));
        let rows = queries::list_sessions(&self.db, range, limit)
            .await
            .map_err(db_err)?;
        json_result(&rows)
    }

    #[tool(description = "\
        Details for one session: header + per-kind action counts + top-5 apps \
        by action volume.")]
    async fn gilb_get_session(
        &self,
        Parameters(args): Parameters<GetSessionArgs>,
    ) -> Result<CallToolResult, McpError> {
        match queries::get_session(&self.db, args.session_id)
            .await
            .map_err(db_err)?
        {
            Some(detail) => json_result(&detail),
            None => Err(McpError::invalid_params(
                format!("session {} not found", args.session_id),
                None,
            )),
        }
    }

    #[tool(description = "\
        List apps the user interacted with in the given time window, grouped \
        by bundle_id + app_name, sorted by action count desc.")]
    async fn gilb_list_apps(
        &self,
        Parameters(args): Parameters<ListAppsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = clamp(args.limit, 20, 200);
        let range = args.range.unwrap_or_default().resolve(Duration::hours(24));
        let rows = queries::list_apps(&self.db, range, limit)
            .await
            .map_err(db_err)?;
        json_result(&rows)
    }

    #[tool(description = "\
        Last N actions in chronological order (newest first). Default window: \
        last 10 minutes. Use this for \"what was I just doing\" prompts. \
        password_flag=true rows have text_content and element_value already \
        replaced with '[masked]'.")]
    async fn gilb_recent_actions(
        &self,
        Parameters(args): Parameters<RecentActionsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = clamp(args.limit, 200, 1000);
        let range = args
            .range
            .unwrap_or_default()
            .resolve(Duration::minutes(10));
        let rows = queries::recent_actions(&self.db, range, limit)
            .await
            .map_err(db_err)?;
        json_result(&rows)
    }

    #[tool(description = "\
        LIKE-based substring search over text_content / element_name / \
        element_value / window_title. Password-flagged rows are excluded \
        entirely from search to prevent leakage. Filter by app and/or kind \
        optionally. FTS5 lands in a later phase.")]
    async fn gilb_search_actions(
        &self,
        Parameters(args): Parameters<SearchActionsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = clamp(args.limit, 100, 500);
        let range = args.range.unwrap_or_default().resolve(Duration::hours(24));
        let rows = queries::search_actions(
            &self.db,
            &args.q,
            args.app.as_deref(),
            args.kind.as_deref(),
            range,
            limit,
        )
        .await
        .map_err(db_err)?;
        json_result(&rows)
    }

    #[tool(description = "\
        Aggregated overview for a time range: total action count, per-kind \
        breakdown, top apps by action volume, and the ten longest typed text \
        snippets (password rows excluded).")]
    async fn gilb_activity_summary(
        &self,
        Parameters(args): Parameters<ActivitySummaryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let range = args.range.resolve(Duration::hours(1));
        let summary = queries::activity_summary(&self.db, range)
            .await
            .map_err(db_err)?;
        json_result(&summary)
    }

    #[tool(description = "\
        List a11y tree snapshots in the time window — one row per focused-window \
        change that the snapshotter decided was substantively different from \
        its predecessor. Returns metadata only (id, app, window, browser_url, \
        simhash, json_bytes); use `gilb_get_tree_snapshot` to fetch the full \
        AX tree by id. Snapshots are the structural counterpart to `actions` \
        — correlate by (session_id, captured_at) within a short window.")]
    async fn gilb_list_tree_snapshots(
        &self,
        Parameters(args): Parameters<ListTreeSnapshotsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = clamp(args.limit, 50, 500);
        let range = args.range.unwrap_or_default().resolve(Duration::hours(24));
        let rows = queries::list_tree_snapshots(&self.db, range, args.app.as_deref(), limit)
            .await
            .map_err(db_err)?;
        json_result(&rows)
    }

    #[tool(description = "\
        Fetch one tree snapshot's full AX tree as parsed JSON. The tree is a \
        list of nodes with role/name/value/depth — walk it to understand what \
        was on screen at `captured_at`. Use sparingly: each snapshot can be \
        tens-to-hundreds of KB.")]
    async fn gilb_get_tree_snapshot(
        &self,
        Parameters(args): Parameters<GetTreeSnapshotArgs>,
    ) -> Result<CallToolResult, McpError> {
        match queries::get_tree_snapshot(&self.db, args.id)
            .await
            .map_err(db_err)?
        {
            Some(snap) => json_result(&snap),
            None => Err(McpError::invalid_params(
                format!("tree_snapshot {} not found", args.id),
                None,
            )),
        }
    }

    #[tool(description = "\
        List `health_events` rows for diagnostics: dropped events, sleep/wake, \
        AX query timeouts, capture start/stop. Useful when the user asks \
        \"why is something missing from my log\".")]
    async fn gilb_list_health_events(
        &self,
        Parameters(args): Parameters<ListHealthEventsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = clamp(args.limit, 100, 1000);
        let range = args.range.map(|r| r.resolve(Duration::hours(24)));
        let rows = queries::list_health_events(&self.db, range, limit)
            .await
            .map_err(db_err)?;
        json_result(&rows)
    }
}

#[tool_handler]
impl ServerHandler for GilbService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "gilb-mcp exposes the user's recorded activity (keystrokes, clicks, \
             clipboard, focus changes, etc.) from a local SQLite database. \
             Call `gilb_help` first for a description of the schema and the \
             other available tools."
                    .to_string(),
            )
    }
}
