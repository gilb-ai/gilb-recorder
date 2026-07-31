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

/// Override the agent command; also how a power user points gilb at a wrapper
/// script or an in-house ACP adapter.
const AGENT_BIN_ENV: &str = "GILB_ASSIST_AGENT";
/// Extra arguments, space-separated. Replaces the defaults for a known agent.
const AGENT_ARGS_ENV: &str = "GILB_ASSIST_AGENT_ARGS";

/// Coding agents we know how to reach over ACP, in preference order.
///
/// The CLI a user has installed may or may not be the thing that speaks ACP.
/// `claude` is an interactive REPL: pipe an ACP `initialize` into it and
/// nothing comes back, and the session dies at the handshake timeout — so
/// Claude Code and Codex are reached through adapter packages. Cursor speaks
/// the protocol itself, behind an `acp` subcommand.
///
/// So: find the CLI, then work out what to run *for* it. Nothing here asks the
/// user to install a second thing — if the adapter is not on disk it is
/// fetched by `npx` on first use, which is how the editors that pioneered this
/// (Zed, block/buzz) do it too.
const HARNESSES: &[Harness] = &[
    Harness {
        name: "Claude Code",
        cli: &["claude"],
        // Both adapter names: `@zed-industries/claude-code-acp` was renamed to
        // `@agentclientprotocol/claude-agent-acp`, and a machine may have
        // either installed. We *fetch* the current one.
        adapter_bin: Some(&["claude-agent-acp", "claude-code-acp"]),
        npx_package: Some("@agentclientprotocol/claude-agent-acp"),
        cli_acp_args: &[],
    },
    Harness {
        name: "Codex",
        cli: &["codex"],
        adapter_bin: Some(&["codex-acp"]),
        npx_package: Some("@agentclientprotocol/codex-acp"),
        cli_acp_args: &[],
    },
    Harness {
        name: "Cursor",
        // Cursor renamed its CLI from `cursor-agent` to `agent`. The specific
        // name goes first: `agent` is generic enough to belong to something
        // else entirely on a given machine.
        cli: &["cursor-agent", "agent"],
        adapter_bin: None,
        npx_package: None,
        cli_acp_args: &["acp"],
    },
];

impl Harness {
    /// An adapter for this harness that is already installed, if any.
    fn installed_adapter(&self) -> Option<PathBuf> {
        Self::first_installed(self.adapter_bin?)
    }

    /// The harness's own CLI, if the user has it.
    fn installed_cli(&self) -> Option<PathBuf> {
        Self::first_installed(self.cli)
    }

    fn first_installed(names: &[&str]) -> Option<PathBuf> {
        names
            .iter()
            .map(|name| resolve(name))
            .find(|bin| agent_available(bin))
    }
}

struct Harness {
    /// What to call it in the UI — the product's name, not our binary.
    name: &'static str,
    /// Names the coding CLI may go by, most specific first. Its presence is
    /// what makes this harness a candidate — the adapter is our problem, not
    /// theirs.
    cli: &'static [&'static str],
    /// Adapter executables to look for before fetching one, newest name first.
    adapter_bin: Option<&'static [&'static str]>,
    /// npm package providing that adapter, run through `npx` when the binary
    /// is not installed. First run downloads it; later runs come from the npx
    /// cache.
    npx_package: Option<&'static str>,
    /// Arguments that put the CLI *itself* into ACP mode, for agents that need
    /// no adapter. Empty means "this CLI cannot speak ACP on its own".
    cli_acp_args: &'static [&'static str],
}

/// The first ACP turn after a cold `npx` start includes downloading the
/// adapter, which is a different order of magnitude from starting a binary
/// that is already on disk.
const NPX_STARTUP_TIMEOUT: Duration = Duration::from_secs(180);

/// The bundled copy of the prompt, inside the app's resources — a file the
/// packager ships, not a string compiled into the binary. Seeded into the
/// user's prompts directory on first run; after that the user's copy wins and
/// this one is never read again.
const BUNDLED_PROMPT: &str = "resources/prompts/realtime_assist.md";

/// A suggestion is worthless once the conversation has moved on. Generous
/// enough for a local model's first token, short enough to stay a suggestion.
const TURN_TIMEOUT: Duration = Duration::from_secs(15);

pub fn init(app: &AppHandle, db: std::sync::Arc<gilb_db::Db>) {
    // Resolve the bundled prompt once, at init: `AssistHost::engine` is called
    // from the pipeline with no `AppHandle` in reach, and resource paths differ
    // between a dev run and an installed bundle.
    let bundled = app
        .path()
        .resolve(BUNDLED_PROMPT, BaseDirectory::Resource)
        .inspect_err(|err| warn!(error = %err, "bundled assist prompt not found"))
        .ok();
    gilb_shell_tauri::assist::init(app, GilbAssistHost { bundled, db });
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
    /// Only to look up where a meeting is being recorded, so its suggestions
    /// can be filed in the same folder.
    db: std::sync::Arc<gilb_db::Db>,
}

impl AssistHost for GilbAssistHost {
    /// No agent, no feature. Same shape as the whisper-model gate: the UI shows
    /// what is missing instead of failing once per suggestion.
    fn available(&self) -> bool {
        agent().is_some()
    }

