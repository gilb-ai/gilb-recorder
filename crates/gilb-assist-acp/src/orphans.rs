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
//! a recorded group is killed only if a live process in it still looks like
//! what was recorded — a pid is reused eventually, and the thing wearing it
//! next is not ours to kill.

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
    // Temp file in the same directory + rename: a crash mid-write must not
    // leave a truncated registry that `read` would then silently forget.
    let tmp = path.with_extension("tmp");
    let result =
        std::fs::write(&tmp, format!("{value:#}\n")).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(err) = result {
        let _ = std::fs::remove_file(&tmp);
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

/// Command lines of every live process in the group (empty when it is gone).
#[cfg(unix)]
fn group_commands(pgid: i32) -> Vec<String> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-eo", "pgid=,command="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (group, cmd) = line.trim_start().split_once(char::is_whitespace)?;
            (group.parse::<i32>().ok()? == pgid).then(|| cmd.trim().to_string())
        })
        .collect()
}

/// Does the recorded group still hold a process that looks like the agent we
/// wrote down? ANY member counts: the npx wrapper we spawned may be long dead
/// while the agent it started lives on in the same group.
///
/// Compared on the binary's file name rather than the whole command: `npx`
/// rewrites what it execs, so the recorded and live strings differ in ways
/// that say nothing about identity, while the adapter's name survives.
#[cfg(unix)]
fn still_ours(pgid: i32, recorded: &str) -> bool {
    let marker = recorded
        .split_whitespace()
        .next_back()
        .unwrap_or(recorded)
        .rsplit('/')
        .next()
        .unwrap_or(recorded);
    !marker.is_empty()
        && group_commands(pgid)
            .iter()
            .any(|live| live.contains(marker))
}

/// Kill agent groups left behind by a previous run. Call once at startup,
/// before anything spawns an agent of its own.
///
/// Returns how many groups were confirmed dead — a number worth logging,
/// because a non-zero one means the last exit was not a clean one. A group
/// that resists (the signal refused, or SIGTERM ignored) stays written down
/// for the next launch instead of being forgotten.
#[cfg(unix)]
pub fn reap(path: &Path) -> usize {
    let entries = read(path);
    if entries.is_empty() {
        return 0;
    }
    let mut killed = 0;
    let mut survivors = Vec::new();
    for entry in entries {
        if !still_ours(entry.pgid, &entry.cmd) {
            debug!(pgid = entry.pgid, "assist: recorded agent is gone already");
            continue;
        }
        // SAFETY: a signal to a process group id we recorded ourselves, whose
        // members were just confirmed to still include the agent we started.
        let sent = unsafe { libc::killpg(entry.pgid, libc::SIGTERM) };
        if sent != 0 {
            warn!(pgid = entry.pgid, cmd = %entry.cmd, "assist: could not signal the recorded agent group");
            survivors.push(entry);
            continue;
        }
        // SIGTERM is a request, and its delivery says nothing about
        // compliance: only a group that is actually gone leaves the list.
        let mut gone = false;
        for _ in 0..10 {
            if group_commands(entry.pgid).is_empty() {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if gone {
            info!(pgid = entry.pgid, cmd = %entry.cmd, "assist: killed an agent left by a previous run");
            killed += 1;
        } else {
            warn!(pgid = entry.pgid, cmd = %entry.cmd, "assist: recorded agent ignored SIGTERM; keeping it on the list");
            survivors.push(entry);
        }
    }
    if survivors.is_empty() {
        // The file has served its purpose: everything in it is dead or was
        // not ours. Keeping stale entries would only slow down every launch.
        let _ = std::fs::remove_file(path);
    } else {
        write(path, &survivors);
    }
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
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            // Its own group, the way real agents are spawned: the check looks
            // at the group, and this pid is nobody's leader otherwise.
            .process_group(0)
            .spawn()
            .expect("spawn sleep");
        let pgid = child.id() as i32;

        assert!(
            !still_ours(pgid, "npx claude-agent-acp"),
            "a `sleep` is not the agent we wrote down"
        );
        assert!(
            still_ours(pgid, "/usr/bin/sleep"),
            "and the same group running what we recorded is"
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

    /// The registry goes through a temp file and a rename, so a crash
    /// mid-write can never leave a half file for the next launch to read.
    /// What must not survive the write is the temp file itself.
    #[test]
    fn writes_are_atomic_and_leave_no_temp_file() {
        let path = temp();
        register(&path, 42, "npx claude-agent-acp");
        assert!(path.exists());
        assert!(
            !path.with_extension("tmp").exists(),
            "the temp file must be renamed away"
        );
    }

    /// The npx-wrapper shape: the leader we spawned is dead, the agent it
    /// started lives on in the same group. The reaper must still recognize
    /// the group as ours and take it down.
    #[cfg(unix)]
    #[test]
    fn a_group_is_alive_while_any_member_matches() {
        use std::os::unix::process::CommandExt;
        let path = temp();
        // The leader backgrounds a stand-in agent and exits, leaving the
        // grandchild holding the group.
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 60 & exit 0"])
            .process_group(0)
            .spawn()
            .expect("spawn sh");
        let pgid = child.id() as i32;
        let _ = child.wait(); // the leader dies, the group lives on

        register(&path, pgid, "sleep");
        assert_eq!(
            reap(&path),
            1,
            "a live grandchild keeps the group on the reaper's list"
        );
        assert!(!path.exists(), "a reaped group is struck from the registry");
    }

    /// A group that shrugs off SIGTERM must not be forgotten: it stays
    /// written down for the next launch instead of being declared dead.
    #[cfg(unix)]
    #[test]
    fn a_group_that_ignores_sigterm_stays_on_the_list() {
        use std::os::unix::process::CommandExt;
        let path = temp();
        let mut child = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; sleep 60 & wait"])
            .process_group(0)
            .spawn()
            .expect("spawn sh");
        let pgid = child.id() as i32;
        // Give the shell a moment to install the trap before the reap fires.
        std::thread::sleep(std::time::Duration::from_millis(200));

        register(&path, pgid, "sh");
        assert_eq!(reap(&path), 0, "nothing was confirmed dead");
        let left = read(&path);
        assert_eq!(left.len(), 1, "the stubborn group stays written down");
        assert_eq!(left[0].pgid, pgid);

        // Teardown: SIGKILL does not take no for an answer.
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
        let _ = child.wait();
    }
}
