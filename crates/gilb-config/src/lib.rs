//! Recording configuration shared between the Tauri app and the standalone CLI.
//!
//! Intentionally tiny: paths, a few capture toggles, and the persisted user
//! preferences. Anything that needs to be true before the app has a window
//! belongs here; anything the user edits through the UI belongs in
//! `Preferences` below.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Folder name inside the user's Documents. Visible on purpose: everything
/// gilb produces — the database, meeting recordings, transcripts, the prompt
/// the assistant runs on — is the user's own data, and data you cannot find in
/// Finder or Explorer is data you cannot delete, back up or inspect.
const DATA_DIR_NAME: &str = "Gilb";
/// Where installs before the move kept everything. Migrated on first run.
const LEGACY_DATA_DIR_NAME: &str = ".gilb";
const DB_FILE_NAME: &str = "db.sqlite";
const LOGS_DIR_NAME: &str = "logs";
const CREDENTIALS_FILE_NAME: &str = "credentials.json";
const PREFERENCES_FILE_NAME: &str = "prefs.json";
const MODELS_DIR_NAME: &str = "models";
const PROMPTS_DIR_NAME: &str = "prompts";

/// The real-time assistant's prompt, in [`prompts_dir`]. Seeded from the
/// bundled copy on first run and then the user's to edit.
pub const ASSIST_PROMPT_FILE: &str = "realtime_assist.md";

/// Filename of the local Whisper model (ggml large-v3-turbo, q5_0 quantized),
/// downloaded on demand into [`models_dir`]. Its presence is the gate that
/// enables on-device meeting transcription.
pub const TRANSCRIBE_MODEL_FILE: &str = "ggml-large-v3-turbo-q5_0.bin";

/// Where [`TRANSCRIBE_MODEL_FILE`] is fetched from. Next to the file name on
/// purpose: post-meeting transcription and realtime suggestions download the
/// same ~570 MB build, and two copies of this string drift the moment one of
/// them is bumped.
pub const TRANSCRIBE_MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin";

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
/// already in place. When never called, [`data_dir`] resolves to
/// `<Documents>/Gilb`.
pub fn set_data_dir(dir: impl Into<PathBuf>) -> std::result::Result<(), PathBuf> {
    DATA_DIR_OVERRIDE.set(dir.into())
}

/// The per-user data directory: the [`set_data_dir`] override when one was
/// installed, otherwise `<Documents>/Gilb/` — `~/Documents/Gilb` on macOS,
/// `%USERPROFILE%\Documents\Gilb` on Windows.
///
/// Documents is resolved through the OS rather than assembled from `$HOME`,
/// because on Windows it is a known folder the user (or OneDrive) may have
/// moved, and writing to a literal `%USERPROFILE%\Documents` that nothing
/// points at any more would hide the data as effectively as the old dotfile
/// did. When the OS has no answer, fall back to `$HOME/Documents/Gilb` and
/// finally to `$HOME/Gilb` — never silently to a hidden directory.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = DATA_DIR_OVERRIDE.get() {
        return Ok(dir.clone());
    }
    Ok(documents_dir()?.join(DATA_DIR_NAME))
}

fn documents_dir() -> Result<PathBuf> {
    if let Some(docs) =
        directories::UserDirs::new().and_then(|d| d.document_dir().map(Path::to_path_buf))
    {
        return Ok(docs);
    }
    let home = home_dir()?;
    let guess = home.join("Documents");
    Ok(if guess.is_dir() { guess } else { home })
}

fn home_dir() -> Result<PathBuf> {
    Ok(directories::BaseDirs::new()
        .context("could not determine the user's home directory")?
        .home_dir()
        .to_path_buf())
}

