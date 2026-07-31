//! gilb's half of the real-time assist stack: suggestions during a live
//! meeting, produced by a **local agent** over ACP.
//!
//! The overlay window, the switch, the whisper-model gate and the audio
//! pipeline all come from `gilb_shell_tauri::assist`; what lives here is the
//! three things that are gilb's own:
//!
//! * **availability** — the agent binary exists. Nothing to run without it, and
//!   the UI says so instead of failing per suggestion;
//! * **the prompt** — a file in `~/.gilb/`. Deliberately local and never
//!   uploaded: a prompter's prompt holds prices and objection handling, which
//!   is the user's business, and the file is theirs to edit;
//! * **the backend** — an ACP session against that agent.
//!
//! The trust boundary a server-backed product needs (fencing recorded speech,
//! keeping the operator's question outside the fence) is *not* enforced here:
//! a local agent is a single-user tool talking to its owner, with no tools and
//! nobody else's data to reach. When this role grows tool access, revisit —
//! `AssistSession::ask` already carries the two halves separately.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use gilb_assist::{AssistBackend, AssistConfig};
use gilb_assist_acp::{agent_available, AcpBackend, AcpConfig};
use gilb_shell_tauri::assist::{AssistHost, AssistStrings};
use tauri::AppHandle;

/// Override the agent binary; also how a test or a power user points gilb at
/// `gemini`, a wrapper script, or a Hermes ACP adapter.
const AGENT_BIN_ENV: &str = "GILB_ASSIST_AGENT";
/// Extra arguments, space-separated (e.g. `--experimental-acp`).
const AGENT_ARGS_ENV: &str = "GILB_ASSIST_AGENT_ARGS";

/// The prompt that makes the agent a meeting prompter rather than a coding
/// assistant. Shipped on first run so there is something to edit.
const PROMPT_FILE: &str = "assist-prompt.md";

const DEFAULT_PROMPT: &str = include_str!("../assets/assist-prompt.md");

/// A suggestion is worthless once the conversation has moved on. Generous
/// enough for a local model's first token, short enough to stay a suggestion.
const TURN_TIMEOUT: Duration = Duration::from_secs(15);

pub fn init(app: &AppHandle) {
    gilb_shell_tauri::assist::init(app, GilbAssistHost);
}

// `gilb_shell_tauri::assist::refresh` re-evaluates availability. Nothing calls
// it here: the agent binary appears while the app is not looking, so there is
// no event to hang it on. A product whose availability follows a sign-in calls
// it there.

struct GilbAssistHost;

impl AssistHost for GilbAssistHost {
    /// No agent, no feature. Same shape as the whisper-model gate: the UI shows
    /// what is missing instead of failing once per suggestion.
    fn available(&self) -> bool {
        agent_available(&agent_bin())
    }

    fn engine(&self) -> Result<(Box<dyn AssistConfig>, Box<dyn AssistBackend>)> {
        let config = FileAssistConfig::load()?;
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

/// Prompt and knobs from `~/.gilb/`. Read per analysis (the trait is queried
/// every time), so editing the file takes effect on the next suggestion — no
/// restart, which is what makes prompt tuning bearable.
struct FileAssistConfig {
    path: PathBuf,
}

impl FileAssistConfig {
    fn load() -> Result<Self> {
        let path = gilb_config::data_dir()
            .context("resolve gilb data dir")?
            .join(PROMPT_FILE);
        if !path.exists() {
            std::fs::write(&path, DEFAULT_PROMPT)
                .with_context(|| format!("write default prompt to {}", path.display()))?;
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
