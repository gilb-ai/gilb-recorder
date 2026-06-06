//! gilb-web client — the one outbound path to the vendor.
//!
//! Only derived abstractions cross this boundary: Phase 1 fetches the titles
//! already pushed in the window (dedup) and POSTs each new Therblig. Status
//! handling follows the therblig-finder contract: `201` is success, `429`
//! aborts the whole run, any other non-2xx is logged and the run *continues*
//! (Phase 1 semantics — distinct from Phase 2, which stops on any failure).
//!
//! HTTP methods need a server; the pure helpers (status mapping, dedup key,
//! id/ref parsing) carry the logic and are unit-tested.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

use crate::run::RunRecord;
use crate::therblig::Therblig;

/// Minimal view of an already-pushed Therblig, for dedup.
#[derive(Debug, Clone, Deserialize)]
pub struct TherbligRef {
    #[serde(default)]
    pub id: Option<i64>,
    pub title: String,
}

/// Outcome of a single Therblig POST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostOutcome {
    /// 2xx — created; server-assigned id if we could read it.
    Created { id: Option<i64> },
    /// 429 — caller must stop the whole run.
    RateLimited,
    /// Other non-2xx — caller logs and continues to the next Therblig.
    Failed { status: u16, body: String },
}

/// How to treat a POST status under Phase 1 semantics.
#[derive(Debug, PartialEq, Eq)]
enum PostStatus {
    Created,
    RateLimited,
    Failed,
}

fn post_status(code: u16) -> PostStatus {
    match code {
        200..=299 => PostStatus::Created,
        429 => PostStatus::RateLimited,
        _ => PostStatus::Failed,
    }
}

/// Normalize a title into a dedup key: trimmed, case-insensitive
/// (per therblig-finder's dedup contract).
pub fn dedup_key(title: &str) -> String {
    title.trim().to_lowercase()
}

/// Pull a server-assigned id out of a create response, accepting either a
/// top-level `id` or a nested `{"therblig": {"id": …}}`.
fn parse_created_id(body: &str) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("id").and_then(serde_json::Value::as_i64).or_else(|| {
        v.get("therblig")
            .and_then(|t| t.get("id"))
            .and_then(serde_json::Value::as_i64)
    })
}

/// Parse the dedup-list response, accepting a bare array or a
/// `{"therbligs": [...]}` wrapper.
fn parse_refs(body: &str) -> Result<Vec<TherbligRef>> {
    let v: serde_json::Value =
        serde_json::from_str(body).context("therbligs list is not valid JSON")?;
    let arr = match v {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Object(mut m) => match m.remove("therbligs") {
            Some(serde_json::Value::Array(a)) => a,
            _ => return Ok(Vec::new()),
        },
        _ => return Ok(Vec::new()),
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        // Skip entries without a title rather than failing the whole list.
        if let Ok(r) = serde_json::from_value::<TherbligRef>(item) {
            out.push(r);
        }
    }
    Ok(out)
}

/// Authenticated client for one gilb-web instance.
pub struct Web {
    client: reqwest::Client,
    base_url: String,
    token: String,
}

// Manual Debug so the bearer token never lands in logs.
impl std::fmt::Debug for Web {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Web")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl Web {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// GET the Therbligs already pushed in `[from, to]` (for dedup by title).
    pub async fn list_therbligs(&self, from: &str, to: &str) -> Result<Vec<TherbligRef>> {
        let url = self.url("/api/v1/therbligs");
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .query(&[("from", from), ("to", to)])
            .send()
            .await
            .with_context(|| format!("GET {url} failed"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("GET {url} returned HTTP {status}");
        }
        parse_refs(&body)
    }

    /// POST one Therblig (`{"therblig": …}`). One object per request.
    pub async fn post_therblig(&self, therblig: &Therblig) -> Result<PostOutcome> {
        let url = self.url("/api/v1/therbligs");
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&json!({ "therblig": therblig }))
            .send()
            .await
            .with_context(|| format!("POST {url} failed"))?;
        let code = resp.status().as_u16();
        match post_status(code) {
            PostStatus::RateLimited => Ok(PostOutcome::RateLimited),
            PostStatus::Created => {
                let body = resp.text().await.unwrap_or_default();
                Ok(PostOutcome::Created {
                    id: parse_created_id(&body),
                })
            }
            PostStatus::Failed => {
                let body = resp.text().await.unwrap_or_default();
                Ok(PostOutcome::Failed { status: code, body })
            }
        }
    }

    /// POST one run-accounting record (`{"run": …}`). Lower-stakes than Therblig
    /// push — the caller logs on failure rather than aborting.
    pub async fn post_run(&self, run: &RunRecord) -> Result<()> {
        let url = self.url("/api/v1/analyzer/runs");
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&json!({ "run": run }))
            .send()
            .await
            .with_context(|| format!("POST {url} failed"))?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("POST {url} returned HTTP {status}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping_follows_phase1_rules() {
        assert_eq!(post_status(201), PostStatus::Created);
        assert_eq!(post_status(200), PostStatus::Created);
        assert_eq!(post_status(429), PostStatus::RateLimited);
        assert_eq!(post_status(500), PostStatus::Failed);
        assert_eq!(post_status(400), PostStatus::Failed);
        assert_eq!(post_status(403), PostStatus::Failed);
    }

    #[test]
    fn dedup_key_is_trimmed_and_lowercased() {
        assert_eq!(dedup_key("  Investor Research  "), "investor research");
        assert_eq!(dedup_key("investor research"), "investor research");
    }

    #[test]
    fn created_id_top_level() {
        assert_eq!(parse_created_id(r#"{"id": 42}"#), Some(42));
    }

    #[test]
    fn created_id_nested() {
        assert_eq!(parse_created_id(r#"{"therblig": {"id": 7}}"#), Some(7));
    }

    #[test]
    fn created_id_absent() {
        assert_eq!(parse_created_id(r#"{"ok": true}"#), None);
        assert_eq!(parse_created_id("not json"), None);
    }

    #[test]
    fn refs_from_bare_array() {
        let refs = parse_refs(r#"[{"id":1,"title":"A"},{"title":"B"}]"#).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].id, Some(1));
        assert_eq!(refs[1].title, "B");
    }

    #[test]
    fn refs_from_wrapper() {
        let refs = parse_refs(r#"{"therbligs":[{"id":1,"title":"A"}]}"#).unwrap();
        assert_eq!(refs.len(), 1);
    }

    #[test]
    fn refs_skip_untitled_entries() {
        let refs = parse_refs(r#"[{"id":1},{"id":2,"title":"ok"}]"#).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].title, "ok");
    }
}
