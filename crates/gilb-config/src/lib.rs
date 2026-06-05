//! Recording configuration shared between the Tauri app and the standalone CLI.
//!
//! v0 is intentionally tiny: paths + a few capture toggles. Per-app exclusion
//! lists land in Phase 4.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_DATA_DIR_NAME: &str = ".gilb";
const DB_FILE_NAME: &str = "db.sqlite";
const LOGS_DIR_NAME: &str = "logs";
const CREDENTIALS_FILE_NAME: &str = "credentials.json";
const PREFERENCES_FILE_NAME: &str = "prefs.json";
const ANALYZER_CONFIG_FILE_NAME: &str = "analyzer_config.json";
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

/// `$HOME/.gilb/` — the per-user data directory for gilb.
pub fn data_dir() -> Result<PathBuf> {
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

/// Server-delivered analyzer configuration, cached at
/// `$HOME/.gilb/analyzer_config.json`. Fetched from gilb-web
/// (`GET /api/v1/analyzer/config`) by the analyzer ("Shannon"); this is the
/// on-disk cache so a run can fall back to it when the network blips. Holds the
/// (private) prompt texts, so it is written `0600` on unix.
///
/// The `version`/`prompts`/`analyze_interval_secs` fields mirror the wire body;
/// `etag` and `fetched_at` are stamped locally to drive conditional refresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzerConfig {
    /// Monotonic server version of the config — logged next to each produced
    /// Therblig so we know which prompt generation made it.
    pub version: i64,
    /// Named prompt texts, e.g. `"therblig-finder"`. A map so new prompts
    /// (e.g. `"skill-builder"`) land without a schema change.
    pub prompts: BTreeMap<String, String>,
    /// Server-controlled analysis cadence in seconds; absent ⇒
    /// [`DEFAULT_ANALYZE_INTERVAL_SECS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analyze_interval_secs: Option<u64>,
    /// ETag of the cached response, sent back as `If-None-Match` to get a cheap
    /// `304 Not Modified` when the config is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// RFC3339 time this cache entry was fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<String>,
}

impl AnalyzerConfig {
    /// Effective analysis cadence: server value, or the hourly default.
    pub fn interval_secs(&self) -> u64 {
        self.analyze_interval_secs
            .unwrap_or(DEFAULT_ANALYZE_INTERVAL_SECS)
    }

    /// Look up a named prompt (e.g. `"therblig-finder"`).
    pub fn prompt(&self, name: &str) -> Option<&str> {
        self.prompts.get(name).map(String::as_str)
    }
}

/// Resolve `$HOME/.gilb/analyzer_config.json`.
pub fn analyzer_config_path() -> Result<PathBuf> {
    Ok(data_dir()?.join(ANALYZER_CONFIG_FILE_NAME))
}

/// Load the analyzer-config cache from `path`. Returns `Ok(None)` when absent,
/// `Err` only on a present-but-malformed file. Path-taking for testability.
pub fn load_analyzer_config_from(path: &Path) -> Result<Option<AnalyzerConfig>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
    };
    let cfg: AnalyzerConfig = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(cfg))
}

/// Write the analyzer-config cache to `path` (creating the parent if needed).
/// Holds private prompt texts, so it is `0600` on unix.
pub fn save_analyzer_config_to(path: &Path, cfg: &AnalyzerConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(cfg).context("failed to serialize analyzer config")?;
    std::fs::write(path, &json).with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod 600 {}", path.display()))?;
    }
    Ok(())
}

/// Load `$HOME/.gilb/analyzer_config.json` (None if absent).
pub fn load_analyzer_config() -> Result<Option<AnalyzerConfig>> {
    load_analyzer_config_from(&analyzer_config_path()?)
}

/// Persist `$HOME/.gilb/analyzer_config.json`.
pub fn save_analyzer_config(cfg: &AnalyzerConfig) -> Result<()> {
    save_analyzer_config_to(&analyzer_config_path()?, cfg)
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

    #[test]
    fn preferences_default_language_is_auto() {
        assert_eq!(Preferences::default().transcription_language, "auto");
    }

    #[test]
    fn analyzer_config_absent_is_none() {
        let dir = std::env::temp_dir().join(format!("gilb-cfg-test-{}", std::process::id()));
        let path = dir.join("analyzer_config.json");
        let _ = std::fs::remove_file(&path);
        assert!(load_analyzer_config_from(&path).unwrap().is_none());
    }

    #[test]
    fn analyzer_config_round_trips() {
        let dir = std::env::temp_dir().join(format!("gilb-cfg-rt-{}", std::process::id()));
        let path = dir.join("analyzer_config.json");
        let mut prompts = BTreeMap::new();
        prompts.insert("therblig-finder".to_string(), "find them".to_string());
        let cfg = AnalyzerConfig {
            version: 7,
            prompts,
            analyze_interval_secs: Some(1800),
            etag: Some("\"abc\"".to_string()),
            fetched_at: Some("2026-06-06T00:00:00Z".to_string()),
        };
        save_analyzer_config_to(&path, &cfg).unwrap();
        let back = load_analyzer_config_from(&path).unwrap().unwrap();
        assert_eq!(cfg, back);
        assert_eq!(back.interval_secs(), 1800);
        assert_eq!(back.prompt("therblig-finder"), Some("find them"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn analyzer_config_interval_defaults() {
        let cfg = AnalyzerConfig {
            version: 1,
            prompts: BTreeMap::new(),
            analyze_interval_secs: None,
            etag: None,
            fetched_at: None,
        };
        assert_eq!(cfg.interval_secs(), DEFAULT_ANALYZE_INTERVAL_SECS);
    }
}
