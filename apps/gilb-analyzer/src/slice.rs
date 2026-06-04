//! The de-identified slice — Shannon's only output (see
//! `docs/redaction-spec.md`). Serialize-only; never carries raw content.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Slice {
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Segment {
    pub app: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url_path_shape: Option<String>,
    pub started_at: String,
    pub duration_s: i64,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Step {
    Click {
        role: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Text {
        role: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        len: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        value_type: Option<String>,
        /// Password/secure field — content was masked at capture; we emit only
        /// the fact, no name/len/value_type.
        #[serde(skip_serializing_if = "is_false")]
        secure: bool,
    },
    Key {
        key: String,
    },
    Scroll {
        dir: String,
    },
    Nav {
        url_host: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        url_path_shape: Option<String>,
    },
    Clipboard {
        #[serde(skip_serializing_if = "Option::is_none")]
        len: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        value_type: Option<String>,
    },
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}
