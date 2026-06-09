//! Recording configuration shared between the Tauri app and the standalone CLI.
//!
//! v0 is intentionally tiny: paths + a few capture toggles. Per-app exclusion
//! lists land in Phase 4.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_DATA_DIR_NAME: &str = ".gilb";
const DB_FILE_NAME: &str = "db.sqlite";
const LOGS_DIR_NAME: &str = "logs";
const CREDENTIALS_FILE_NAME: &str = "credentials.json";
const PREFERENCES_FILE_NAME: &str = "prefs.json";
const MODELS_DIR_NAME: &str = "models";

/// Filename of the local Whisper model (ggml large-v3-turbo, q5_0 quantized),
/// downloaded on demand into [`models_dir`]. Its presence is the gate that
/// enables on-device meeting transcription.
pub const TRANSCRIBE_MODEL_FILE: &str = "ggml-large-v3-turbo-q5_0.bin";

/// Default cadence for the analyzer's incremental upload when the server
/// doesn't specify one. Hourly — see `Credentials::analyze_interval_secs`.
pub const DEFAULT_ANALYZE_INTERVAL_SECS: u64 = 3600;

/// Toggles controlled by `CAPTURE_*` env vars or the future config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingSettings {
    /// Master switch. Default: enabled.
    pub capture_events: bool,
    /// Capture every mouse-move event. Default: disabled (too noisy).
    pub capture_mouse_move: bool,
    /// Capture clipboard via 750ms poll. Default: enabled.
    pub capture_clipboard: bool,
    /// Persist tree snapshots. Default: enabled.
    pub capture_tree_snapshots: bool,
    /// Seconds the pre-record countdown popup fills before auto-arming.
    /// Default: 5.
    pub countdown_seconds: u32,
}

/// Default fill duration for the pre-record countdown popup, in seconds.
pub const DEFAULT_COUNTDOWN_SECONDS: u32 = 5;

impl Default for RecordingSettings {
    fn default() -> Self {
        Self {
            capture_events: true,
            capture_mouse_move: false,
            capture_clipboard: true,
            capture_tree_snapshots: true,
            countdown_seconds: DEFAULT_COUNTDOWN_SECONDS,
        }
    }
}

impl RecordingSettings {
    /// Read settings from environment variables, falling back to defaults.
    pub fn from_env() -> Self {
        let mut s = Self::default();
        if let Some(v) = env_bool("CAPTURE_EVENTS") {
            s.capture_events = v;
        }
        if let Some(v) = env_bool("CAPTURE_MOUSE_MOVE") {
            s.capture_mouse_move = v;
        }
        if let Some(v) = env_bool("CAPTURE_CLIPBOARD") {
            s.capture_clipboard = v;
        }
        if let Some(v) = env_bool("CAPTURE_TREE_SNAPSHOTS") {
            s.capture_tree_snapshots = v;
        }
        if let Some(v) = std::env::var("GILB_COUNTDOWN_SECONDS")
            .ok()
            .and_then(|raw| raw.parse::<u32>().ok())
        {
            s.countdown_seconds = v;
        }
        s
    }
}

fn env_bool(name: &str) -> Option<bool> {
    match std::env::var(name).ok()?.to_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

static DATA_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Override the per-user data directory for this process. Every path helper in
/// this crate derives from [`data_dir`], so a downstream app built on the gilb
/// crates can keep its data under its own directory (e.g. `$HOME/.myapp`) by
/// calling this once at startup, before any other gilb API.
///
/// First write wins: returns `Err` with the rejected path if an override is
/// already in place. When never called, [`data_dir`] resolves to `$HOME/.gilb`.
pub fn set_data_dir(dir: impl Into<PathBuf>) -> std::result::Result<(), PathBuf> {
    DATA_DIR_OVERRIDE.set(dir.into())
}

/// The per-user data directory: the [`set_data_dir`] override when one was
/// installed, otherwise `$HOME/.gilb/`.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = DATA_DIR_OVERRIDE.get() {
        return Ok(dir.clone());
    }
    let home = directories::BaseDirs::new()
        .context("could not determine the user's home directory")?
        .home_dir()
        .to_path_buf();
    Ok(home.join(DEFAULT_DATA_DIR_NAME))
}

