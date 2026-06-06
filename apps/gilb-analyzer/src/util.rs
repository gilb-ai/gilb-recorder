//! Small shared helpers.

/// Truncate `s` to at most ~160 chars on a char boundary, for error messages
/// that quote a chunk of model output without dumping the whole thing.
pub(crate) fn snippet(s: &str) -> String {
    const MAX: usize = 160;
    let s = s.trim();
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_string_is_unchanged() {
        assert_eq!(snippet("  hi  "), "hi");
    }

    #[test]
    fn long_string_is_truncated_with_ellipsis() {
        let s = "x".repeat(500);
        let out = snippet(&s);
        assert!(out.ends_with('…'));
        assert!(out.len() <= 161 + 3); // 160 bytes + multibyte ellipsis
    }

    #[test]
    fn truncation_respects_char_boundary() {
        // 200 multibyte chars; must not panic slicing mid-codepoint.
        let s = "é".repeat(200);
        let _ = snippet(&s);
    }
}