/// Bring an older install's directory to where (and what) [`data_dir`] says,
/// once, at startup — before anything opens the database.
///
/// Two shapes of "older": the hidden `$HOME/.gilb` every install had before the
/// move into Documents, and a `Documents/Gilb` from the short window when the
/// folder was lowercase. The second is a rename that only changes case, which
/// macOS and Windows will do happily on their case-insensitive filesystems and
/// which is invisible to `Path::exists` — so it is detected by reading the
/// parent directory, not by asking whether the target is there.
///
/// A rename in both cases, so the ~570 MB model and the meeting recordings are
/// never copied. If it fails — the two paths on different volumes, a permission
/// problem — the old directory is left exactly as it was and the caller is
/// told, rather than the app half-migrating and losing history.
///
/// Returns where the data came from, or `None` when there was nothing to do
/// (fresh install, already migrated, or a [`set_data_dir`] override in force —
/// a product with its own directory is not ours to move).
pub fn migrate_legacy_data_dir() -> Result<Option<PathBuf>> {
    if DATA_DIR_OVERRIDE.get().is_some() {
        return Ok(None);
    }
    let target = data_dir()?;

    if let Some(previous) = differently_cased_sibling(&target)? {
        rename_into_place(&previous, &target)?;
        return Ok(Some(previous));
    }

    let legacy = home_dir()?.join(LEGACY_DATA_DIR_NAME);
    if !legacy.is_dir() || target.exists() {
        // Nothing to move, or both are present — in which case the new one
        // wins (it is what the app has been writing to) and the old one stays
        // for the user to look through and delete.
        return Ok(None);
    }
    rename_into_place(&legacy, &target)?;
    Ok(Some(legacy))
}

/// A directory next to `target` whose name matches it apart from case. `None`
/// when the name on disk is already exactly right — including when nothing is
/// there at all.
fn differently_cased_sibling(target: &Path) -> Result<Option<PathBuf>> {
    let (Some(parent), Some(name)) = (target.parent(), target.file_name().and_then(|n| n.to_str()))
    else {
        return Ok(None);
    };
    if !parent.is_dir() {
        return Ok(None);
    }
    for entry in std::fs::read_dir(parent)
        .with_context(|| format!("failed to read {}", parent.display()))?
        .flatten()
    {
        let Some(found) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if found != name && found.eq_ignore_ascii_case(name) && entry.path().is_dir() {
            return Ok(Some(parent.join(found)));
        }
    }
    Ok(None)
}

fn rename_into_place(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::rename(from, to).with_context(|| {
        format!(
            "failed to move {} to {} — the old directory is untouched; move it by hand",
            from.display(),
            to.display()
        )
    })
}

/// The database file inside [`data_dir`]. Caller is responsible for creating
/// the parent directory before opening it.
pub fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join(DB_FILE_NAME))
}

/// Ensure [`data_dir`] exists; returns its absolute path.
pub fn ensure_data_dir() -> Result<PathBuf> {
    let dir = data_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create gilb data directory at {}", dir.display()))?;
    Ok(dir)
}

/// `<data_dir>/logs/` — rotating log files written by the Tauri app (the CLI
/// smoke binary stays stdout-only).
pub fn logs_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join(LOGS_DIR_NAME))
}

/// Ensure [`logs_dir`] exists; returns its absolute path.
pub fn ensure_logs_dir() -> Result<PathBuf> {
    let dir = logs_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create gilb logs directory at {}", dir.display()))?;
    Ok(dir)
}

/// `<data_dir>/models/` — where downloaded local transcription models live.
pub fn models_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join(MODELS_DIR_NAME))
}

/// Ensure [`models_dir`] exists; returns its absolute path.
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

/// `<data_dir>/prompts/` — the prompts the user is meant to edit. A directory
/// rather than a single file so a second prompt does not have to invent a new
/// home for itself.
pub fn prompts_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join(PROMPTS_DIR_NAME))
}

/// Ensure [`prompts_dir`] exists; returns its absolute path.
pub fn ensure_prompts_dir() -> Result<PathBuf> {
    let dir = prompts_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| {
        format!(
            "failed to create gilb prompts directory at {}",
            dir.display()
        )
    })?;
    Ok(dir)
}

/// `<data_dir>/prompts/realtime_assist.md` — the real-time assistant's prompt.
pub fn assist_prompt_path() -> Result<PathBuf> {
    Ok(prompts_dir()?.join(ASSIST_PROMPT_FILE))
}

/// Absolute path to the local Whisper model file ([`TRANSCRIBE_MODEL_FILE`]).
/// Its existence is the gate for on-device transcription.
pub fn transcribe_model_path() -> Result<PathBuf> {
    Ok(models_dir()?.join(TRANSCRIBE_MODEL_FILE))
}

/// Enterprise credentials written by the recorder's auth flow and read by the
/// analyzer ("Shannon") to know where to push and how to authenticate. Stored
/// at `<data_dir>/credentials.json`. Its presence is also the gate that
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

