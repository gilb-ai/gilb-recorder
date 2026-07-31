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
use gilb_assist_audio::{LocalTranscriber, SharedModel};
use gilb_shell_tauri::assist::{
    AgentChoice, AssistHost, AssistStrings, SessionChoiceInfo, SessionOptionInfo,
};
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Manager};
use tracing::{info, warn};

/// Override the agent command; also how a power user points gilb at a wrapper
/// script or an in-house ACP adapter.
///
/// Set it to [`AGENT_NONE`] to pretend nothing is installed — the only way to
/// see the empty state on a machine that has an agent, which is every machine
/// this is developed on.
const AGENT_BIN_ENV: &str = "GILB_ASSIST_AGENT";

/// `GILB_ASSIST_AGENT=none` — "act as if no agent were installed".
const AGENT_NONE: &str = "none";
/// Extra arguments, space-separated. Replaces the defaults for a known agent.
const AGENT_ARGS_ENV: &str = "GILB_ASSIST_AGENT_ARGS";
/// Model for the suggestions session, e.g. `haiku` — one of the values the
/// agent's ACP `configOptions` advertises. Separate from the agent's own
/// default on purpose: a suggestion is worth having for ~15 seconds, and the
/// model someone picked for interactive coding (a heavyweight with high
/// reasoning effort) is usually the wrong shape for that.
const MODEL_ENV: &str = "GILB_ASSIST_MODEL";
/// Reasoning effort for the session, e.g. `low`. Same mechanism.
const EFFORT_ENV: &str = "GILB_ASSIST_EFFORT";

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
        id: "claude",
        name: "Claude Code",
        preferred_model: Some("haiku"),
        preferred_effort: Some("low"),
        effort_config_id: "effort",
        cli: &["claude"],
        // Both adapter names: `@zed-industries/claude-code-acp` was renamed to
        // `@agentclientprotocol/claude-agent-acp`, and a machine may have
        // either installed. We *fetch* the current one.
        adapter_bin: Some(&["claude-agent-acp", "claude-code-acp"]),
        npx_package: Some("@agentclientprotocol/claude-agent-acp"),
        cli_acp_args: &[],
    },
    Harness {
        id: "codex",
        name: "Codex",
        // "Fast and affordable" in Codex's own words. If a future adapter
        // drops the value, seeding skips it and the agent default stands —
        // preferences are matched against what is advertised, never assumed.
        preferred_model: Some("gpt-5.6-luna"),
        preferred_effort: Some("low"),
        effort_config_id: "reasoning_effort",
        cli: &["codex"],
        adapter_bin: Some(&["codex-acp"]),
        npx_package: Some("@agentclientprotocol/codex-acp"),
        cli_acp_args: &[],
    },
    Harness {
        id: "cursor",
        name: "Cursor",
        preferred_model: None,
        preferred_effort: None,
        effort_config_id: "effort",
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
    /// Stable id, persisted in preferences and sent to the UI. Never shown.
    id: &'static str,
    /// Session model to select when the user sets this agent up, matched
    /// against what the agent actually advertises — never sent blind. A
    /// prompter's answer expires in seconds, so the fast tier is the right
    /// default; the coding default (here, whatever `~/.claude/settings.json`
    /// says) is tuned for the opposite trade. Visible and changeable in
    /// Settings — a default, not a decision made behind the user's back.
    preferred_model: Option<&'static str>,
    /// Same for reasoning effort.
    preferred_effort: Option<&'static str>,
    /// What this agent calls the effort knob in its `configOptions`. gilb's
    /// canonical id is `effort` (prefs, UI); the wire uses the agent's own —
    /// Claude Code says `effort`, Codex says `reasoning_effort`.
    effort_config_id: &'static str,
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
        // The settings screen persists the choice; the env vars are a dev
        // override and win when set.
        let prefs = gilb_config::load_preferences();
        let mut config_options = Vec::new();
        for (env, config_id, saved) in [
            (MODEL_ENV, "model", prefs.assist_model),
            (EFFORT_ENV, agent.effort_config_id, prefs.assist_effort),
        ] {
            let value = std::env::var(env).ok().filter(|v| !v.trim().is_empty());
            if let Some(value) = value.or(saved) {
                info!(config_id, value = %value, "assist session option");
                config_options.push((config_id.to_string(), value));
            }
        }
        let acp = AcpConfig {
            bin: agent.bin,
            args: agent.args,
            startup_timeout: agent.startup_timeout,
            // The agent gets a scratch directory, not the user's project: a
            // meeting prompter has no business reading the working tree.
            cwd: std::env::temp_dir(),
            turn_timeout: TURN_TIMEOUT,
            config_options,
        };
        Ok((Box::new(config), Box::new(AcpBackend::new(acp))))
    }

    /// Name the agent the suggestions will run on. Whichever of several
    /// installed CLIs we picked is not something to leave the user guessing at
    /// — it decides which vendor sees the conversation.
    fn backend_label(&self) -> Option<String> {
        agent().map(|a| a.label)
    }

    /// Ask the chosen agent what a session can configure. The list is the
    /// agent's own — nothing to hardcode, nothing to fall out of date — and
    /// only the knobs a prompter cares about are surfaced: model and effort.
    /// Permission modes and the like stay ours to decide.
    fn session_options(&self) -> anyhow::Result<Vec<SessionOptionInfo>> {
        let agent = agent().ok_or_else(|| anyhow!("no agent chosen"))?;
        let acp = AcpConfig {
            bin: agent.bin,
            args: agent.args,
            startup_timeout: agent.startup_timeout,
            cwd: std::env::temp_dir(),
            ..AcpConfig::default()
        };
        let options = tauri::async_runtime::block_on(gilb_assist_acp::probe_session_options(&acp))?;
        // Select by *category*, not id: Claude Code calls its effort knob
        // `effort`, Codex calls it `reasoning_effort`, and both file it under
        // `thought_level`. The UI and prefs speak gilb's canonical ids
        // (`model`/`effort`); the wire id is the harness's business
        // ([`Harness::effort_config_id`]).
        Ok(options
            .into_iter()
            .filter_map(|o| {
                let id = match o.category.as_str() {
                    "model" => "model",
                    "thought_level" => "effort",
                    _ => return None,
                };
                Some(SessionOptionInfo {
                    id: id.to_string(),
                    name: o.name,
                    agent_default: o.current,
                    choices: o
                        .choices
                        .into_iter()
                        .map(|c| SessionChoiceInfo {
                            value: c.value,
                            label: c.label,
                        })
                        .collect(),
                })
            })
            .collect())
    }

    /// The same model the post-meeting transcription worker uses.
    ///
    /// Without this the two paths load ~570 MB each, and the moment they
    /// overlap — a meeting ending while the panel is still warm — is not rare,
    /// it is every meeting.
    fn shared_model(&self) -> Option<std::sync::Arc<SharedModel<LocalTranscriber>>> {
        Some(crate::transcribe_worker::shared_model())
    }

    /// Every agent gilb knows how to reach, and whether its CLI is here.
    ///
    /// Uninstalled ones are listed too: "Codex — not installed" tells the user
    /// what their options are, which their absence from a list cannot.
    fn agents(&self) -> Vec<AgentChoice> {
        HARNESSES
            .iter()
            .map(|h| AgentChoice {
                id: h.id.to_string(),
                label: h.name.to_string(),
                installed: h.installed_cli().is_some(),
            })
            .collect()
    }

    /// Install the agent's ACP adapter by *using* it: open a session and let
    /// the handshake happen.
    ///
    /// That is the whole install — `npx` fetches the package on first run —
    /// and it is also the only proof worth having. A package that downloaded
    /// but does not answer `initialize` is not installed in any sense the user
    /// cares about, and finding that out here beats finding out mid-meeting.
    fn prepare(&self, agent_id: &str) -> Result<()> {
        let harness = HARNESSES
            .iter()
            .find(|h| h.id == agent_id)
            .ok_or_else(|| anyhow!("unknown agent `{agent_id}`"))?;
        if harness.installed_cli().is_none() {
            return Err(anyhow!("{} is not installed on this machine", harness.name));
        }
        let agent = agent().ok_or_else(|| anyhow!("could not resolve {}", harness.name))?;
        info!(bin = %agent.bin.display(), args = ?agent.args, "preparing assist agent");
        let acp = AcpConfig {
            bin: agent.bin,
            args: agent.args,
            startup_timeout: agent.startup_timeout,
            cwd: std::env::temp_dir(),
            turn_timeout: TURN_TIMEOUT,
            ..AcpConfig::default()
        };
        // The options probe *is* the handshake check — initialize plus
        // session/new — and it comes back with the knobs, which is what lets
        // the default below be honest: preferred values are applied only when
        // the agent actually advertises them, never sent blind.
        let options = tauri::async_runtime::block_on(gilb_assist_acp::probe_session_options(&acp))
            .with_context(|| format!("{} could not start", harness.name))?;

        let (model, effort) = seed_choices(harness, &options);
        if model.is_some() || effort.is_some() {
            info!(?model, ?effort, "assist session defaults for this agent");
            gilb_config::update_preferences(|p| {
                // Setting up an agent is when its session gets its defaults —
                // including on a re-pick, where keeping the previous agent's
                // leftovers would be the surprise.
                p.assist_model = model.clone();
                p.assist_effort = effort.clone();
            })?;
        }
        Ok(())
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
            unavailable: "Needs a coding agent: install Claude Code, Codex or Cursor \
                 and this turns on."
                .into(),
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
    /// [`Harness::effort_config_id`] of the harness this came from.
    effort_config_id: &'static str,
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
        let bin = bin.trim();
        if bin.eq_ignore_ascii_case(AGENT_NONE) {
            info!("{AGENT_BIN_ENV}={AGENT_NONE}: pretending no agent is installed");
            return None;
        }
        if !bin.is_empty() {
            let bin = PathBuf::from(bin);
            // Check it exists rather than taking the user's word for it: an
            // override with a typo would otherwise report the feature as ready
            // and then fail the handshake once per meeting, which looks like a
            // hang and says nothing about the cause.
            if !agent_available(&bin) {
                warn!(
                    bin = %bin.display(),
                    "{AGENT_BIN_ENV} points at something that is not there"
                );
                return None;
            }
            let label = bin
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| bin.display().to_string());
            return Some(Agent {
                bin,
                args: env_args.unwrap_or_default(),
                startup_timeout: AcpConfig::default().startup_timeout,
                label: format!("{label} (set by {AGENT_BIN_ENV})"),
                effort_config_id: "effort",
            });
        }
    }

    // No choice yet means no agent — deliberately, so the UI asks instead of
    // guessing. Which coding CLI runs the suggestions decides whose model
    // hears the meeting; picking that silently because it happened to be
    // first in a list is not a decision to make on someone's behalf.
    //
    // And a choice, once made, is the whole answer — including "the one you
    // picked is gone", which must not quietly fall through to another vendor.
    let chosen = gilb_config::load_preferences().assist_agent?;

    HARNESSES.iter().filter(|h| h.id == chosen).find_map(|h| {
        // An adapter already on disk beats fetching one.
        if let Some(bin) = h.installed_adapter() {
            return Some(Agent {
                bin,
                args: env_args.clone().unwrap_or_default(),
                startup_timeout: AcpConfig::default().startup_timeout,
                label: h.name.to_string(),
                effort_config_id: h.effort_config_id,
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
                effort_config_id: h.effort_config_id,
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
            effort_config_id: h.effort_config_id,
        })
    })
}