    fn engine(&self) -> Result<(Box<dyn AssistConfig>, Box<dyn AssistBackend>)> {
        let config = FileAssistConfig::load(self.bundled.as_deref())?;
        let agent = agent().ok_or_else(|| {
            anyhow!(
                "no coding agent found — install Claude Code or Codex, \
                 or point {AGENT_BIN_ENV} at an ACP-speaking command"
            )
        })?;
        info!(bin = %agent.bin.display(), args = ?agent.args, "assist backend");
        let acp = AcpConfig {
            bin: agent.bin,
            args: agent.args,
            startup_timeout: agent.startup_timeout,
            // The agent gets a scratch directory, not the user's project: a
            // meeting prompter has no business reading the working tree.
            cwd: std::env::temp_dir(),
            turn_timeout: TURN_TIMEOUT,
        };
        Ok((Box::new(config), Box::new(AcpBackend::new(acp))))
    }

    /// Name the agent the suggestions will run on. Whichever of several
    /// installed CLIs we picked is not something to leave the user guessing at
    /// — it decides which vendor sees the conversation.
    fn backend_label(&self) -> Option<String> {
        agent().map(|a| a.label)
    }

    /// `assist.md` in the meeting's own folder, next to `video.mp4` and
    /// `audio.wav`. A meeting is a thing the user opens as a folder; what the
    /// assistant said during it belongs there, not in a separate archive they
    /// have to know about.
    ///
    /// The recorder decides that folder (it stamps it with the start time), so
    /// it is read back off the meeting row rather than derived a second time —
    /// two places computing the same path is how they end up disagreeing.
    fn journal_path(&self, meeting_id: i64) -> Option<PathBuf> {
        let db = self.db.clone();
        let meeting = tauri::async_runtime::block_on(async move {
            gilb_db::meetings::get_meeting(&db, meeting_id).await
        });
        let meeting = meeting
            .inspect_err(|err| warn!(error = %err, meeting_id, "assist journal: meeting lookup"))
            .ok()??;
        // Audio is written for every recording; video may be absent.
        let dir = meeting
            .audio_path
            .as_deref()
            .or(meeting.video_path.as_deref())
            .and_then(|p| Path::new(p).parent().map(Path::to_path_buf))?;
        Some(dir.join("assist.md"))
    }

    fn strings(&self) -> AssistStrings {
        AssistStrings {
            window_title: "gilb".into(),
            app_name: "gilb".into(),
            model_downloaded: "Speech model ready — suggestions start at the next meeting.".into(),
            model_failed: "Could not download the speech model. Try again from the app.".into(),
            unavailable: "needs Claude Code or Codex installed".into(),
        }
    }
}

/// What to spawn for an ACP session.
struct Agent {
    bin: PathBuf,
    args: Vec<String>,
    /// Longer when the first run has to fetch the adapter.
    startup_timeout: Duration,
    /// For the UI: which agent this is, and how we are reaching it.
    label: String,
}

/// The ACP agent to run, resolved from what the user already has installed.
///
/// The override wins outright. Otherwise the first [`HARNESSES`] entry whose
/// CLI is present wins, and we work out what to run for it: an installed
/// adapter binary, else the CLI itself if it speaks ACP, else `npx` fetching
/// the adapter. Probing goes through the known install dirs as well as PATH —
/// a bundled `.app` starts with a minimal PATH.
///
/// `None` when the user has no coding agent at all. That is a real state, not
/// an error: the UI hides the feature rather than offering a switch that ends
/// in a handshake timeout every meeting.
fn agent() -> Option<Agent> {
    let env_args = std::env::var(AGENT_ARGS_ENV).ok().map(|args| {
        args.split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>()
    });

    if let Ok(bin) = std::env::var(AGENT_BIN_ENV) {
        if !bin.trim().is_empty() {
            let bin = PathBuf::from(bin);
            let label = bin
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| bin.display().to_string());
            return Some(Agent {
                bin,
                args: env_args.unwrap_or_default(),
                startup_timeout: AcpConfig::default().startup_timeout,
                label: format!("{label} (set by {AGENT_BIN_ENV})"),
            });
        }
    }

    HARNESSES.iter().find_map(|h| {
        // An adapter already on disk beats fetching one.
        if let Some(bin) = h.installed_adapter() {
            return Some(Agent {
                bin,
                args: env_args.clone().unwrap_or_default(),
                startup_timeout: AcpConfig::default().startup_timeout,
                label: h.name.to_string(),
            });
        }
        // The harness itself has to be installed for either remaining path:
        // the adapter drives that CLI, and npx-fetching one for a CLI the user
        // does not have would fail slowly instead of quickly.
        let cli = h.installed_cli()?;
        if !h.cli_acp_args.is_empty() {
            return Some(Agent {
                bin: cli,
                args: env_args
                    .clone()
                    .unwrap_or_else(|| h.cli_acp_args.iter().map(|a| a.to_string()).collect()),
                startup_timeout: AcpConfig::default().startup_timeout,
                label: h.name.to_string(),
            });
        }
        let package = h.npx_package?;
        let npx = resolve("npx");
        agent_available(&npx).then(|| Agent {
            bin: npx,
            // `-y` so a first run installs without waiting on a prompt nobody
            // is there to answer.
            args: vec!["-y".into(), package.into()],
            startup_timeout: NPX_STARTUP_TIMEOUT,
            label: h.name.to_string(),
        })
    })
}

fn resolve(name: &str) -> PathBuf {
    PathBuf::from(gilb_config::resolve_agent_bin(name, AGENT_BIN_ENV))
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
