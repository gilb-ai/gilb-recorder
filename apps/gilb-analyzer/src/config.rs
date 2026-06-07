//! Fetch the analyzer config (the list of analysis jobs) from gilb-web.
//!
//! gilb-web is the control plane: after the employee authenticates (bearer
//! token in [`gilb_config::Credentials`]), Shannon pulls the jobs — each a
//! prompt + trigger + destination — from `GET /api/v1/analyzer/config`. The
//! prompts live on the server (not in this public repo, not in env vars) and
//! can be iterated without shipping a new binary.
//!
//! **The prompt is never written to disk.** The config — including the private
//! prompt text — is held only in process memory. The daemon keeps the last good
//! copy in-memory across ticks and passes it back in as `cached`; refresh is a
//! conditional GET (cached `ETag` → `If-None-Match`) so an unchanged config
//! answers `304` and the body isn't re-downloaded. A one-shot `find` has no
//! prior cache and so always fetches fresh.

use anyhow::{bail, Context, Result};
use gilb_config::{AnalyzerConfig, Credentials, Job, Trigger};
use reqwest::header::{ETAG, IF_NONE_MATCH};
use serde::Deserialize;

const CONFIG_PATH: &str = "/api/v1/analyzer/config";

/// Wire body of `GET /api/v1/analyzer/config`. `etag` on [`AnalyzerConfig`] is
/// observed from the response header, not the body — kept as separate structs so
/// the wire shape can evolve independently of the in-memory types.
#[derive(Debug, Deserialize)]
struct WireConfig {
    version: i64,
    jobs: Vec<WireJob>,
}

#[derive(Debug, Deserialize)]
struct WireJob {
    name: String,
    prompt: String,
    trigger: WireTrigger,
    post_to: String,
}

/// `{"on":"interval","secs":3600}` | `{"on":"meeting_end"}`.
#[derive(Debug, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
enum WireTrigger {
    Interval { secs: u64 },
    MeetingEnd,
}

impl From<WireTrigger> for Trigger {
    fn from(w: WireTrigger) -> Self {
        match w {
            WireTrigger::Interval { secs } => Trigger::Interval { secs },
            WireTrigger::MeetingEnd => Trigger::MeetingEnd,
        }
    }
}

impl From<WireJob> for Job {
    fn from(w: WireJob) -> Self {
        Job {
            name: w.name,
            prompt: w.prompt,
            trigger: w.trigger.into(),
            post_to: w.post_to,
        }
    }
}

/// Returned when the server rejects the token — surfaced as a hard failure so
/// callers don't silently keep running on a stale in-memory copy after a deauth.
#[derive(Debug, thiserror::Error)]
#[error("gilb-web rejected the token (HTTP {0}) fetching analyzer config — re-authenticate")]
struct Unauthorized(u16);

/// Map the wire body + the observed ETag into the in-memory config.
fn build(wire: WireConfig, etag: Option<String>) -> AnalyzerConfig {
    AnalyzerConfig {
        version: wire.version,
        jobs: wire.jobs.into_iter().map(Job::from).collect(),
        etag,
    }
}

/// How to treat an HTTP status from the config endpoint.
#[derive(Debug, PartialEq, Eq)]
enum StatusClass {
    NotModified,
    Ok,
    Unauthorized,
    Other,
}

fn classify(status: u16) -> StatusClass {
    match status {
        304 => StatusClass::NotModified,
        200..=299 => StatusClass::Ok,
        401 | 403 => StatusClass::Unauthorized,
        _ => StatusClass::Other,
    }
}

enum Fetch {
    NotModified,
    Modified(Box<AnalyzerConfig>),
}

