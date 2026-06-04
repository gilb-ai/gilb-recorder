//! Meeting-app allowlist.
//!
//! Maps bundle IDs of known meeting/telephony apps to a display name.
//! Ported verbatim from the owner's rodnik app
//! (`meetingDetection/allowlist.js`, `MEETING_APP_ALLOWLIST`). Shared
//! between the macOS unified-log detector here and the Windows detector
//! (GILB-30), so changes belong in this one place.
//!
//! Browsers are intentionally omitted — they produce too many false
//! positives from voice search and similar in-page audio.

/// `(bundle_id, display_name)` pairs for known meeting apps.
const ALLOWLIST: &[(&str, &str)] = &[
    // Video conferencing
    ("us.zoom.xos", "Zoom"),
    ("com.microsoft.teams", "Microsoft Teams"),
    ("com.microsoft.teams2", "Microsoft Teams"),
    ("com.tinyspeck.slackmacgap", "Slack"),
    ("com.apple.FaceTime", "FaceTime"),
    ("com.cisco.webexmeetingsapp", "Webex"),
    ("com.webex.meetingmanager", "Webex"),
    ("net.whatsapp.WhatsApp", "WhatsApp"),
    ("com.skype.skype", "Skype"),
    ("com.hnc.Discord", "Discord"),
    ("com.tencent.tencentmeeting", "VooV Meeting"),
    ("app.tuple.app", "Tuple"),
    ("com.gather.Gather", "Gather"),
    // Russian video conferencing
    ("ru.yandex.desktop.telemost", "Яндекс Телемост"),
    ("kontur.talk", "Контур.Толк"),
    ("salutejazz.jazz-app", "SaluteJazz"),
    // Telephony
    ("io.aircall.phone", "Aircall"),
    ("com.electron.dialpad", "Dialpad"),
    ("com.electron.uberconference", "Dialpad Meetings"),
    // AI assistants
    ("ai.perplexity.comet", "Perplexity"),
];

/// True if `bundle_id` belongs to a known meeting app.
pub fn is_allowlisted(bundle_id: &str) -> bool {
    ALLOWLIST.iter().any(|(id, _)| *id == bundle_id)
}

/// Display name for `bundle_id`, or `None` if it is not allowlisted.
pub fn display_name(bundle_id: &str) -> Option<&'static str> {
    ALLOWLIST
        .iter()
        .find(|(id, _)| *id == bundle_id)
        .map(|(_, name)| *name)
}
