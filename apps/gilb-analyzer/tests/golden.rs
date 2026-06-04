//! Golden + leak tests for the redactor, driven by the spec fixtures in
//! `docs/redaction-fixtures/` (see `docs/redaction-spec.md`).

use gilb_analyzer::{redact, ActionRow};
use serde_json::Value;

const INPUT: &str = include_str!("../../../docs/redaction-fixtures/basic.input.json");
const SLICE: &str = include_str!("../../../docs/redaction-fixtures/basic.slice.json");
const SECRETS: &str = include_str!("../../../docs/redaction-fixtures/basic.secrets.json");

#[test]
fn basic_fixture_matches_expected_slice() {
    let input: Vec<ActionRow> = serde_json::from_str(INPUT).expect("parse input fixture");
    let slice = redact(&input);
    let got: Value = serde_json::to_value(&slice).expect("serialize slice");
    let expected: Value = serde_json::from_str(SLICE).expect("parse expected slice");
    assert_eq!(got, expected, "redacted slice != expected slice");
}

#[test]
fn basic_fixture_leaks_no_secret() {
    let input: Vec<ActionRow> = serde_json::from_str(INPUT).expect("parse input fixture");
    let slice = redact(&input);
    let serialized = serde_json::to_string(&slice).expect("serialize slice");

    let secrets: Value = serde_json::from_str(SECRETS).expect("parse secrets fixture");
    for s in secrets["secrets"].as_array().expect("secrets array") {
        let needle = s.as_str().expect("secret string");
        assert!(
            !serialized.contains(needle),
            "secret leaked into slice: {needle}"
        );
    }
}