/// Resolve `$HOME/.gilb/db.sqlite`. Caller is responsible for creating the
/// parent directory before opening the database.
pub fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join(DB_FILE_NAME))
}

/// Ensure `$HOME/.gilb/` exists; returns its absolute path.
pub fn ensure_data_dir() -> Result<PathBuf> {
    let dir = data_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create gilb data directory at {}", dir.display()))?;
    Ok(dir)
}

/// `$HOME/.gilb/logs/` — directory for rotating log files written by the
/// Tauri app (the CLI smoke binary stays stdout-only).
pub fn logs_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join(LOGS_DIR_NAME))
}

/// Ensure `$HOME/.gilb/logs/` exists; returns its absolute path.
pub fn ensure_logs_dir() -> Result<PathBuf> {
    let dir = logs_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create gilb logs directory at {}", dir.display()))?;
    Ok(dir)
}

/// `$HOME/.gilb/models/` — where downloaded local transcription models live.
pub fn models_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join(MODELS_DIR_NAME))
}

/// Ensure `$HOME/.gilb/models/` exists; returns its absolute path.
pub fn ensure_models_dir() -> Result<PathBuf> {
    let dir = models_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| {
        format!(
            "failed to create gilb models directory at {}",
            dir.display()
        )
    })?;
    Ok(dir)
}

/// Absolute path to the local Whisper model file ([`TRANSCRIBE_MODEL_FILE`]).
/// Its existence is the gate for on-device transcription.
pub fn transcribe_model_path() -> Result<PathBuf> {
    Ok(models_dir()?.join(TRANSCRIBE_MODEL_FILE))
}

/// Enterprise credentials written by the recorder's auth flow and read by the
/// analyzer ("Shannon") to know where to push and how to authenticate. Stored
/// at `$HOME/.gilb/credentials.json`. Its presence is also the gate that
/// activates the analyzer — absent means Tier-1 (local-only, nothing uploaded).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// Base URL of the gilb-web instance to push to.
    pub gilb_web_url: String,
    /// Per-employee bearer token (identifies the employee server-side).
    pub token: String,
    /// Optional human-readable employee label (server-side identity is the
    /// token; this is only for local display/logs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub employee: Option<String>,
    /// Server-controlled cadence for the analyzer's incremental upload, in
    /// seconds. Delivered with credentials so the cadence is tuned from
    /// gilb-web; absent ⇒ [`DEFAULT_ANALYZE_INTERVAL_SECS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analyze_interval_secs: Option<u64>,
}

impl Credentials {
    /// Effective upload cadence: server value, or the hourly default.
    pub fn interval_secs(&self) -> u64 {
        self.analyze_interval_secs
            .unwrap_or(DEFAULT_ANALYZE_INTERVAL_SECS)
    }
}

/// Resolve `$HOME/.gilb/credentials.json`.
pub fn credentials_path() -> Result<PathBuf> {
    Ok(data_dir()?.join(CREDENTIALS_FILE_NAME))
}

/// Load `$HOME/.gilb/credentials.json` if it exists. Returns `Ok(None)` when the
/// file is absent (Tier-1 / not enterprise-configured), `Err` only on a present
/// but unreadable/malformed file.
pub fn load_credentials() -> Result<Option<Credentials>> {
    let path = credentials_path()?;
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let creds: Credentials = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(creds))
}

