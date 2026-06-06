//! gilb-web client — the one outbound path to the vendor.
//!
//! Only derived findings cross this boundary. The recorder is kind-agnostic: it
//! POSTs each emitted finding as `{ "run_id", "item" }` to the job's `post_to`
//! endpoint; gilb-web validates + dedups + stores per kind. Status handling:
//! `2xx` = created, `409` = duplicate (server already had it), `429` = stop the
//! run, any other non-2xx = log + continue to the next finding.
//!
//! HTTP needs a server; `post_status` carries the mapping and is unit-tested.

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::run::RunRecord;

/// Outcome of a single finding POST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostOutcome {
    /// 2xx — server accepted a new finding.
    Created,
    /// 409 — server already had it (its dedup).
    Duplicate,
    /// 429 — caller must stop the whole run.
    RateLimited,
    /// Other non-2xx — caller logs and continues to the next finding.
    Failed { status: u16, body: String },
}

fn post_status(code: u16) -> PostOutcome {
    match code {
        200..=299 => PostOutcome::Created,
        409 => PostOutcome::Duplicate,
        429 => PostOutcome::RateLimited,
        _ => PostOutcome::Failed {
            status: code,
            body: String::new(),
        },
    }
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

    /// POST one finding to `post_to` as `{ "run_id", "item" }`. One object per
    /// request; the server validates/dedups/stores it per its kind, and `run_id`
    /// links it to the run that produced it (for cost).
    pub async fn post_finding(
        &self,
        post_to: &str,
        item: &Value,
        run_id: &str,
    ) -> Result<PostOutcome> {
        let url = self.url(post_to);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&json!({ "run_id": run_id, "item": item }))
            .send()
            .await
            .with_context(|| format!("POST {url} failed"))?;
        let code = resp.status().as_u16();
        match post_status(code) {
            PostOutcome::Failed { status, .. } => {
                let body = resp.text().await.unwrap_or_default();
                Ok(PostOutcome::Failed { status, body })
            }
            other => Ok(other),
        }
    }

    /// POST one run-accounting record (`{"run": …}`). Lower-stakes than a finding
    /// — the caller logs on failure rather than aborting.
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
    fn status_mapping() {
        assert_eq!(post_status(201), PostOutcome::Created);
        assert_eq!(post_status(200), PostOutcome::Created);
        assert_eq!(post_status(409), PostOutcome::Duplicate);
        assert_eq!(post_status(429), PostOutcome::RateLimited);
        assert!(matches!(
            post_status(500),
            PostOutcome::Failed { status: 500, .. }
        ));
        assert!(matches!(
            post_status(400),
            PostOutcome::Failed { status: 400, .. }
        ));
    }
}
