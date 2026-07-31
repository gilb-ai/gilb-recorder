//! Which coding agents gilb can reach, and how.
//!
//! The table is the product decision of this feature: which CLIs are looked
//! for, what actually speaks ACP for each (an adapter fetched by `npx`, or
//! the CLI itself behind a flag), which fast tier a fresh setup is seeded
//! with, and what each agent calls its knobs. Everything else here serves the
//! table — resolution through version-manager bin dirs, the env overrides,
//! and the only-if-advertised seeding rule.

use std::path::PathBuf;
use std::time::Duration;

use gilb_assist_acp::{agent_available, AcpConfig};
use tracing::{info, warn};

use super::TURN_TIMEOUT;

/// Override the agent command; also how a power user points gilb at a wrapper
/// script or an in-house ACP adapter.
///
/// Set it to [`AGENT_NONE`] to pretend nothing is installed — the only way to
/// see the empty state on a machine that has an agent, which is every machine
/// this is developed on.
pub(super) const AGENT_BIN_ENV: &str = "GILB_ASSIST_AGENT";

/// `GILB_ASSIST_AGENT=none` — "act as if no agent were installed".
const AGENT_NONE: &str = "none";
/// Extra arguments, space-separated. Replaces the defaults for a known agent.
const AGENT_ARGS_ENV: &str = "GILB_ASSIST_AGENT_ARGS";
/// Model for the suggestions session, e.g. `haiku` — one of the values the
/// agent's ACP `configOptions` advertises. Separate from the agent's own
/// default on purpose: a suggestion is worth having for ~15 seconds, and the
/// model someone picked for interactive coding (a heavyweight with high
/// reasoning effort) is usually the wrong shape for that.
pub(super) const MODEL_ENV: &str = "GILB_ASSIST_MODEL";
/// Reasoning effort for the session, e.g. `low`. Same mechanism.
pub(super) const EFFORT_ENV: &str = "GILB_ASSIST_EFFORT";

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
pub(super) const HARNESSES: &[Harness] = &[
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
    Harness {
        id: "opencode",
        // As the app and its bundle spell it.
        name: "OpenCode",
        // Nothing preferred, because there is nothing universal to prefer:
        // opencode's model list is whatever providers the user configured, so
        // the names differ from machine to machine. Its own default is the
        // only choice that is right everywhere, and seeding only ever applies
        // values the agent actually advertises anyway.
        preferred_model: None,
        preferred_effort: None,
        // Unused — opencode advertises `model` and `mode`, no thinking tier.
        // The effort row simply does not appear in Settings for it.
        effort_config_id: "effort",
        cli: &["opencode"],
        // Speaks the protocol itself: `opencode acp` starts an ACP server, so
        // there is no adapter to fetch.
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
    pub(super) fn installed_cli(&self) -> Option<PathBuf> {
        Self::first_installed(self.cli)
    }

    fn first_installed(names: &[&str]) -> Option<PathBuf> {
        names
            .iter()
            .map(|name| resolve(name))
            .find(|bin| agent_available(bin))
    }
}

pub(super) struct Harness {
    /// Stable id, persisted in preferences and sent to the UI. Never shown.
    pub(super) id: &'static str,
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
    pub(super) name: &'static str,
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

/// What to spawn for an ACP session.
pub(super) struct Agent {
    pub(super) bin: PathBuf,
    pub(super) args: Vec<String>,
    /// Longer when the first run has to fetch the adapter.
    startup_timeout: Duration,
    /// [`Harness::effort_config_id`] of the harness this came from.
    pub(super) effort_config_id: &'static str,
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
pub(super) fn agent() -> Option<Agent> {
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
            return Some(Agent {
                bin,
                args: env_args.unwrap_or_default(),
                startup_timeout: AcpConfig::default().startup_timeout,
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
            effort_config_id: h.effort_config_id,
        })
    })
}

fn resolve(name: &str) -> PathBuf {
    PathBuf::from(gilb_config::resolve_agent_bin(name, AGENT_BIN_ENV))
}

impl Agent {
    /// The one way this app talks to its agent. Everything varies by agent
    /// (binary, args, deadline for a possibly-downloading first start); what
    /// never varies is the sandbox: a scratch cwd, because a meeting prompter
    /// has no business reading the user's working tree.
    pub(super) fn acp_config(self, config_options: Vec<(String, String)>) -> AcpConfig {
        AcpConfig {
            bin: self.bin,
            args: self.args,
            startup_timeout: self.startup_timeout,
            cwd: std::env::temp_dir(),
            turn_timeout: TURN_TIMEOUT,
            // Written down so the next launch can finish the job if this one
            // ends without running any destructors.
            registry: super::agent_registry(),
            config_options,
        }
    }
}

/// The session defaults to seed for a freshly set-up agent: the harness's
/// preferred fast tier, but **only** the parts this agent actually advertises.
///
/// `None` means "leave the agent's own default" — which is the whole fallback
/// story: a preference that stopped existing (an adapter renamed its tiers, a
/// model was retired) is silently skipped, never sent, and can never fail the
/// setup. Categories, not ids, identify the knobs: Codex spells effort
/// `reasoning_effort`, Claude Code spells it `effort`.
pub(super) fn seed_choices(
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every harness has to have an answer to "how does this speak ACP?".
    ///
    /// The two answers are an adapter we fetch (`npx_package`) and a flag that
    /// puts the CLI itself into ACP mode (`cli_acp_args`). An entry with
    /// neither resolves to a bare interactive CLI, which reads our handshake,
    /// answers nothing, and fails at the startup timeout — the exact shape of
    /// the bug that made this feature look broken for a week.
    #[test]
    fn every_harness_knows_how_to_reach_its_agent() {
        for h in HARNESSES {
            assert!(
                h.npx_package.is_some() || !h.cli_acp_args.is_empty(),
                "{}: no adapter to fetch and no ACP flag — nothing would answer",
                h.id
            );
            assert!(!h.cli.is_empty(), "{}: no command to look for", h.id);
        }
    }

    /// Ids are persisted in preferences and matched on; two entries sharing one
    /// would make the user's choice ambiguous.
    #[test]
    fn harness_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for h in HARNESSES {
            assert!(seen.insert(h.id), "duplicate harness id `{}`", h.id);
        }
    }

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