/// Write `$HOME/.gilb/credentials.json` (creating `$HOME/.gilb/` if needed),
/// flipping the recorder into Tier-2. The file holds a personal bearer token,
/// so it is written `0600` on unix.
pub fn save_credentials(creds: &Credentials) -> Result<()> {
    ensure_data_dir()?;
    let path = credentials_path()?;
    let json = serde_json::to_vec_pretty(creds).context("failed to serialize credentials")?;
    std::fs::write(&path, &json).with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod 600 {}", path.display()))?;
    }
    Ok(())
}

/// Remove `$HOME/.gilb/credentials.json`, returning the recorder to Tier-1
/// (local-only). A no-op (Ok) if the file is already absent — used by sign-out.
pub fn clear_credentials() -> Result<()> {
    let path = credentials_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("failed to remove {}", path.display())),
    }
}

/// When an analyzer job fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// Run on a rolling window every `secs` seconds.
    Interval { secs: u64 },
    /// Run once per finished meeting (event-driven).
    MeetingEnd,
}

/// One analysis job served by gilb-web: a prompt, when to run it, and where to
/// post the findings. The recorder runs `name`'s prompt as `claude -p` over
/// gilb-mcp and forwards what it emits to `post_to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Stable id, e.g. `"therblig-finder"` / `"meeting-facts"`.
    pub name: String,
    /// Emit-only prompt body (the recorder appends the window/params trigger).
    pub prompt: String,
    pub trigger: Trigger,
    /// gilb-web endpoint findings are POSTed to (e.g. `/api/v1/therbligs`).
    pub post_to: String,
}

impl Job {
    /// Cadence for an interval job; hourly default otherwise.
    pub fn interval_secs(&self) -> u64 {
        match self.trigger {
            Trigger::Interval { secs } => secs,
            _ => DEFAULT_ANALYZE_INTERVAL_SECS,
        }
    }
}

/// Server-delivered analyzer configuration fetched from gilb-web
/// (`GET /api/v1/analyzer/config`) by the analyzer ("Shannon").
///
/// **Deliberately not persisted.** Prompts are private, so the config is never
/// written to the filesystem — it lives only in process memory for the lifetime
/// of a run (the daemon keeps the last good copy in-memory across ticks and uses
/// `etag` for a conditional refresh). `version`/`jobs` mirror the wire body;
/// `etag` is the cache validator echoed back as `If-None-Match`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzerConfig {
    /// Monotonic server version — logged next to each produced finding so we
    /// know which config generation made it.
    pub version: i64,
    /// The analysis jobs to run (prompt + trigger + destination each).
    pub jobs: Vec<Job>,
    /// ETag of the last response, sent back as `If-None-Match` to get a cheap
    /// `304 Not Modified` when the config is unchanged.
    pub etag: Option<String>,
}

impl AnalyzerConfig {
    /// Look up a job by name (e.g. `"therblig-finder"`).
    pub fn job(&self, name: &str) -> Option<&Job> {
        self.jobs.iter().find(|j| j.name == name)
    }
}

/// On-disk shape of `$HOME/.gilb/prefs.json` — persisted UI preferences that
/// survive restarts. Not secret, so no `0600`. `#[serde(default)]` lets new
/// fields land later without breaking older files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    /// The user paused always-on activity tracking. When `true` the app does not
    /// auto-resume capture on launch — a deliberate pause survives restarts.
    pub tracking_paused: bool,
    /// Master switch for meeting detection (subsystem B). When `false` the
    /// detector is stopped and no meeting countdowns/recordings happen. Defaults
    /// to `true` — meeting recording is on out of the box.
    pub meeting_detection_enabled: bool,
    /// Language for on-device meeting transcription: `"auto"` | `"ru"` | `"en"`.
    /// Passed to Whisper; `"auto"` detects the language from the first audio.
    pub transcription_language: String,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            tracking_paused: false,
            meeting_detection_enabled: true,
            transcription_language: "auto".to_string(),
        }
    }
}

/// Resolve `$HOME/.gilb/prefs.json`.
pub fn preferences_path() -> Result<PathBuf> {
    Ok(data_dir()?.join(PREFERENCES_FILE_NAME))
}

