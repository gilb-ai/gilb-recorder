//! Deterministic reduction + redaction: raw action rows → de-identified slice.
//! Pure (`redact`) so the golden fixtures in `docs/redaction-fixtures/` test it
//! without a database. See `docs/redaction-spec.md` for the rules.

use chrono::DateTime;
use serde::Deserialize;
use serde_json::Value;

use crate::pii;
use crate::slice::{Segment, Slice, Step};

const MAX_NAME_LEN: usize = 64;

/// macOS bundle ids / Windows exe names we never record from. Mirrors
/// `gilb_a11y::password_masking` (kept independent so Shannon's redaction is
/// self-auditable; unify into a shared crate later).
const EXCLUDED_BUNDLE_IDS: &[&str] = &[
    "com.1password.1password",
    "com.1password.1password7",
    "com.bitwarden.desktop",
    "org.keepassxc.keepassxc",
    "com.apple.keychainaccess",
];
const EXCLUDED_EXES: &[&str] = &[
    "1password.exe",
    "bitwarden.exe",
    "keepass.exe",
    "keepassxc.exe",
    "dashlane.exe",
    "logonui.exe",
    "consent.exe",
    "credentialuibroker.exe",
];

fn is_excluded_app(bundle: Option<&str>, name: Option<&str>) -> bool {
    if let Some(b) = bundle {
        if EXCLUDED_BUNDLE_IDS.iter().any(|x| x.eq_ignore_ascii_case(b)) {
            return true;
        }
    }
    if let Some(n) = name {
        if EXCLUDED_EXES.iter().any(|x| x.eq_ignore_ascii_case(n)) {
            return true;
        }
    }
    false
}

/// One raw action row. Field names match the `actions` columns and the JSON
/// fixtures; the CLI maps DB rows into this, tests deserialize fixtures into it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ActionRow {
    pub captured_at: String,
    pub kind: String,
    #[serde(default)]
    pub app_name: Option<String>,
    #[serde(default)]
    pub app_bundle_id: Option<String>,
    #[serde(default)]
    pub window_title: Option<String>,
    #[serde(default)]
    pub browser_url: Option<String>,
    #[serde(default)]
    pub element_role: Option<String>,
    #[serde(default)]
    pub element_name: Option<String>,
    #[serde(default)]
    pub element_value: Option<String>,
    #[serde(default)]
    pub text_content: Option<String>,
    #[serde(default)]
    pub password_flag: bool,
    #[serde(default)]
    pub extra_json: Option<Value>,
}

struct SegBuilder {
    app: String,
    window_title: Option<String>,
    url_host: Option<String>,
    url_path_shape: Option<String>,
    started_at: String,
    last_at: String,
    excluded: bool,
    steps: Vec<Step>,
}

impl SegBuilder {
    fn new(row: &ActionRow) -> Self {
        let (url_host, url_path_shape) = parse_url(row.browser_url.as_deref());
        SegBuilder {
            app: row
                .app_name
                .clone()
                .or_else(|| row.app_bundle_id.clone())
                .unwrap_or_else(|| "unknown".to_string()),
            window_title: row.window_title.as_deref().map(redact_title),
            url_host,
            url_path_shape,
            started_at: row.captured_at.clone(),
            last_at: row.captured_at.clone(),
            excluded: is_excluded_app(row.app_bundle_id.as_deref(), row.app_name.as_deref()),
            steps: Vec::new(),
        }
    }

    fn finish(self) -> Segment {
        Segment {
            app: self.app,
            window_title: self.window_title,
            url_host: self.url_host,
            url_path_shape: self.url_path_shape,
            started_at: self.started_at.clone(),
            duration_s: duration_secs(&self.started_at, &self.last_at),
            steps: self.steps,
        }
    }
}

/// Reduce + redact a time-ordered list of action rows into a slice.
pub fn redact(rows: &[ActionRow]) -> Slice {
    let mut segments = Vec::new();
    let mut cur: Option<SegBuilder> = None;

    for row in rows {
        if row.kind == "focus_change" {
            if let Some(b) = cur.take() {
                if !b.excluded {
                    segments.push(b.finish());
                }
            }
            cur = Some(SegBuilder::new(row));
            continue; // focus_change marks the segment; not a step
        }
        if cur.is_none() {
            cur = Some(SegBuilder::new(row));
        }
        let b = cur.as_mut().unwrap();
        b.last_at = row.captured_at.clone();
        if b.excluded {
            continue;
        }
        if let Some(step) = row_to_step(row) {
            b.steps.push(step);
        }
    }
    if let Some(b) = cur.take() {
        if !b.excluded {
            segments.push(b.finish());
        }
    }
    Slice { segments }
}

