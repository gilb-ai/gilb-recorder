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

/// `(process_exe, bundle_id)` pairs mapping a Windows process to the
/// bundle ID of the same meeting app. Ported verbatim from rodnik's
/// `WINDOWS_PROCESS_MAP`; the keys are lowercase exe basenames (the
/// match is case-insensitive — see [`bundle_id_from_process_name`]).
///
/// WASAPI reports a session's process, not a bundle ID, so the Windows
/// detector maps through here before reusing the shared allowlist.
const WINDOWS_PROCESS_MAP: &[(&str, &str)] = &[
    // Video conferencing
    ("zoom.exe", "us.zoom.xos"),
    ("teams.exe", "com.microsoft.teams2"),
    ("ms-teams.exe", "com.microsoft.teams2"),
    ("webex.exe", "com.cisco.webexmeetingsapp"),
    ("voovmeetingapp.exe", "com.tencent.tencentmeeting"),
    ("tuple.exe", "app.tuple.app"),
    ("gather.exe", "com.gather.Gather"),
    // Russian video conferencing
    ("yandextelemost.exe", "ru.yandex.desktop.telemost"),
    ("jazz.exe", "salutejazz.jazz-app"),
    ("ktalk.exe", "kontur.talk"),
    // Messaging
    ("slack.exe", "com.tinyspeck.slackmacgap"),
    ("discord.exe", "com.hnc.Discord"),
    ("whatsapp.exe", "net.whatsapp.WhatsApp"),
    ("skype.exe", "com.skype.skype"),
    // Telephony
    ("aircall.exe", "io.aircall.phone"),
    ("aircall workspace.exe", "io.aircall.phone"),
    ("dialpad.exe", "com.electron.dialpad"),
    ("dialpadmeetings.exe", "com.electron.uberconference"),
    // AI assistants
    ("comet.exe", "ai.perplexity.comet"),
];

/// Bundle ID for a Windows process, or `None` if it is not a known
/// meeting app. `process_name` may be a full path or a bare exe name;
/// only the basename is matched, case-insensitively (port of rodnik
/// `getBundleIdFromProcessName`).
pub fn bundle_id_from_process_name(process_name: &str) -> Option<&'static str> {
    let basename = process_name
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(process_name)
        .to_ascii_lowercase();
    WINDOWS_PROCESS_MAP
        .iter()
        .find(|(exe, _)| *exe == basename)
        .map(|(_, id)| *id)
}

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
