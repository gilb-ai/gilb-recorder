//! Agents that outlived the app, and how they stop doing that.
//!
//! An ACP adapter is reached through `npx`, so what we spawn is a wrapper:
//! `npm` → `node` → the agent itself. `kill_on_drop` reaps only the process we
//! spawned, and the grandchild carries on — reparented to `launchd`, holding
//! its ~200 MB, forever. Two of them accumulated in fifteen minutes of
//! ordinary use, and one from a crashed run was still resident three hours
//! later.
//!
//! Two things fix that, and both are needed:
//!
//! * every agent is spawned into **its own process group**, so dropping it
//!   signals the whole wrapper chain rather than the one process we happen to
//!   hold a handle to. This covers every ordinary exit;
//! * the groups are **written down**, so the next launch can finish the job
//!   after a crash or a `kill -9`, where no destructor ever runs.
//!
//! The registry is deliberately a plain file rather than a scan for
//! adapter-shaped processes: this machine's owner runs `claude` and `codex`
//! themselves, and a reaper that matched on names would kill the terminal
//! session they are sitting in. Only groups this app started are recorded, and
//! a recorded group is killed only if the live process still looks like what
//! was recorded — a pid is reused eventually, and the thing wearing it next is
//! not ours to kill.

use std::path::Path;

use serde_json::{json, Value};
use tracing::{debug, info, warn};

/// One agent group this process started.
struct Entry {
    /// Process group id — the spawned wrapper, and everything it went on to
    /// start.
    pgid: i32,
    /// What it was, for the identity check at reap time. The command of the
    /// group leader as we asked for it.
    cmd: String,
}

fn read(path: &Path) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text) else {
        warn!(path = %path.display(), "assist: unreadable agent registry, starting over");
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            Some(Entry {
                pgid: item.get("pgid")?.as_i64()? as i32,
                cmd: item.get("cmd")?.as_str()?.to_string(),
            })
        })
        .collect()
}

fn write(path: &Path, entries: &[Entry]) {
    let value = Value::Array(
        entries
            .iter()
            .map(|e| json!({ "pgid": e.pgid, "cmd": e.cmd }))
            .collect(),
    );
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(err) = std::fs::write(path, format!("{value:#}\n")) {
        warn!(error = %err, path = %path.display(), "assist: could not record the agent group");
    }
}

/// Note that `pgid` is ours, so a later launch can clean up after a crash.
pub(crate) fn register(path: &Path, pgid: i32, cmd: &str) {
    let mut entries = read(path);
    entries.retain(|e| e.pgid != pgid);
    entries.push(Entry {
        pgid,
        cmd: cmd.to_string(),
    });
    write(path, &entries);
}

/// Forget `pgid` — it has been shut down the ordinary way.
pub(crate) fn unregister(path: &Path, pgid: i32) {
    let mut entries = read(path);
    let before = entries.len();
    entries.retain(|e| e.pgid != pgid);
    if entries.len() != before {
        write(path, &entries);
    }
}

/// The command line of a running process, or `None` if it is gone.
#[cfg(unix)]
fn live_command(pid: i32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Does the process wearing `pgid` still look like the agent we recorded?
///
/// Compared on the binary's file name rather than the whole command: `npx`
/// rewrites what it execs, so the recorded and live strings differ in ways
/// that say nothing about identity, while the adapter's name survives.
#[cfg(unix)]
fn still_ours(pgid: i32, recorded: &str) -> bool {
    let Some(live) = live_command(pgid) else {
        return false;
    };
    let marker = recorded
        .split_whitespace()
        .next_back()
        .unwrap_or(recorded)
        .rsplit('/')
        .next()
        .unwrap_or(recorded);
    !marker.is_empty() && live.contains(marker)
}

/// Kill agent groups left behind by a previous run. Call once at startup,
/// before anything spawns an agent of its own.
///
/// Returns how many groups were signalled — a number worth logging, because a
/// non-zero one means the last exit was not a clean one.
#[cfg(unix)]
pub fn reap(path: &Path) -> usize {
    let entries = read(path);
    if entries.is_empty() {
        return 0;
    }
    let mut killed = 0;
    for entry in &entries {
        if !still_ours(entry.pgid, &entry.cmd) {
            debug!(pgid = entry.pgid, "assist: recorded agent is gone already");
            continue;
        }
        // SAFETY: a signal to a process group id we recorded ourselves, whose
        // leader was just confirmed to still be the agent we started.
        let sent = unsafe { libc::killpg(entry.pgid, libc::SIGTERM) };
        if sent == 0 {
            info!(pgid = entry.pgid, cmd = %entry.cmd, "assist: killed an agent left by a previous run");
            killed += 1;
        }
    }
    // The file has served its purpose either way: everything in it is dead or
    // was not ours. Keeping stale entries would only slow down every launch.
    let _ = std::fs::remove_file(path);
    killed
}

#[cfg(not(unix))]
pub fn reap(_path: &Path) -> usize {
    // Windows has no process groups in this sense; a job object is the
    // equivalent and none is set up yet. Left explicit rather than silently
    // pretending to have cleaned up.
    0
}

/// Signal a whole agent group. Used when the session is dropped.
#[cfg(unix)]
pub(crate) fn kill_group(pgid: i32) {
    // SAFETY: our own child's group, which we put it in at spawn.
    unsafe { libc::killpg(pgid, libc::SIGTERM) };
}

#[cfg(not(unix))]
pub(crate) fn kill_group(_pgid: i32) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> std::path::PathBuf {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        dir.join("agents.json")
    }

    #[test]
    fn a_registered_group_survives_a_round_trip_and_can_be_forgotten() {
        let path = temp();
        register(&path, 42, "npx claude-agent-acp");
        register(&path, 43, "npx codex-acp");
        let entries = read(&path);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].pgid, 42);

        unregister(&path, 42);
        let left = read(&path);
        assert_eq!(left.len(), 1, "only the one told to go should go");
        assert_eq!(left[0].pgid, 43);
    }

    /// Registering the same group twice must not accumulate — a re-registered
    /// pgid is the same agent, and two entries would mean two kills.
    #[test]
    fn registering_twice_records_once() {
        let path = temp();
        register(&path, 42, "npx claude-agent-acp");
        register(&path, 42, "npx claude-agent-acp");
        assert_eq!(read(&path).len(), 1);
    }

    /// The identity check is what keeps the reaper from killing a stranger who
    /// inherited the pid. A live process whose command does not match what was
    /// recorded is left alone.
    #[cfg(unix)]
    #[test]
    fn a_pid_wearing_someone_elses_command_is_not_ours() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;

        assert!(
            !still_ours(pid, "npx claude-agent-acp"),
            "a `sleep` is not the agent we wrote down"
        );
        assert!(
            still_ours(pid, "/usr/bin/sleep"),
            "and the same pid running what we recorded is"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    /// A recorded group that has already exited is simply forgotten.
    #[cfg(unix)]
    #[test]
    fn reaping_a_dead_group_kills_nothing_and_clears_the_file() {
        let path = temp();
        // A pid that cannot be running: one past the maximum on macOS.
        register(&path, 999_999, "npx claude-agent-acp");
        assert_eq!(reap(&path), 0);
        assert!(!path.exists(), "the registry is cleared after a reap");
    }
}
