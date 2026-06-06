//! Therbligs — the abstractions Shannon's Phase-1 (`find`) produces from
//! recorded activity. The LLM (run as `claude -p` over `gilb-mcp`) emits a
//! JSON array of these; this module owns the types and the tolerant parser
//! that turns the model's stdout into `Vec<Therblig>`.
//!
//! Pure: no IO. The parser is deliberately lenient about how the model wraps
//! its JSON (bare array, ```json fence, surrounding prose, `{"therblig": …}`
//! or `{"therbligs": […]}` wrappers) because that shape is not under our
//! control — only the schema of each Therblig is.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One abstracted unit of repeating digital work (see the therblig-finder
/// contract). Pushed to gilb-web as `{"therblig": <this>}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Therblig {
    pub title: String,
    pub intent_summary: String,
    pub time_window_from: String,
    pub time_window_to: String,
    pub steps: Vec<TherbligStep>,
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TherbligStep {
    pub label: String,
    pub delegation: Delegation,
}

/// Delegation tier — the contract Phase 2 (skill-builder) keys off. Wire form
/// is kebab-case: `fully` | `llm` | `human` | `semi-auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Delegation {
    Fully,
    Llm,
    Human,
    SemiAuto,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub captured_at: String,
    pub app: String,
    pub summary: String,
}

/// Parse the model's raw stdout into Therbligs. Tolerant of code fences,
/// surrounding prose, and the common wrapper shapes; strict about each
/// Therblig's schema (an invalid delegation or a missing field is an error,
/// not a silent drop).
pub fn parse_therbligs(raw: &str) -> Result<Vec<Therblig>> {
    let json = extract_json(raw).context("no JSON array/object found in model output")?;
    let value: Value = serde_json::from_str(json).with_context(|| {
        format!(
            "model output is not valid JSON: {}",
            crate::util::snippet(json)
        )
    })?;

    let items = into_items(value)?;
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.into_iter().enumerate() {
        let therblig = unwrap_item(item);
        let parsed: Therblig = serde_json::from_value(therblig)
            .with_context(|| format!("therblig #{i} does not match schema"))?;
        out.push(parsed);
    }
    Ok(out)
}

/// Normalize the top-level JSON into a flat list of candidate therblig values.
/// Accepts a bare array, `{"therbligs": [...]}`, or a single object (treated
/// as a one-element list).
fn into_items(value: Value) -> Result<Vec<Value>> {
    match value {
        Value::Array(items) => Ok(items),
        Value::Object(mut map) => {
            if let Some(Value::Array(items)) = map.remove("therbligs") {
                Ok(items)
            } else {
                // A single object — either a bare therblig or `{"therblig": …}`.
                Ok(vec![Value::Object(map)])
            }
        }
        other => bail!("expected a JSON array or object, got {}", kind_of(&other)),
    }
}

/// Unwrap a `{"therblig": {…}}` element down to the inner therblig; pass other
/// shapes through untouched.
fn unwrap_item(item: Value) -> Value {
    if let Value::Object(map) = &item {
        if map.len() == 1 {
            if let Some(inner) = map.get("therblig") {
                return inner.clone();
            }
        }
    }
    item
}

/// Slice out the JSON payload from arbitrary model text: strip a ```json /
/// ``` fence if present, otherwise take from the first `[`/`{` to its matching
/// last `]`/`}`.
fn extract_json(raw: &str) -> Result<&str> {
    let trimmed = raw.trim();

    // Prefer a fenced block if one is present.
    if let Some(after) = find_fence(trimmed) {
        let body = after
            .find("```")
            .map(|end| after[..end].trim())
            .unwrap_or(after.trim());
        if !body.is_empty() {
            return Ok(body);
        }
    }

    // Otherwise carve from the first opening bracket to the last closing one.
    let start = trimmed
        .find(['[', '{'])
        .ok_or_else(|| anyhow!("no '[' or '{{' in output"))?;
    let end = trimmed
        .rfind([']', '}'])
        .ok_or_else(|| anyhow!("no ']' or '}}' in output"))?;
    if end <= start {
        bail!("malformed JSON bounds in output");
    }
    Ok(trimmed[start..=end].trim())
}

