//! Recording configuration shared between the Tauri app and the standalone CLI.
//!
//! v0 is intentionally tiny: paths + a few capture toggles. Per-app exclusion
//! lists land in Phase 4.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_DATA_DIR_NAME: &str = ".gilb";
const DB_FILE_NAME: &str = "db.sqlite";
const LOGS_DIR_NAME: &str = "logs";
const CREDENTIALS_FILE_NAME: &str = "credentials.json";

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
}

impl Default for RecordingSettings {
    fn default() -> Self {
        Self {
            capture_events: true,
            capture_mouse_move: false,
            capture_clipboard: true,
            capture_tree_snapshots: true,
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
