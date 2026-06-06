//! Supervises the bundled `gilb-analyzer run` daemon (Shannon).
//!
//! The daemon runs only while activity capture is on (subsystem A) and only when
//! the recorder is enterprise-configured (Tier-2 credentials present). A single
//! monitor task owns the child's whole lifecycle: it spawns it when capture goes
//! active, respawns it (after a backoff) if it dies unexpectedly while capture is
//! still on, and terminates it (SIGTERM, then SIGKILL) when capture stops or the
//! app exits.
//!
//! We spawn it directly (not via tauri-plugin-shell) because nothing in the
//! webview needs to launch it. The binary is resolved next to our own executable
//! — `Contents/MacOS/` in the bundle, `target/<profile>/` in dev — where the
//! externalBin sidecar sits alongside `gilb-mcp`. The analyzer itself then
//! resolves `claude` and the sibling `gilb-mcp` (see `gilb_analyzer::claude`).

use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::sync::watch;
use tracing::{info, warn};

/// Respawn delay after the daemon exits unexpectedly (e.g. config fetch hit a
/// hard 401/403) — keeps a broken state from hot-looping.
const RESPAWN_BACKOFF: Duration = Duration::from_secs(15);
/// Grace period for a SIGTERM'd daemon to finish its tick before we SIGKILL it.
const TERM_GRACE: Duration = Duration::from_secs(10);

/// Handle to the analyzer daemon's lifecycle. Cheap to clone-free share via the
/// managed `AppState`; flipping `set_active` is all the rest of the app does.
pub struct AnalyzerSupervisor {
    desired_active: watch::Sender<bool>,
}

impl AnalyzerSupervisor {
    /// Spawn the monitor task. It idles until `set_active(true)`.
    pub fn spawn() -> Self {
        let (tx, rx) = watch::channel(false);
        tauri::async_runtime::spawn(monitor(rx));
        Self { desired_active: tx }
    }

    /// Mark capture active/inactive. The monitor reacts: spawns on `true`
    /// (Tier-2 only), terminates on `false`.
    pub fn set_active(&self, active: bool) {
        // Errs only if the monitor task is gone (app shutting down) — ignore.
        let _ = self.desired_active.send(active);
    }
}

async fn monitor(mut rx: watch::Receiver<bool>) {
    loop {
        // Idle until capture is active.
        if !*rx.borrow() {
            if rx.changed().await.is_err() {
                return; // supervisor dropped
            }
            continue;
        }

        // Capture is active but we're Tier-1: nothing to run. Wait for the next
        // state change (e.g. capture toggled again after sign-in).
        if !tier2_configured() {
            info!("analyzer: no credentials (Tier-1); daemon not started");
            if rx.changed().await.is_err() {
                return;
            }
            continue;
        }

        let Some(child) = spawn_daemon() else {
            if !wait_or_changed(&mut rx, RESPAWN_BACKOFF).await {
                return;
            }
            continue;
        };

        match supervise(child, &mut rx).await {
            Outcome::Deactivated => {} // loop: will idle on the next borrow
            Outcome::SupervisorGone => return,
            Outcome::ExitedWhileActive => {
                warn!("analyzer: daemon exited while capture active; respawning after backoff");
                if !wait_or_changed(&mut rx, RESPAWN_BACKOFF).await {
                    return;
                }
            }
        }
    }
}

enum Outcome {
    /// Capture was turned off — child terminated, go idle.
    Deactivated,
    /// The daemon exited on its own while capture is still on — respawn.
    ExitedWhileActive,
    /// The supervisor handle was dropped — stop the monitor.
    SupervisorGone,
}

/// Watch one running child until it exits or capture is deactivated.
async fn supervise(mut child: Child, rx: &mut watch::Receiver<bool>) -> Outcome {
    loop {
        tokio::select! {
            status = child.wait() => {
                match status {
                    Ok(s) => info!("analyzer: daemon exited ({s})"),
                    Err(e) => warn!("analyzer: failed to wait on daemon: {e}"),
                }
                return Outcome::ExitedWhileActive;
            }
            changed = rx.changed() => {
                if changed.is_err() {
                    terminate(&mut child).await;
                    return Outcome::SupervisorGone;
                }
                if !*rx.borrow() {
                    terminate(&mut child).await;
                    return Outcome::Deactivated;
                }
                // Spurious wake (still active) — keep supervising.
            }
        }
    }
}

/// Stop the daemon: SIGTERM (so its `run` loop exits cleanly and kills its own
/// `claude` child), then SIGKILL if it overstays the grace period.
async fn terminate(child: &mut Child) {
    let Some(pid) = child.id() else {
        return; // already reaped
    };
    #[cfg(unix)]
    // SAFETY: plain libc::kill on a pid we own; SIGTERM is the graceful stop.
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
    }

    match tokio::time::timeout(TERM_GRACE, child.wait()).await {
        Ok(_) => info!("analyzer: daemon stopped"),
        Err(_) => {
            warn!("analyzer: daemon did not stop within grace; killing");
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

/// Spawn `gilb-analyzer run` from the sidecar next to our own executable.
fn spawn_daemon() -> Option<Child> {
    let exe = std::env::current_exe().ok()?;
    let bin = exe.parent()?.join("gilb-analyzer");
    if !bin.is_file() {
        warn!("analyzer: sidecar not found at {}", bin.display());
        return None;
    }
    let mut cmd = Command::new(&bin);
    cmd.arg("run");
    // Reaped by `terminate`; kill_on_drop is a backstop if the task unwinds.
    cmd.kill_on_drop(true);
    match cmd.spawn() {
        Ok(c) => {
            info!("analyzer: started daemon ({})", bin.display());
            Some(c)
        }
        Err(e) => {
            warn!("analyzer: failed to spawn daemon: {e}");
            None
        }
    }
}

fn tier2_configured() -> bool {
    matches!(gilb_config::load_credentials(), Ok(Some(_)))
}

/// Sleep for `dur`, or return early if `desired_active` changes. Returns `false`
/// only if the supervisor handle was dropped (caller should stop the monitor).
async fn wait_or_changed(rx: &mut watch::Receiver<bool>, dur: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(dur) => true,
        changed = rx.changed() => changed.is_ok(),
    }
}
