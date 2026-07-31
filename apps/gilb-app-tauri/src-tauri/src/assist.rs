//! gilb's half of the real-time assist stack: suggestions during a live
//! meeting, produced by a **local agent** over ACP.
//!
//! The overlay window, the switch, the whisper-model gate and the audio
//! pipeline all come from `gilb_shell_tauri::assist`; what lives here is the
//! three things that are gilb's own:
//!
//! * **availability** — the agent binary exists. Nothing to run without it, and
//!   the UI says so instead of failing per suggestion;
//! * **the prompt** — `prompts/realtime_assist.md` in the visible data folder.
//!   Deliberately local and never uploaded: a prompter's prompt holds prices
//!   and objection handling, which is the user's business, and the file is
//!   theirs to edit;
//! * **the backend** — an ACP session against that agent.
//!
//! The trust boundary a server-backed product needs (fencing recorded speech,
//! keeping the operator's question outside the fence) is *not* enforced here:
//! a local agent is a single-user tool talking to its owner, with no tools and
//! nobody else's data to reach. When this role grows tool access, revisit —
//! `AssistSession::ask` already carries the two halves separately.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use gilb_assist::{AssistBackend, AssistConfig};
use gilb_assist_acp::{agent_available, AcpBackend, AcpConfig};
use gilb_shell_tauri::assist::{AssistHost, AssistStrings};
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Manager};
use tracing::{info, warn};

/// Override the agent binary; also how a test or a power user points gilb at
/// `gemini`, a wrapper script, or a Hermes ACP adapter.
const AGENT_BIN_ENV: &str = "GILB_ASSIST_AGENT";
/// Extra arguments, space-separated (e.g. `--experimental-acp`).
const AGENT_ARGS_ENV: &str = "GILB_ASSIST_AGENT_ARGS";

/// The bundled copy of the prompt, inside the app's resources — a file the
/// packager ships, not a string compiled into the binary. Seeded into the
/// user's prompts directory on first run; after that the user's copy wins and
/// this one is never read again.
const BUNDLED_PROMPT: &str = "resources/prompts/realtime_assist.md";

/// A suggestion is worthless once the conversation has moved on. Generous
/// enough for a local model's first token, short enough to stay a suggestion.
const TURN_TIMEOUT: Duration = Duration::from_secs(15);

pub fn init(app: &AppHandle) {
    // Resolve the bundled prompt once, at init: `AssistHost::engine` is called
    // from the pipeline with no `AppHandle` in reach, and resource paths differ
    // between a dev run and an installed bundle.
    let bundled = app
        .path()
        .resolve(BUNDLED_PROMPT, BaseDirectory::Resource)
        .inspect_err(|err| warn!(error = %err, "bundled assist prompt not found"))
        .ok();
    gilb_shell_tauri::assist::init(app, GilbAssistHost { bundled });
}

// `gilb_shell_tauri::assist::refresh` re-evaluates availability. Nothing calls
// it here: the agent binary appears while the app is not looking, so there is
// no event to hang it on. A product whose availability follows a sign-in calls
// it there.

struct GilbAssistHost {
    /// Absolute path to the shipped prompt, or `None` if the resource is
    /// missing from this build — in which case the feature refuses to start
    /// rather than inventing a prompt of its own.
    bundled: Option<PathBuf>,
}

impl AssistHost for GilbAssistHost {
    /// No agent, no feature. Same shape as the whisper-model gate: the UI shows
    /// what is missing instead of failing once per suggestion.
    fn available(&self) -> bool {
        agent_available(&agent_bin())
    }

    fn engine(&self) -> Result<(Box<dyn AssistConfig>, Box<dyn AssistBackend>)> {
        let config = FileAssistConfig::load(self.bundled.as_deref())?;
        let acp = AcpConfig {
            bin: agent_bin(),
            args: agent_args(),
            // The agent gets a scratch directory, not the user's project: a
            // meeting prompter has no business reading the working tree.
            cwd: std::env::temp_dir(),
            turn_timeout: TURN_TIMEOUT,
            ..AcpConfig::default()
        };
        Ok((Box::new(config), Box::new(AcpBackend::new(acp))))
    }

    fn strings(&self) -> AssistStrings {
        AssistStrings {
            window_title: "gilb".into(),
            app_name: "gilb".into(),
            model_downloaded: "Speech model ready — suggestions start at the next meeting.".into(),
            model_failed: "Could not download the speech model. Try again from the app.".into(),
            unavailable: "install the agent CLI first".into(),
        }
    }
}

/// Same resolution the analyzer uses for `claude -p`: the override wins, then
/// the known install dirs, then PATH. A bundled `.app` starts with a minimal
/// PATH, so probing is not optional.
fn agent_bin() -> PathBuf {
    PathBuf::from(gilb_config::resolve_agent_bin("claude", AGENT_BIN_ENV))
}

fn agent_args() -> Vec<String> {
    std::env::var(AGENT_ARGS_ENV)
        .ok()
        .map(|args| args.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

/// The prompt, from `<data dir>/prompts/realtime_assist.md`. Read when a
/// session opens — once per meeting, since the engine resets the session at
/// each new one — so an edit applies to the next meeting without restarting
/// the app. It does *not* apply mid-meeting: the agent was already given the
/// old prompt as its opening turn, and re-sending a new one would contradict
/// it.
struct FileAssistConfig {
    path: PathBuf,
}

impl FileAssistConfig {
    /// Seeds the user's copy from `bundled` on first run. A missing bundled
    /// prompt is an error rather than a silent fallback: a prompter without
    /// its `[NO_RESP]` discipline would talk over every meeting, which is
    /// worse than not starting.
    fn load(bundled: Option<&Path>) -> Result<Self> {
        let path = gilb_config::assist_prompt_path().context("resolve the prompt path")?;
        if !path.exists() {
            let bundled = bundled.ok_or_else(|| {
                anyhow!("this build ships no {}", gilb_config::ASSIST_PROMPT_FILE)
            })?;
            gilb_config::ensure_prompts_dir()?;
            std::fs::copy(bundled, &path)
                .with_context(|| format!("seed {} from {}", path.display(), bundled.display()))?;
            info!(path = %path.display(), "assist prompt created — edit it to tune suggestions");
        }
        Ok(Self { path })
    }
}

#[async_trait]
impl AssistConfig for FileAssistConfig {
    async fn system_prompt(&self) -> Result<String> {
        let text = tokio::fs::read_to_string(&self.path)
            .await
            .with_context(|| format!("read {}", self.path.display()))?;
        Ok(text)
    }

    /// The user switch (shared prefs) is the only flag here — gilb has no
    /// server to ask, and the shell already gates on it.
    async fn enabled(&self) -> bool {
        true
    }

    /// A local agent is slower than a cloud model, so batch harder than a
    /// hosted backend would: fewer, better-fed requests beat a queue that
    /// never drains.
    async fn turns_before_analysis(&self) -> u32 {
        4
    }
}
