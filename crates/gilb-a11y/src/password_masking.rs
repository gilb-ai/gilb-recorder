//! Heuristics for redacting secrets before they reach the DB.
//!
//! Phase 0 ships the static lookup tables and unit tests; AX-based
//! `AXSecureTextField` detection is hooked in Phase 1.

use once_cell::sync::Lazy;
use regex::Regex;

/// macOS bundle IDs we never record from. Hardcoded to fail safe even if the
/// user-level config is misconfigured.
pub const EXCLUDED_BUNDLE_IDS: &[&str] = &[
    "com.1password.1password",
    "com.1password.1password7",
    "com.1password.1password.helper",
    "com.bitwarden.desktop",
    "org.keepassxc.keepassxc",
    "com.apple.keychainaccess",
];

/// Lowercase substrings that mark an element / field name as password-like.
pub const PASSWORD_NAME_SUBSTRINGS: &[&str] = &[
    "password", "passwd", "passphrase", "secret", "pin code", "pincode",
    "cvv", "cvc", "security code", "card number", "credit card", "token",
    "api key", "private key",
];

/// AX role identifying a secure (masked) text field.
pub const AX_SECURE_TEXT_FIELD: &str = "AXSecureTextField";

/// Returns `true` when the active app should not be recorded.
pub fn is_excluded_app(bundle_id: &str) -> bool {
    EXCLUDED_BUNDLE_IDS
        .iter()
        .any(|b| b.eq_ignore_ascii_case(bundle_id))
}

/// Returns `true` when the AX role indicates a secure text field.
pub fn is_secure_role(role: &str) -> bool {
    role.eq_ignore_ascii_case(AX_SECURE_TEXT_FIELD)
}

/// Returns `true` when an element name or identifier looks like a password
/// field by string match.
pub fn is_password_field(name: Option<&str>, identifier: Option<&str>) -> bool {
    [name, identifier].into_iter().flatten().any(|s| {
        let lower = s.to_lowercase();
        PASSWORD_NAME_SUBSTRINGS
            .iter()
            .any(|needle| lower.contains(needle))
    })
}

static PII_REGEXES: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        // 13–19 digit credit-card like sequences (Luhn check left to consumers).
        Regex::new(r"\b\d{13,19}\b").unwrap(),
        // US SSN.
        Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap(),
        // Long-looking secret token (40+ url-safe chars).
        Regex::new(r"\b[A-Za-z0-9_-]{40,}\b").unwrap(),
    ]
});

/// Replace anything matching the PII regexes with `[redacted]`.
pub fn redact_pii(text: &str) -> String {
    let mut out = text.to_string();
    for re in PII_REGEXES.iter() {
        out = re.replace_all(&out, "[redacted]").into_owned();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excluded_apps_are_blocked_case_insensitively() {
        assert!(is_excluded_app("com.1password.1password"));
        assert!(is_excluded_app("COM.BITWARDEN.DESKTOP"));
        assert!(!is_excluded_app("com.apple.Notes"));
    }

    #[test]
    fn password_substrings_are_detected() {
        assert!(is_password_field(Some("Password"), None));
        assert!(is_password_field(None, Some("user-passphrase-input")));
        assert!(is_password_field(Some("Security code"), None));
        assert!(!is_password_field(Some("Username"), Some("user-input")));
    }

    #[test]
    fn secure_role_check() {
        assert!(is_secure_role("AXSecureTextField"));
        assert!(is_secure_role("axsecuretextfield"));
        assert!(!is_secure_role("AXTextField"));
    }

    #[test]
    fn pii_regex_redacts_card_and_ssn() {
        let s = "card 4111111111111111 and ssn 111-22-3333";
        let r = redact_pii(s);
        assert!(r.contains("[redacted]"));
        assert!(!r.contains("4111111111111111"));
        assert!(!r.contains("111-22-3333"));
    }
}
