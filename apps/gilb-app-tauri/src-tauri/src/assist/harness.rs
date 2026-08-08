//! Which coding agent gilb reaches, and how it is configured for a meeting.
//!
//! The table of agents — which CLIs are looked for, what actually speaks ACP
//! for each, and what each calls its knobs — is no longer gilb's: it lives in
//! [`acp_agents`], shared with the other products that reach a local agent the
//! same way, together with the resolution rules (an installed adapter beats
//! fetching one, a bare `agent` is only Cursor's when the file behind it says
//! so) and the tests that pin them.
//!
//! What stays here is what is gilb's own: the env overrides, where *this* app
//! looks for binaries, the deadline a meeting can afford, and the rule that a
//! preferred tier is only ever applied when the agent advertises it.

use std::path::PathBuf;
use std::time::Duration;

use acp_agents::Lookup;
use gilb_assist_acp::{agent_available, AcpConfig};
use tracing::warn;

use super::TURN_TIMEOUT;

/// Every agent this project knows how to reach, in the order the panel offers
/// them. Re-exported so the rest of the module has one name for it.
pub(super) use acp_agents::{Harness, HARNESSES};

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

/// The first ACP turn after a cold `npx` start includes downloading the
/// adapter, which is a different order of magnitude from starting a binary
/// that is already on disk.
const NPX_STARTUP_TIMEOUT: Duration = Duration::from_secs(180);

/// Where this app looks for a binary, in the order it trusts them.
///
/// `gilb_config` owns that list — the analyzer resolves `claude` through the
/// same one — so it is handed to the catalogue rather than replaced by it. The
/// version-manager directories matter for the same reason they always do here:
/// a bundle launched from Finder is given launchd's PATH, where a perfectly
/// good agent is invisible.
fn lookup() -> Lookup {
    let mut dirs: Vec<PathBuf> = gilb_config::agent_bin_dirs()
        .into_iter()
        .map(PathBuf::from)
        .collect();
    dirs.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    Lookup {
        search: Some(dirs),
        // Nothing here asks the user to install a second thing: if the adapter
        // is not on disk it is fetched on first use, which is how the editors
        // that pioneered this do it too.
        allow_npx: true,
        ..Lookup::default()
    }
}

/// Whether this agent's own CLI is on the machine. Its presence is what makes
/// a harness a candidate — the adapter is our problem, not the user's.
pub(super) fn installed(harness: &Harness) -> bool {
    harness.installed_cli(&lookup()).is_some()
}

/// What to spawn for an ACP session.
pub(super) struct Agent {
    pub(super) bin: PathBuf,
    pub(super) args: Vec<String>,
    /// Longer when the first run has to fetch the adapter.
    startup_timeout: Duration,
    /// What the chosen agent calls its effort knob: Claude Code says `effort`,
    /// Codex says `reasoning_effort`.
    pub(super) effort_config_id: &'static str,
}

/// The ACP agent to run, resolved from what the user already has installed.
///
/// The override wins outright. Otherwise the agent the user picked decides,
/// and the catalogue works out what to run for it.
///
/// `None` when there is nothing to run. That is a real state, not an error:
/// the UI hides the feature rather than offering a switch that ends in a
/// handshake timeout every meeting.
pub(super) fn agent() -> Option<Agent> {
    let env_args = std::env::var(AGENT_ARGS_ENV).ok().map(|args| {
        args.split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>()
    });

    if let Ok(bin) = std::env::var(AGENT_BIN_ENV) {
        let bin = bin.trim();
        if bin.eq_ignore_ascii_case(AGENT_NONE) {
            tracing::info!("{AGENT_BIN_ENV}={AGENT_NONE}: pretending no agent is installed");
            return None;
        }
        if !bin.is_empty() {
            let bin = PathBuf::from(bin);
            // Checked rather than taken on faith: an override with a typo
            // would otherwise report the feature as ready and then fail the
            // handshake once per meeting, which looks like a hang and says
            // nothing about the cause.
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
    // first in a list is not a decision to make on somebody's behalf.
    //
    // And a choice, once made, is the whole answer — including "the one you
    // picked is gone", which must not quietly fall through to another vendor.
    let chosen = gilb_config::load_preferences().assist_agent?;
    let harness = acp_agents::harness(&chosen)?;
    let launch = acp_agents::launch(harness, &lookup())?;

    Some(Agent {
        bin: launch.bin,
        args: env_args.unwrap_or(launch.args),
        startup_timeout: if launch.fetches {
            NPX_STARTUP_TIMEOUT
        } else {
            AcpConfig::default().startup_timeout
        },
        effort_config_id: harness.effort_config_id,
    })
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
            // The bin directories, ahead of whatever we were launched with.
            // Resolving the agent's own path is not enough: `npx` goes on to
            // look up `node` by name, and a bundle's PATH has no node in it.
            path_env: Some(gilb_config::agent_path_env()),
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

    use gilb_assist_acp::{SessionChoice, SessionOption};

    fn opt(category: &str, id: &str, values: &[&str]) -> SessionOption {
        SessionOption {
            id: id.into(),
            name: id.into(),
            category: category.into(),
            kind: "select".into(),
            current: values.first().unwrap_or(&"").to_string(),
            choices: values
                .iter()
                .map(|v| SessionChoice {
                    value: v.to_string(),
                    name: v.to_string(),
                })
                .collect(),
        }
    }

    fn claude() -> &'static Harness {
        acp_agents::harness("claude").unwrap()
    }
    fn codex() -> &'static Harness {
        acp_agents::harness("codex").unwrap()
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