/// Return the text right after an opening ```` ```json ```` / ```` ``` ````
/// fence, if the text contains one.
fn find_fence(s: &str) -> Option<&str> {
    let open = s.find("```")?;
    let after_ticks = &s[open + 3..];
    // Drop an optional language tag on the fence line (e.g. `json`).
    let body_start = after_ticks.find('\n').map(|n| n + 1).unwrap_or(0);
    Some(&after_ticks[body_start..])
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> &'static str {
        r#"[
          {
            "title": "Investor research on Crunchbase",
            "intent_summary": "Looked up recent funding rounds to build a prospect list.",
            "time_window_from": "2026-06-04T10:00:00Z",
            "time_window_to": "2026-06-04T10:12:00Z",
            "steps": [
              {"label": "Open Advanced Search with saved filters", "delegation": "fully"},
              {"label": "Skim results and judge relevance", "delegation": "human"},
              {"label": "Draft a short note per company", "delegation": "semi-auto"}
            ],
            "evidence": [
              {"captured_at": "2026-06-04T10:00:10Z", "app": "Google Chrome", "summary": "Opened Crunchbase advanced search"},
              {"captured_at": "2026-06-04T10:05:00Z", "app": "Google Chrome", "summary": "Filtered by funding stage"},
              {"captured_at": "2026-06-04T10:11:30Z", "app": "Notion", "summary": "Pasted three company names into a list"}
            ]
          }
        ]"#
    }

    #[test]
    fn bare_array_parses() {
        let ts = parse_therbligs(sample()).unwrap();
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].title, "Investor research on Crunchbase");
        assert_eq!(ts[0].steps.len(), 3);
        assert_eq!(ts[0].steps[0].delegation, Delegation::Fully);
        assert_eq!(ts[0].steps[2].delegation, Delegation::SemiAuto);
        assert_eq!(ts[0].evidence.len(), 3);
    }

    #[test]
    fn json_fence_is_stripped() {
        let fenced = format!(
            "Here are the therbligs I found:\n\n```json\n{}\n```\n",
            sample()
        );
        let ts = parse_therbligs(&fenced).unwrap();
        assert_eq!(ts.len(), 1);
    }

    #[test]
    fn bare_fence_without_lang_is_stripped() {
        let fenced = format!("```\n{}\n```", sample());
        let ts = parse_therbligs(&fenced).unwrap();
        assert_eq!(ts.len(), 1);
    }

    #[test]
    fn surrounding_prose_is_ignored() {
        let prose = format!("Sure! {} \n\nLet me know if you want more.", sample());
        let ts = parse_therbligs(&prose).unwrap();
        assert_eq!(ts.len(), 1);
    }

    #[test]
    fn therblig_wrapper_is_unwrapped() {
        let wrapped = r#"[{"therblig": {
            "title": "T", "intent_summary": "s",
            "time_window_from": "2026-06-04T10:00:00Z", "time_window_to": "2026-06-04T10:02:00Z",
            "steps": [{"label": "a", "delegation": "llm"}],
            "evidence": [{"captured_at": "2026-06-04T10:00:00Z", "app": "X", "summary": "y"}]
        }}]"#;
        let ts = parse_therbligs(wrapped).unwrap();
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].steps[0].delegation, Delegation::Llm);
    }

    #[test]
    fn therbligs_object_wrapper_is_unwrapped() {
        let wrapped = format!(r#"{{"therbligs": {}}}"#, sample());
        let ts = parse_therbligs(&wrapped).unwrap();
        assert_eq!(ts.len(), 1);
    }

    #[test]
    fn empty_array_is_ok() {
        let ts = parse_therbligs("[]").unwrap();
        assert!(ts.is_empty());
    }

    #[test]
    fn invalid_delegation_is_error() {
        let bad = r#"[{
            "title": "T", "intent_summary": "s",
            "time_window_from": "a", "time_window_to": "b",
            "steps": [{"label": "a", "delegation": "maybe"}],
            "evidence": [{"captured_at": "c", "app": "X", "summary": "y"}]
        }]"#;
        assert!(parse_therbligs(bad).is_err());
    }

    #[test]
    fn missing_field_is_error() {
        let bad = r#"[{"title": "only a title"}]"#;
        assert!(parse_therbligs(bad).is_err());
    }

    #[test]
    fn no_json_is_error() {
        assert!(parse_therbligs("I could not find any patterns.").is_err());
    }

    #[test]
    fn round_trips_through_serialize() {
        let ts = parse_therbligs(sample()).unwrap();
        let json = serde_json::to_string(&ts).unwrap();
        let again = parse_therbligs(&json).unwrap();
        assert_eq!(ts, again);
    }
}