/// Load preferences from `path`. A missing or malformed file yields defaults —
/// preferences are non-critical and must never block startup. Path-taking for
/// testability.
pub fn load_preferences_from(path: &Path) -> Preferences {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Preferences::default(),
    }
}

/// Write preferences to `path`, creating the parent directory if needed.
pub fn save_preferences_to(path: &Path, prefs: &Preferences) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(prefs).context("failed to serialize preferences")?;
    std::fs::write(path, &json).with_context(|| format!("failed to write {}", path.display()))
}

/// Load `$HOME/.gilb/prefs.json` (defaults if absent/unreadable).
pub fn load_preferences() -> Preferences {
    match preferences_path() {
        Ok(p) => load_preferences_from(&p),
        Err(_) => Preferences::default(),
    }
}

/// Persist preferences to `$HOME/.gilb/prefs.json`.
pub fn save_preferences(prefs: &Preferences) -> Result<()> {
    save_preferences_to(&preferences_path()?, prefs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countdown_seconds_defaults_to_five() {
        assert_eq!(
            RecordingSettings::default().countdown_seconds,
            DEFAULT_COUNTDOWN_SECONDS
        );
        assert_eq!(DEFAULT_COUNTDOWN_SECONDS, 5);
    }

    #[test]
    fn countdown_seconds_env_override_parses() {
        std::env::set_var("GILB_COUNTDOWN_SECONDS", "12");
        let s = RecordingSettings::from_env();
        std::env::remove_var("GILB_COUNTDOWN_SECONDS");
        assert_eq!(s.countdown_seconds, 12);
    }

    // The override is process-global (first write wins), so this is the only
    // test in the crate allowed to touch data_dir-derived paths.
    #[test]
    fn data_dir_override_flows_into_every_path() {
        set_data_dir("/tmp/gilb-config-test-ns").expect("override must be unset at test start");
        assert!(
            set_data_dir("/tmp/other").is_err(),
            "second set must be rejected"
        );

        let base = Path::new("/tmp/gilb-config-test-ns");
        assert_eq!(data_dir().unwrap(), base);
        assert_eq!(db_path().unwrap(), base.join(DB_FILE_NAME));
        assert_eq!(logs_dir().unwrap(), base.join(LOGS_DIR_NAME));
        assert_eq!(models_dir().unwrap(), base.join(MODELS_DIR_NAME));
        assert_eq!(
            credentials_path().unwrap(),
            base.join(CREDENTIALS_FILE_NAME)
        );
        assert_eq!(
            preferences_path().unwrap(),
            base.join(PREFERENCES_FILE_NAME)
        );
    }

    #[test]
    fn preferences_default_language_is_auto() {
        assert_eq!(Preferences::default().transcription_language, "auto");
    }

    #[test]
    fn analyzer_config_job_lookup_and_interval() {
        let cfg = AnalyzerConfig {
            version: 1,
            jobs: vec![
                Job {
                    name: "therblig-finder".to_string(),
                    prompt: "find them".to_string(),
                    trigger: Trigger::Interval { secs: 900 },
                    post_to: "/api/v1/therbligs".to_string(),
                },
                Job {
                    name: "meeting-facts".to_string(),
                    prompt: "extract".to_string(),
                    trigger: Trigger::MeetingEnd,
                    post_to: "/api/v1/meeting_facts".to_string(),
                },
            ],
            etag: None,
        };

        let finder = cfg.job("therblig-finder").unwrap();
        assert_eq!(finder.prompt, "find them");
        assert_eq!(finder.interval_secs(), 900);
        assert_eq!(finder.post_to, "/api/v1/therbligs");

        // MeetingEnd has no interval → hourly default.
        assert_eq!(
            cfg.job("meeting-facts").unwrap().interval_secs(),
            DEFAULT_ANALYZE_INTERVAL_SECS
        );
        assert!(cfg.job("nope").is_none());
    }
}