/// Resolve `<data_dir>/credentials.json`.
pub fn credentials_path() -> Result<PathBuf> {
    Ok(data_dir()?.join(CREDENTIALS_FILE_NAME))
}

/// Load `<data_dir>/credentials.json` if it exists. Returns `Ok(None)` when the
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

/// Write `<data_dir>/credentials.json` (creating the data directory if needed),
/// flipping the recorder into Tier-2. The file holds a personal bearer token,
/// so it is written `0600` on unix.
pub fn save_credentials(creds: &Credentials) -> Result<()> {
    ensure_data_dir()?;
    let path = credentials_path()?;
    let json = serde_json::to_vec_pretty(creds).context("failed to serialize credentials")?;
    write_secret_file(&path, &json)
}

/// Atomically write `bytes` to `path` as an owner-only (`0600`) file: create a
/// fresh sibling temp file, fsync it, then `rename` it over `path`. `rename` is
/// atomic on a single filesystem, so a reader never observes a partial file and
/// a crash mid-write can never truncate the destination — the previous file
/// survives intact. For on-disk secrets (credentials, auth tokens). The parent
/// directory must already exist.
pub fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    // Unique temp sibling: the same dir keeps the rename atomic (no cross-fs
    // copy); pid + counter keep concurrent writers from clobbering each other.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let stem = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("secret");
    let tmp = dir.join(format!(
        ".{stem}.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&tmp)
        .with_context(|| format!("failed to create temp {}", tmp.display()))?;

    if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("failed to write temp {}", tmp.display()));
    }
    drop(file);

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e)
            .with_context(|| format!("failed to rename {} -> {}", tmp.display(), path.display()));
    }
    Ok(())
}

/// Remove `<data_dir>/credentials.json`, returning the recorder to Tier-1
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

/// On-disk shape of `<data_dir>/prefs.json` — persisted UI preferences that
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
    /// User-level switch for the real-time meeting assistant (suggestions
    /// overlay). Independent of the server-side feature flag: `false` tears
    /// the local pipeline down entirely, so no STT runs during meetings.
    pub assist_enabled: bool,
    /// Which agent the assistant runs on, as the product's own id (gilb:
    /// `"claude"` / `"codex"` / `"cursor"`). `None` means the user has not
    /// chosen yet — with two coding CLIs installed, picking one for them is
    /// picking whose model sees the meeting.
    #[serde(default)]
    pub assist_agent: Option<String>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            tracking_paused: false,
            meeting_detection_enabled: true,
            transcription_language: "auto".to_string(),
            assist_enabled: true,
            assist_agent: None,
        }
    }
}

/// Resolve `<data_dir>/prefs.json`.
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

/// Load `<data_dir>/prefs.json` (defaults if absent/unreadable).
pub fn load_preferences() -> Preferences {
    match preferences_path() {
        Ok(p) => load_preferences_from(&p),
        Err(_) => Preferences::default(),
    }
}

/// Persist preferences to `<data_dir>/prefs.json`.
pub fn save_preferences(prefs: &Preferences) -> Result<()> {
    save_preferences_to(&preferences_path()?, prefs)
}

/// Serializes writes to `prefs.json` so concurrent single-field updates can't
/// lose each other's change.
static PREFERENCES_LOCK: Mutex<()> = Mutex::new(());

/// Read-modify-write the preferences file under a process-global lock. Without
/// this, two commands that each load → mutate one field → save (e.g. the
/// tracking-pause toggle and the meeting-detection switch) can interleave and
/// the second save clobbers the first field's change. `update` mutates a fresh
/// load and the write happens while the lock is held, so updates compose.
/// Returns the persisted preferences.
pub fn update_preferences(update: impl FnOnce(&mut Preferences)) -> Result<Preferences> {
    let _guard = PREFERENCES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut prefs = load_preferences();
    update(&mut prefs);
    save_preferences(&prefs)?;
    Ok(prefs)
}

// ---------------------------------------------------------------------------
// Locating an external agent CLI
// ---------------------------------------------------------------------------

