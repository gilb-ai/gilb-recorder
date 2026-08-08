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

mod harness;

use std::path::{Path, PathBuf};
use std::time::Duration;

use harness::{agent, installed, seed_choices, AGENT_BIN_ENV, EFFORT_ENV, HARNESSES, MODEL_ENV};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use gilb_assist::{AssistBackend, AssistConfig};
use gilb_assist_acp::AcpBackend;
use gilb_assist_audio::{LocalTranscriber, SharedModel};
use gilb_shell_tauri::assist::{
    AgentChoice, AssistHost, AssistStrings, SessionChoiceInfo, SessionOptionInfo,
};
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Manager};
use tracing::{info, warn};

/// The bundled copy of the prompt, inside the app's resources — a file the
/// packager ships, not a string compiled into the binary. Seeded into the
/// user's prompts directory on first run; after that the user's copy wins and
/// this one is never read again.
const BUNDLED_PROMPT: &str = "resources/prompts/realtime_assist.md";

/// A suggestion is worthless once the conversation has moved on. Generous
/// enough for a local model's first token, short enough to stay a suggestion.
const TURN_TIMEOUT: Duration = Duration::from_secs(15);

/// Where the ACP agent process groups are written down, so a launch after a
/// crash can clean up what no destructor got to.
fn agent_registry() -> Option<PathBuf> {
    gilb_config::data_dir()
        .inspect_err(|err| warn!(error = %err, "assist: no data dir for the agent registry"))
        .ok()
        .map(|dir| dir.join("acp-agents.json"))
}

/// Kill agents a previous run left behind, before starting any of our own.
///
/// An adapter is reached through `npx`, so what dies with the app is the
/// wrapper — the agent itself is reparented and keeps its ~200 MB. Ordinary
/// exits are handled at the source (each agent gets its own process group);
/// this is for the exits that run no code at all.
pub fn reap_orphaned_agents() {
    let Some(path) = agent_registry() else { return };
    let killed = gilb_assist_acp::reap_orphaned_agents(&path);
    if killed > 0 {
        info!(killed, "assist: cleaned up agents left by a previous run");
    }
}

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
        let acp = agent.acp_config(config_options);
        Ok((Box::new(config), Box::new(AcpBackend::new(acp))))
    }

    /// Ask the chosen agent what a session can configure. The list is the
    /// agent's own — nothing to hardcode, nothing to fall out of date — and
    /// only the knobs a prompter cares about are surfaced: model and effort.
    /// Permission modes and the like stay ours to decide.
    fn session_options(&self) -> anyhow::Result<Vec<SessionOptionInfo>> {
        let agent = agent().ok_or_else(|| anyhow!("no agent chosen"))?;
        let acp = agent.acp_config(Vec::new());
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
                            label: c.name,
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
                installed: installed(h),
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
        if !installed(harness) {
            return Err(anyhow!("{} is not installed on this machine", harness.name));
        }
        let agent = agent().ok_or_else(|| anyhow!("could not resolve {}", harness.name))?;
        info!(bin = %agent.bin.display(), args = ?agent.args, "preparing assist agent");
        let acp = agent.acp_config(Vec::new());
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
    /// This meeting's own folder — where the recorder put `video.mp4` and
    /// `audio.wav`, and so where `assist.json`/`assist.txt` belong.
    ///
    /// Read from the meetings row rather than rebuilt from a timestamp: the
    /// recorder owns the naming, and a journal that guessed would drift the
    /// first time that changed.
    fn journal_dir(&self, meeting_id: i64) -> Option<PathBuf> {
        let db = self.db.clone();
        let meeting = tauri::async_runtime::block_on(async move {
            gilb_db::meetings::get_meeting(&db, meeting_id).await
        });
        let meeting = meeting
            .inspect_err(|err| warn!(error = %err, meeting_id, "assist journal: meeting lookup"))
            .ok()??;
        // Audio is written for every recording; video may be absent.
        meeting
            .audio_path
            .as_deref()
            .or(meeting.video_path.as_deref())
            .and_then(|p| Path::new(p).parent().map(Path::to_path_buf))
    }

    fn strings(&self) -> AssistStrings {
        AssistStrings {
            window_title: "gilb".into(),
            app_name: "gilb".into(),
            model_downloaded: "Speech model ready — suggestions start at the next meeting.".into(),
            model_failed: "Could not download the speech model. Try again from the app.".into(),
            unavailable: "Needs a coding agent: install Claude Code, Codex, Cursor \
                 or OpenCode and this turns on."
                .into(),
        }
    }
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

    /// The journal's folder cannot be resolved when a recording arms, and can
    /// be once the recorder has written its paths.
    ///
    /// This is the bug that made the feature look implemented and write
    /// nothing for every meeting: `Armed` carries the meeting id, the recorder
    /// fills in the paths from its own subscriber to that same event, and
    /// whoever asks first gets a row with no folder in it. Hence the lazy
    /// lookup on the first entry — pinned here, on both sides of the write.
    ///
    /// Not a `#[tokio::test]`, deliberately: `journal_dir` blocks on the
    /// database, and blocking inside an async context panics. Being callable
    /// only from a blocking thread is part of the contract, so the test holds
    /// itself to it — the shell calls this from the blocking pool.
    #[test]
    fn journal_dir_needs_the_paths_the_recorder_writes_after_arming() {
        let setup = tokio::runtime::Runtime::new().expect("runtime");
        let file =
            std::env::temp_dir().join(format!("gilb-journal-{}.sqlite", uuid::Uuid::new_v4()));
        let dir = std::env::temp_dir().join("gilb-journal-meeting");

        let (db, meeting_id) = setup.block_on(async {
            let db = std::sync::Arc::new(gilb_db::open_db(&file).await.expect("open db"));
            let id = gilb_db::meetings::insert_meeting(&db, 0, "test.app")
                .await
                .expect("insert meeting");
            (db, id)
        });
        let host = GilbAssistHost {
            bundled: None,
            db: db.clone(),
        };

        // As it is the instant a recording arms: a meeting, no folder yet.
        assert!(
            host.journal_dir(meeting_id).is_none(),
            "no folder is knowable before the recorder records one"
        );

        setup.block_on(async {
            gilb_db::meetings::set_recording_paths(
                &db,
                meeting_id,
                &dir.join("video.mp4").to_string_lossy(),
                &dir.join("audio.wav").to_string_lossy(),
            )
            .await
            .expect("set paths");
        });

        assert_eq!(
            host.journal_dir(meeting_id),
            Some(dir),
            "once the paths are in, the folder is the recording's own"
        );
        let _ = std::fs::remove_file(&file);
    }
}