fn row_to_step(row: &ActionRow) -> Option<Step> {
    match row.kind.as_str() {
        "click" => Some(Step::Click {
            role: role_of(row),
            name: guarded_name(row.element_name.as_deref(), row.text_content.as_deref()),
        }),
        "text" => {
            if row.password_flag {
                return Some(Step::Text {
                    role: role_of(row),
                    name: None,
                    len: None,
                    value_type: None,
                    secure: true,
                });
            }
            let text = row.text_content.as_deref().unwrap_or("");
            Some(Step::Text {
                role: role_of(row),
                name: guarded_name(row.element_name.as_deref(), Some(text)),
                len: Some(text.chars().count()),
                value_type: Some(pii::classify_value_type(text).to_string()),
                secure: false,
            })
        }
        "key" => extra_str(row, "key").map(|key| Step::Key { key }),
        "scroll" => {
            let dy = extra_i64(row, "delta_y").unwrap_or(0);
            Some(Step::Scroll {
                dir: if dy < 0 { "down" } else { "up" }.to_string(),
            })
        }
        "clipboard" => {
            let text = row.text_content.as_deref().unwrap_or("");
            Some(Step::Clipboard {
                len: if text.is_empty() {
                    None
                } else {
                    Some(text.chars().count())
                },
                value_type: if text.is_empty() {
                    None
                } else {
                    Some(pii::classify_value_type(text).to_string())
                },
            })
        }
        _ => None,
    }
}

fn role_of(row: &ActionRow) -> String {
    row.element_role
        .clone()
        .unwrap_or_else(|| "unknown".to_string())
}

/// Keep an a11y label, or drop it (N1–N3 from the spec).
fn guarded_name(name: Option<&str>, text: Option<&str>) -> Option<String> {
    let name = name?.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_LEN {
        return None; // N2
    }
    if pii::looks_like_pii(name) {
        return None; // N3
    }
    let digits = name.chars().filter(|c| c.is_ascii_digit()).count();
    if digits > 0 && digits * 2 > name.chars().count() {
        return None; // N3 — mostly digits
    }
    if let Some(t) = text {
        let t = t.trim();
        if !t.is_empty() && (name == t || t.contains(name) || name.contains(t)) {
            return None; // N1 — name is (part of) the typed value
        }
    }
    Some(name.to_string())
}

fn redact_title(title: &str) -> String {
    let redacted = pii::redact_pii(title);
    if redacted.chars().count() > 200 {
        redacted.chars().take(200).collect()
    } else {
        redacted
    }
}

/// Host + generalized path shape; query/fragment/userinfo/port dropped.
fn parse_url(url: Option<&str>) -> (Option<String>, Option<String>) {
    let url = match url {
        Some(u) if !u.is_empty() => u,
        _ => return (None, None),
    };
    let after = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let (host, rest) = match after.find('/') {
        Some(i) => (&after[..i], &after[i..]),
        None => (after, ""),
    };
    let host = host.rsplit('@').next().unwrap_or(host); // drop userinfo
    let host = host.split(':').next().unwrap_or(host); // drop port
    let path = rest.split(['?', '#']).next().unwrap_or(rest);
    let path_shape = if path.is_empty() || path == "/" {
        if path == "/" {
            Some("/".to_string())
        } else {
            None
        }
    } else {
        Some(generalize_path(path))
    };
    (Some(host.to_string()), path_shape)
}

fn generalize_path(path: &str) -> String {
    let parts: Vec<String> = path
        .split('/')
        .map(|seg| {
            if seg.is_empty() {
                String::new()
            } else if is_dynamic_segment(seg) {
                ":id".to_string()
            } else {
                seg.to_string()
            }
        })
        .collect();
    parts.join("/")
}

fn is_dynamic_segment(seg: &str) -> bool {
    let all_digits = seg.chars().all(|c| c.is_ascii_digit());
    let long_hex = seg.len() >= 24 && seg.chars().all(|c| c.is_ascii_hexdigit());
    let uuidish = seg.len() == 36 && seg.matches('-').count() == 4;
    all_digits || long_hex || uuidish
}

fn duration_secs(start: &str, end: &str) -> i64 {
    match (
        DateTime::parse_from_rfc3339(start),
        DateTime::parse_from_rfc3339(end),
    ) {
        (Ok(s), Ok(e)) => (e - s).num_seconds().max(0),
        _ => 0,
    }
}

fn extra_str(row: &ActionRow, key: &str) -> Option<String> {
    row.extra_json
        .as_ref()?
        .get(key)?
        .as_str()
        .map(|s| s.to_string())
}

fn extra_i64(row: &ActionRow, key: &str) -> Option<i64> {
    row.extra_json.as_ref()?.get(key)?.as_i64()
}