/// Bin dirs a coding agent (and the `node` it needs) commonly installs into, in
/// probe order.
///
/// A bundled `.app` starts with a minimal PATH — no shell profile has run — so
/// a bare `claude` is not findable there. Both the analyzer (`claude -p`) and
/// the assist backend (ACP) hit this, which is why the probe lives here rather
/// than in whichever crate needed it first.
pub fn agent_bin_dirs() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut dirs = vec![
        format!("{home}/.local/bin"),
        format!("{home}/.claude/local"),
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
        format!("{home}/.npm-global/bin"),
        // Version managers put global npm binaries under the *active* runtime,
        // which a GUI app never sees: they work by rewriting PATH in a login
        // shell, and a bundle launched from Finder gets none of that. Omitting
        // them means a user with a perfectly good agent installed is told they
        // have none.
        format!("{home}/.volta/bin"),
        format!("{home}/.bun/bin"),
    ];
    dirs.extend(node_version_dirs(&home));
    dirs
}

/// `bin/` of every installed nvm/fnm node version, newest first.
///
/// Newest first because a binary installed globally under an old runtime is
/// usually a leftover, and running it under that old node is how you get an
/// error from inside the agent rather than from us.
fn node_version_dirs(home: &str) -> Vec<String> {
    let roots = [
        format!("{home}/.nvm/versions/node"),
        format!("{home}/.local/share/fnm/node-versions"),
    ];
    let mut found = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut versions: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        // Version-aware order: v10 after v9, not before it.
        versions.sort_by_key(|name| std::cmp::Reverse(version_key(name)));
        for name in versions {
            // nvm: <root>/<version>/bin. fnm: <root>/<version>/installation/bin.
            found.push(format!("{root}/{name}/bin"));
            found.push(format!("{root}/{name}/installation/bin"));
        }
    }
    found
}

fn version_key(name: &str) -> Vec<u32> {
    name.trim_start_matches('v')
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

/// Locate an agent CLI by name. `env_override` (when set and non-empty) wins
/// over everything; otherwise the known install dirs are probed. Falls back to
/// the bare name so PATH still gets its chance.
pub fn resolve_agent_bin(name: &str, env_override: &str) -> String {
    if let Ok(path) = std::env::var(env_override) {
        if !path.trim().is_empty() {
            return path;
        }
    }
    for dir in agent_bin_dirs() {
        let candidate = format!("{dir}/{name}");
        if std::path::Path::new(&candidate).is_file() {
            return candidate;
        }
    }
    name.to_string()
}

/// PATH for a spawned agent: the known bin dirs prepended to the inherited
/// PATH, so an npm-installed CLI can find its `node` even from a bundle.
pub fn agent_path_env() -> String {
    let mut parts = agent_bin_dirs();
    if let Ok(current) = std::env::var("PATH") {
        if !current.is_empty() {
            parts.push(current);
        }
    }
    parts.join(":")
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
        assert_eq!(prompts_dir().unwrap(), base.join(PROMPTS_DIR_NAME));
        assert_eq!(
            assist_prompt_path().unwrap(),
            base.join(PROMPTS_DIR_NAME).join(ASSIST_PROMPT_FILE)
        );

        // A product that brought its own directory must never have its data
        // moved by gilb's migration.
        assert_eq!(migrate_legacy_data_dir().unwrap(), None);
    }

    /// The point of the move: a user can find this in Finder / Explorer.
    /// `set_data_dir` is process-global and claimed by the test above, so this
    /// checks the pieces the default is assembled from instead.
    #[test]
    fn default_data_dir_is_visible() {
        assert!(
            !DATA_DIR_NAME.starts_with('.'),
            "the data directory must not be hidden"
        );
        let docs = documents_dir().expect("a home directory exists in any test environment");
        assert!(
            docs.is_absolute(),
            "documents dir must be absolute, got {}",
            docs.display()
        );
    }

    #[test]
    fn preferences_default_language_is_auto() {
        assert_eq!(Preferences::default().transcription_language, "auto");
    }

    #[test]
    fn write_secret_file_is_atomic_and_owner_only() {
        let dir = std::env::temp_dir().join(format!("gilb-secret-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret.json");

        write_secret_file(&path, b"first").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");

        // Overwriting replaces the contents wholesale (no leftover bytes from a
        // longer previous write) and never leaves a partial file.
        write_secret_file(&path, b"second-and-much-longer").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second-and-much-longer");
        write_secret_file(&path, b"x").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"x");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "secret file must be owner-only");
        }

        // No temp siblings left behind after a successful write.
        let leftover = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!leftover, "temp files must be cleaned up");

        std::fs::remove_dir_all(&dir).ok();
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
