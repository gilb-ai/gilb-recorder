//! Time-range parsing. Accepts:
//! - `{"from": "2026-05-22T20:00:00Z", "to": "2026-05-22T21:00:00Z"}` — explicit ISO 8601
//! - `{"last": "10m"}` / `{"last": "2h"}` / `{"last": "today"}` — human shorthand
//!
//! `today` resolves to midnight (local) → now. Other shorthands resolve to
//! now-minus-N → now.

use chrono::{DateTime, Local, NaiveTime, Utc};
use serde::{Deserialize, Serialize};

/// JSON shape exposed to MCP clients.
///
/// Either fully-specified `from`/`to`, or `last` shorthand. If neither is set,
/// the caller is expected to default to "last 1h" or similar at the
/// query-function level.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TimeRange {
    /// ISO 8601 / RFC 3339 lower bound, inclusive. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<DateTime<Utc>>,
    /// ISO 8601 / RFC 3339 upper bound, exclusive. Optional, defaults to "now".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<DateTime<Utc>>,
    /// Shorthand like `"10m"`, `"2h"`, `"1d"`, `"today"`. Wins over `from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct ResolvedRange {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

impl TimeRange {
    /// Resolve to absolute UTC bounds. `default_lookback` is used when neither
    /// `from` nor `last` is provided.
    pub fn resolve(&self, default_lookback: chrono::Duration) -> ResolvedRange {
        let to = self.to.unwrap_or_else(Utc::now);
        let from = if let Some(s) = self.last.as_deref() {
            parse_lookback(s)
                .map(|d| to - d)
                .unwrap_or_else(|| to - default_lookback)
        } else {
            self.from.unwrap_or(to - default_lookback)
        };
        ResolvedRange { from, to }
    }
}

impl ResolvedRange {
    /// Format both endpoints as the same ISO 8601 representation that
    /// `chrono::DateTime<Utc>::to_rfc3339()` uses — `actions.captured_at` is
    /// stored in that form, so lex comparison is correct.
    pub fn as_strings(&self) -> (String, String) {
        (self.from.to_rfc3339(), self.to.to_rfc3339())
    }
}

fn parse_lookback(s: &str) -> Option<chrono::Duration> {
    let s = s.trim().to_ascii_lowercase();

    // Special keywords.
    if s == "today" {
        let now = Local::now();
        let midnight = now.date_naive().and_time(NaiveTime::MIN);
        let from = midnight
            .and_local_timezone(Local)
            .single()?
            .with_timezone(&Utc);
        return Some(Utc::now() - from);
    }
    if s == "yesterday" {
        // From midnight yesterday (local) to now.
        let now = Local::now();
        let yesterday = now.date_naive().pred_opt()?.and_time(NaiveTime::MIN);
        let from = yesterday
            .and_local_timezone(Local)
            .single()?
            .with_timezone(&Utc);
        return Some(Utc::now() - from);
    }

    // `<n><unit>` where unit ∈ {s, m, h, d, w}.
    let (num_part, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = num_part.parse().ok()?;
    let dur = match unit {
        "s" => chrono::Duration::seconds(n),
        "m" => chrono::Duration::minutes(n),
        "h" => chrono::Duration::hours(n),
        "d" => chrono::Duration::days(n),
        "w" => chrono::Duration::weeks(n),
        _ => return None,
    };
    Some(dur)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minutes() {
        let r = TimeRange {
            last: Some("10m".into()),
            ..Default::default()
        }
        .resolve(chrono::Duration::hours(1));
        let span = r.to - r.from;
        assert!((span - chrono::Duration::minutes(10)).num_seconds().abs() <= 1);
    }

    #[test]
    fn parses_hours() {
        let r = TimeRange {
            last: Some("2h".into()),
            ..Default::default()
        }
        .resolve(chrono::Duration::hours(1));
        let span = r.to - r.from;
        assert!((span - chrono::Duration::hours(2)).num_seconds().abs() <= 1);
    }

    #[test]
    fn falls_back_to_default_on_unknown() {
        let r = TimeRange {
            last: Some("garbage".into()),
            ..Default::default()
        }
        .resolve(chrono::Duration::minutes(30));
        let span = r.to - r.from;
        assert!((span - chrono::Duration::minutes(30)).num_seconds().abs() <= 1);
    }

    #[test]
    fn explicit_from_to_preserved() {
        let from = "2026-05-22T20:00:00Z".parse().unwrap();
        let to = "2026-05-22T21:00:00Z".parse().unwrap();
        let r = TimeRange {
            from: Some(from),
            to: Some(to),
            last: None,
        }
        .resolve(chrono::Duration::hours(99));
        assert_eq!(r.from, from);
        assert_eq!(r.to, to);
    }
}
