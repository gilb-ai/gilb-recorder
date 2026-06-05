//! Fetch + cache the analyzer config (prompts + cadence) from gilb-web.
//!
//! gilb-web is the control plane: after the employee authenticates (bearer
//! token in [`gilb_config::Credentials`]), Shannon pulls the prompt texts and
//! cadence from `GET /api/v1/analyzer/config`. The prompts therefore live on
//! the server (not in this public repo, not in env vars) and can be iterated
//! without shipping a new binary.
//!
//! Refresh is a conditional GET: we send the cached `ETag` as `If-None-Match`
//! and the server answers `304 Not Modified` when nothing changed, so the
//! prompt body is not re-downloaded each tick. The last good response is cached
//! at `~/.gilb/analyzer_config.json`; if a refresh fails (network blip) we fall
//! back to that cache — except on 401/403, which is a hard "re-authenticate".

use anyhow::{bail, Context, Result};
use gilb_config::{AnalyzerConfig, Credentials};
use reqwest::header::{ETAG, IF_NONE_MATCH};
use serde::Deserialize;
use std::collections::BTreeMap;

const CONFIG_PATH: &str = "/api/v1/analyzer/config";

/// Wire body of `GET /api/v1/analyzer/config`. `etag`/`fetched_at` on
/// [`AnalyzerConfig`] are stamped locally, not part of the body — kept as a
/// separate struct so the wire shape can evolve independently of the cache.
#[derive(Debug, Deserialize)]
struct WireConfig {
    version: i64,
    prompts: BTreeMap<String, String>,
    #[serde(default)]
    analyze_interval_secs: Option<u64>,
}

/// Returned when the server rejects the token — surfaced as a hard failure so
/// callers don't silently keep running on a stale cache after a deauth.
#[derive(Debug, thiserror::Error)]
#[error("gilb-web rejected the token (HTTP {0}) fetching analyzer config — re-authenticate")]
struct Unauthorized(u16);

/// Map the wire body + locally-observed metadata into the cached shape.
fn build(wire: WireConfig, etag: Option<String>, now: String) -> AnalyzerConfig {
    AnalyzerConfig {
        version: wire.version,
        prompts: wire.prompts,
        analyze_interval_secs: wire.analyze_interval_secs,
        etag,
        fetched_at: Some(now),
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

async fn fetch(
    client: &reqwest::Client,
    creds: &Credentials,
    etag: Option<&str>,
    now: String,
) -> Result<Fetch> {
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
            Ok(Fetch::Modified(Box::new(build(wire, new_etag, now))))
        }
    }
}

/// Fetch the freshest analyzer config, preferring the cache when the server is
/// unreachable. A `304` returns the cache as-is; a hard auth failure (401/403)
/// propagates so the caller can prompt re-authentication.
pub async fn ensure_config(creds: &Credentials) -> Result<AnalyzerConfig> {
    let cached = gilb_config::load_analyzer_config()?;
    let client = reqwest::Client::new();
    let now = chrono::Utc::now().to_rfc3339();
    let etag = cached.as_ref().and_then(|c| c.etag.clone());

    match fetch(&client, creds, etag.as_deref(), now).await {
        Ok(Fetch::Modified(cfg)) => {
            gilb_config::save_analyzer_config(&cfg).context("failed to cache analyzer config")?;
            Ok(*cfg)
        }
        Ok(Fetch::NotModified) => {
            cached.context("server returned 304 but no cached analyzer config exists")
        }
        Err(e) if e.downcast_ref::<Unauthorized>().is_some() => Err(e),
        Err(e) => match cached {
            Some(c) => {
                tracing::warn!(
                    "config refresh failed ({e:#}); using cached config v{}",
                    c.version
                );
                Ok(c)
            }
            None => Err(e).context("config refresh failed and no cached config to fall back on"),
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
    fn build_maps_wire_and_stamps_local_fields() {
        let mut prompts = BTreeMap::new();
        prompts.insert("therblig-finder".to_string(), "find".to_string());
        let wire = WireConfig {
            version: 7,
            prompts,
            analyze_interval_secs: Some(1800),
        };
        let cfg = build(
            wire,
            Some("\"e1\"".to_string()),
            "2026-06-06T00:00:00Z".to_string(),
        );
        assert_eq!(cfg.version, 7);
        assert_eq!(cfg.prompt("therblig-finder"), Some("find"));
        assert_eq!(cfg.interval_secs(), 1800);
        assert_eq!(cfg.etag.as_deref(), Some("\"e1\""));
        assert_eq!(cfg.fetched_at.as_deref(), Some("2026-06-06T00:00:00Z"));
    }

    #[test]
    fn wire_parses_without_optional_cadence() {
        let wire: WireConfig =
            serde_json::from_str(r#"{"version":3,"prompts":{"therblig-finder":"x"}}"#).unwrap();
        assert_eq!(wire.version, 3);
        assert!(wire.analyze_interval_secs.is_none());
    }
}