fn resolve(name: &str) -> PathBuf {
    PathBuf::from(gilb_config::resolve_agent_bin(name, AGENT_BIN_ENV))
}

/// The session defaults to seed for a freshly set-up agent: the harness's
/// preferred fast tier, but **only** the parts this agent actually advertises.
///
/// `None` means "leave the agent's own default" — which is the whole fallback
/// story: a preference that stopped existing (an adapter renamed its tiers, a
/// model was retired) is silently skipped, never sent, and can never fail the
/// setup. Categories, not ids, identify the knobs: Codex spells effort
/// `reasoning_effort`, Claude Code spells it `effort`.
fn seed_choices(
    harness: &Harness,
    options: &[gilb_assist_acp::SessionOption],
) -> (Option<String>, Option<String>) {
    let advertised = |category: &str, wanted: &str| {
        options
            .iter()
            .find(|o| o.category == category)
            .is_some_and(|o| o.choices.iter().any(|c| c.value == wanted))
    };
    let model = harness
        .preferred_model
        .filter(|m| advertised("model", m))
        .map(str::to_string);
    let effort = harness
        .preferred_effort
        .filter(|e| advertised("thought_level", e))
        .map(str::to_string);
    (model, effort)
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

    /// Every utterance goes to the agent.
    ///
    /// This used to be 4, reasoning that a local agent is slow so fewer,
    /// better-fed requests would beat a queue that never drains. What it
    /// actually bought was silence: someone asks one question out loud, three
    /// turns never arrive, and the feature looks broken. A suggestion nobody
    /// receives is not cheaper — it is worthless.
    ///
    /// Bursts are still coalesced, by `EngineParams::min_analysis_interval`
    /// rather than by a count: turns that arrive while the throttle is cooling
    /// down go in the next request together. That batches a fast talker
    /// without making a slow one wait for company.
    async fn turns_before_analysis(&self) -> u32 {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gilb_assist_acp::{SessionChoice, SessionOption};

    fn opt(category: &str, id: &str, values: &[&str]) -> SessionOption {
        SessionOption {
            id: id.into(),
            name: id.into(),
            category: category.into(),
            current: values.first().unwrap_or(&"").to_string(),
            choices: values
                .iter()
                .map(|v| SessionChoice {
                    value: v.to_string(),
                    label: v.to_string(),
                })
                .collect(),
        }
    }

    fn claude() -> &'static Harness {
        HARNESSES.iter().find(|h| h.id == "claude").unwrap()
    }
    fn codex() -> &'static Harness {
        HARNESSES.iter().find(|h| h.id == "codex").unwrap()
    }

    #[test]
    fn preferred_tier_is_seeded_when_the_agent_advertises_it() {
        let options = [
            opt("model", "model", &["default", "sonnet", "haiku"]),
            opt("thought_level", "effort", &["default", "low"]),
        ];
        assert_eq!(
            seed_choices(claude(), &options),
            (Some("haiku".into()), Some("low".into()))
        );
    }

    /// The contract this module was asked to keep: a preference the agent does
    /// not advertise is skipped — the agent's own default stands, and setup
    /// never fails over a model that stopped existing.
    #[test]
    fn a_vanished_preference_falls_back_to_the_agent_default() {
        let options = [
            // An adapter update renamed every tier; "haiku" is gone.
            opt("model", "model", &["fast-2", "deep-2"]),
            // And the effort knob disappeared entirely.
        ];
        assert_eq!(seed_choices(claude(), &options), (None, None));
    }

    /// Codex spells the effort knob `reasoning_effort`; the category is what
    /// identifies it. An id-based match would silently skip the seed.
    #[test]
    fn codex_effort_is_found_by_category_not_id() {
        let options = [
            opt(
                "model",
                "model",
                &["gpt-5.5", "gpt-5.6-luna", "gpt-5.4-mini"],
            ),
            opt(
                "thought_level",
                "reasoning_effort",
                &["low", "medium", "high"],
            ),
        ];
        assert_eq!(
            seed_choices(codex(), &options),
            (Some("gpt-5.6-luna".into()), Some("low".into()))
        );
    }

    /// No options at all — an adapter that advertises nothing. Nothing seeded,
    /// nothing sent, nothing failed.
    #[test]
    fn an_agent_with_no_knobs_seeds_nothing() {
        assert_eq!(seed_choices(claude(), &[]), (None, None));
    }
}
