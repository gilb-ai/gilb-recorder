//! Parse the model's stdout into a list of findings.
//!
//! Kind-agnostic: a job's prompt emits a JSON array of objects (Therbligs,
//! meeting facts, …); we return them as opaque `serde_json::Value`s and forward
//! each to the job's endpoint, where gilb-web validates the shape per kind. The
//! parser is tolerant of how the model wraps its JSON (bare array, ```json
//! fence, surrounding prose, or a single `{"<name>": [ … ]}` wrapper).

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

/// Parse the model's raw output into a flat list of finding objects.
pub fn parse_findings(raw: &str) -> Result<Vec<Value>> {
    let json = extract_json(raw).context("no JSON array/object found in model output")?;
    let value: Value = serde_json::from_str(json).with_context(|| {
        format!(
            "model output is not valid JSON: {}",
            crate::util::snippet(json)
        )
    })?;
    into_items(value)
}

/// Normalize the top-level JSON into a flat list of finding values:
/// a bare array; a single `{"<name>": [ … ]}` wrapper (e.g. `{"therbligs":[…]}`);
/// or a lone object (treated as a one-element list).
fn into_items(value: Value) -> Result<Vec<Value>> {
    match value {
        Value::Array(items) => Ok(items),
        Value::Object(map) => {
            // A single field whose value is an array → unwrap it (covers
            // `{"therbligs":[…]}` / `{"findings":[…]}` without hardcoding a key).
            if map.len() == 1 {
                if let Some(Value::Array(items)) = map.values().next() {
                    return Ok(items.clone());
                }
            }
            Ok(vec![Value::Object(map)])
        }
        other => bail!("expected a JSON array or object, got {}", kind_of(&other)),
    }
}

/// Slice the JSON payload out of arbitrary model text: strip a ```json / ```
/// fence if present, otherwise take from the first `[`/`{` to the last `]`/`}`.
fn extract_json(raw: &str) -> Result<&str> {
    let trimmed = raw.trim();

    if let Some(after) = find_fence(trimmed) {
        let body = after
            .find("```")
            .map(|end| after[..end].trim())
            .unwrap_or(after.trim());
        if !body.is_empty() {
            return Ok(body);
        }
    }

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

/// Text right after an opening ```` ```json ```` / ```` ``` ```` fence, if any.
fn find_fence(s: &str) -> Option<&str> {
    let open = s.find("```")?;
    let after_ticks = &s[open + 3..];
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

    const ARR: &str = r#"[
      {"title": "A", "k": 1},
      {"title": "B", "k": 2}
    ]"#;

    #[test]
    fn bare_array() {
        let f = parse_findings(ARR).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(f[0]["title"], "A");
    }

    #[test]
    fn json_fence_is_stripped() {
        let fenced = format!("Here you go:\n\n```json\n{ARR}\n```\n");
        assert_eq!(parse_findings(&fenced).unwrap().len(), 2);
    }

    #[test]
    fn bare_fence_is_stripped() {
        let fenced = format!("```\n{ARR}\n```");
        assert_eq!(parse_findings(&fenced).unwrap().len(), 2);
    }

    #[test]
    fn surrounding_prose_is_ignored() {
        let prose = format!("Sure! {ARR}\n\nlet me know.");
        assert_eq!(parse_findings(&prose).unwrap().len(), 2);
    }

    #[test]
    fn single_key_array_wrapper_is_unwrapped() {
        assert_eq!(
            parse_findings(r#"{"therbligs": [{"x":1}]}"#).unwrap().len(),
            1
        );
        assert_eq!(
            parse_findings(r#"{"findings": [{"x":1},{"y":2}]}"#)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn lone_object_becomes_one_element() {
        let f = parse_findings(r#"{"title":"solo"}"#).unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0]["title"], "solo");
    }

    #[test]
    fn empty_array_is_ok() {
        assert!(parse_findings("[]").unwrap().is_empty());
    }

    #[test]
    fn no_json_is_error() {
        assert!(parse_findings("nothing here").is_err());
    }
}