async fn fetch(client: &reqwest::Client, creds: &Credentials, etag: Option<&str>) -> Result<Fetch> {
    let url = format!(
        "{}{}",
        creds.gilb_web_url.trim_end_matches('/'),
        CONFIG_PATH
    );
    let mut req = client.get(&url).bearer_auth(&creds.token);
    if let Some(tag) = etag {
        req = req.header(IF_NONE_MATCH, tag);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("GET {url} failed"))?;
    let status = resp.status();
    match classify(status.as_u16()) {
        StatusClass::NotModified => Ok(Fetch::NotModified),
        StatusClass::Unauthorized => Err(Unauthorized(status.as_u16()).into()),
        StatusClass::Other => bail!("GET {url} returned HTTP {status}"),
        StatusClass::Ok => {
            let new_etag = resp
                .headers()
                .get(ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let wire: WireConfig = resp.json().await.context("config body is not valid JSON")?;
            Ok(Fetch::Modified(Box::new(build(wire, new_etag))))
        }
    }
}

/// Fetch the freshest analyzer config, reusing the in-memory `cached` copy when
/// the server says `304` or is unreachable. A hard auth failure (401/403)
/// propagates so the caller can prompt re-authentication. Nothing is persisted.
pub async fn ensure_config(
    creds: &Credentials,
    cached: Option<&AnalyzerConfig>,
) -> Result<AnalyzerConfig> {
    let client = reqwest::Client::new();
    let etag = cached.and_then(|c| c.etag.clone());

    match fetch(&client, creds, etag.as_deref()).await {
        Ok(Fetch::Modified(cfg)) => Ok(*cfg),
        Ok(Fetch::NotModified) => cached
            .cloned()
            .context("server returned 304 but no in-memory config to reuse"),
        Err(e) if e.downcast_ref::<Unauthorized>().is_some() => Err(e),
        Err(e) => match cached {
            Some(c) => {
                tracing::warn!(
                    "config refresh failed ({e:#}); reusing in-memory config v{}",
                    c.version
                );
                Ok(c.clone())
            }
            None => Err(e).context("config refresh failed and no in-memory config to reuse"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_statuses() {
        assert_eq!(classify(304), StatusClass::NotModified);
        assert_eq!(classify(200), StatusClass::Ok);
        assert_eq!(classify(201), StatusClass::Ok);
        assert_eq!(classify(401), StatusClass::Unauthorized);
        assert_eq!(classify(403), StatusClass::Unauthorized);
        assert_eq!(classify(500), StatusClass::Other);
        assert_eq!(classify(404), StatusClass::Other);
    }

    #[test]
    fn build_maps_wire_jobs_and_observed_etag() {
        let body = r#"{
            "version": 7,
            "jobs": [
              {"name":"therblig-finder","prompt":"find","trigger":{"on":"interval","secs":1800},"post_to":"/api/v1/therbligs"},
              {"name":"meeting-facts","prompt":"extract","trigger":{"on":"meeting_end"},"post_to":"/api/v1/meeting_facts"}
            ]
        }"#;
        let wire: WireConfig = serde_json::from_str(body).unwrap();
        let cfg = build(wire, Some("\"e1\"".to_string()));

        assert_eq!(cfg.version, 7);
        assert_eq!(cfg.etag.as_deref(), Some("\"e1\""));
        let finder = cfg.job("therblig-finder").unwrap();
        assert_eq!(finder.prompt, "find");
        assert_eq!(finder.interval_secs(), 1800);
        assert_eq!(finder.post_to, "/api/v1/therbligs");
        assert_eq!(
            cfg.job("meeting-facts").unwrap().trigger,
            Trigger::MeetingEnd
        );
    }

    #[test]
    fn wire_parses_meeting_end_trigger() {
        let wire: WireConfig = serde_json::from_str(
            r#"{"version":3,"jobs":[{"name":"m","prompt":"p","trigger":{"on":"meeting_end"},"post_to":"/x"}]}"#,
        )
        .unwrap();
        assert_eq!(wire.version, 3);
        assert!(matches!(wire.jobs[0].trigger, WireTrigger::MeetingEnd));
    }
}
