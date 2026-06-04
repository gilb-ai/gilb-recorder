//! Deterministic, auditable PII detection + content-type classification.
//! Self-contained (Shannon re-applies redaction independently of the recorder).

use once_cell::sync::Lazy;
use regex::Regex;

static EMAIL: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}").unwrap());
static SSN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap());
// 13–19 digits, optionally separated by single spaces/dashes (payment cards).
static CARD: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(?:\d[ -]?){13,19}\b").unwrap());
// 32+ char url-safe run — API keys / tokens.
static TOKEN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b[A-Za-z0-9_-]{32,}\b").unwrap());
static UUID: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b").unwrap()
});
static DATE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*(\d{4}-\d{2}-\d{2}|\d{1,2}[/.]\d{1,2}[/.]\d{2,4})").unwrap());
static CURRENCY: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*[$€£¥]\s?\d").unwrap());
static NUMBER: Lazy<Regex> = Lazy::new(|| Regex::new(r"^-?\d{1,12}([.,]\d+)?$").unwrap());

/// Replace any PII match with `[redacted]`. Used on window titles / labels.
pub fn redact_pii(s: &str) -> String {
    let mut out = s.to_string();
    for re in [&*EMAIL, &*SSN, &*CARD, &*TOKEN] {
        out = re.replace_all(&out, "[redacted]").into_owned();
    }
    out
}

/// True if `s` contains anything PII-like (used to drop element names).
pub fn looks_like_pii(s: &str) -> bool {
    EMAIL.is_match(s) || SSN.is_match(s) || CARD.is_match(s) || TOKEN.is_match(s)
}

fn is_card(t: &str) -> bool {
    let digits = t.chars().filter(|c| c.is_ascii_digit()).count();
    let other = t
        .chars()
        .filter(|c| !c.is_ascii_digit() && !c.is_whitespace() && *c != '-')
        .count();
    other == 0 && (13..=19).contains(&digits)
}

/// Coarse, deterministic content-type tag for a typed/clipboard value. Never
/// returns the value itself — only a category. See `docs/redaction-spec.md`.
pub fn classify_value_type(text: &str) -> &'static str {
    let t = text.trim();
    if t.is_empty() {
        return "empty";
    }
    if t == "[masked]" || t == "[redacted]" {
        return "redacted";
    }
    if EMAIL.is_match(t) {
        return "email";
    }
    if SSN.is_match(t) {
        return "ssn";
    }
    if is_card(t) {
        return "card";
    }
    if t.starts_with("http://") || t.starts_with("https://") {
        return "url";
    }
    if UUID.is_match(t) {
        return "uuid";
    }
    if CURRENCY.is_match(t) {
        return "currency";
    }
    if DATE.is_match(t) {
        return "date";
    }
    if NUMBER.is_match(t) {
        return "number";
    }
    if !t.contains(char::is_whitespace) && TOKEN.is_match(t) {
        return "token";
    }
    if t.starts_with('/') || t.starts_with("~/") || t.starts_with("./") {
        return "path";
    }
    "freeform"
}
