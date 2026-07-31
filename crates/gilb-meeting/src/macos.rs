//! macOS meeting detection via the unified log.
//!
//! Control Center (`com.apple.controlcenter`) renders the orange
//! "microphone in use" indicator and logs a `sensor-indicators` line
//! whenever the set of active attributions changes, e.g.
//!
//! ```text
//! Active activity attributions changed to ["mic:us.zoom.xos", "camera:com.apple.FaceTime"]
//! ```
//!
//! [`MacosDetector`] streams those lines from `/usr/bin/log`, keeps the
//! mic attributions for allowlisted apps, debounces rapid changes, and
//! emits [`MeetingEvent`]s on count transitions. The parsing
//! ([`parse_attribution_line`]) and the count state machine ([`Tracker`])
//! are pure and unit-tested without the subprocess.
//!
//! The predicate, the debounce window and the allowlist are carried over
//! unchanged from an earlier Electron implementation — they are the part
//! that took real meetings to get right.

use chrono::{DateTime, Utc};

use crate::{MeetingApp, MeetingEvent};

/// Prefix every attribution-change `eventMessage` starts with; the JSON
/// array of attributions follows it.
const EVENT_MESSAGE_PREFIX: &str = "Active activity attributions changed to ";

/// Parse one NDJSON `log stream` line into the mic bundle IDs it reports.
///
/// Returns `None` when the line is not an attribution-change event (log
/// header lines, non-JSON, unrelated messages). Returns `Some(ids)` —
/// possibly empty — when the line *is* an attribution change; an empty
/// vec means "no app is using the mic", i.e. a meeting ended. The IDs are
/// the raw `mic:` attributions with the prefix stripped; allowlist
/// filtering is applied separately by the detector.
pub fn parse_attribution_line(line: &str) -> Option<Vec<String>> {
    let event: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let message = event.get("eventMessage")?.as_str()?;
    let rest = message.strip_prefix(EVENT_MESSAGE_PREFIX)?;

    // The prefix matched, so this is an attribution-change line. A
    // missing or malformed payload is treated as "no mic apps" — the
    // change still gets reported, with an empty set.
    let mics = serde_json::from_str::<Vec<serde_json::Value>>(rest)
        .ok()
        .map(|attrs| {
            attrs
                .iter()
                .filter_map(|a| a.as_str())
                .filter_map(|a| a.strip_prefix("mic:"))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Some(mics)
}

/// Count state machine mapping a debounced mic-app set to a [`MeetingEvent`].
///
/// Feed it the (allowlist-filtered) set of apps currently using the mic
/// after each debounce flush. It emits `Started` on the first transition
/// from no apps to some, `Ended` when the last app drops off, and
/// `AppsChanged` when the set changes while still non-empty. Identical
/// observations produce no event. The set is order-independent.
#[derive(Debug, Default)]
pub struct Tracker {
    current: Vec<MeetingApp>,
    started_at: Option<DateTime<Utc>>,
}

impl Tracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe the current mic-app set at time `now`; see [`Tracker`].
    pub fn observe(&mut self, apps: Vec<MeetingApp>, now: DateTime<Utc>) -> Option<MeetingEvent> {
        let mut apps = apps;
        apps.sort_by(|a, b| a.bundle_id.cmp(&b.bundle_id));
        apps.dedup_by(|a, b| a.bundle_id == b.bundle_id);

        if apps == self.current {
            return None;
        }

        let was_active = !self.current.is_empty();
        let now_active = !apps.is_empty();
        self.current = apps.clone();

        match (was_active, now_active) {
            (false, true) => {
                self.started_at = Some(now);
                Some(MeetingEvent::Started { at: now, apps })
            }
            (true, true) => Some(MeetingEvent::AppsChanged { at: now, apps }),
            (true, false) => {
                let started = self.started_at.take().unwrap_or(now);
                let duration = (now - started).to_std().unwrap_or_default();
                Some(MeetingEvent::Ended { at: now, duration })
            }
            // Both empty is impossible here: `apps == self.current` above
            // already short-circuits the empty-to-empty case.
            (false, false) => None,
        }
    }
}

#[cfg(target_os = "macos")]
mod detector {
    use std::process::Stdio;
    use std::time::Duration;

    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use chrono::Utc;
    use tokio::io::AsyncBufReadExt;
    use tokio::process::Command;
    use tokio::sync::{mpsc, oneshot, Mutex};
    use tokio::task::JoinHandle;
    use tokio::time::Instant;

    use super::{parse_attribution_line, Tracker};
    use crate::{allowlist, MeetingApp, MeetingDetector, MeetingEvent, EVENT_CHANNEL_CAPACITY};

    /// Coalesce changes arriving within this window into one update.
    const DEBOUNCE_MS: u64 = 2000;

    /// The `log stream` predicate, verbatim (a private Control Center
    /// interface; may break across macOS releases).
    const LOG_PREDICATE: &str = "subsystem == \"com.apple.controlcenter\" \
        AND category == \"sensor-indicators\" \
        AND eventMessage BEGINSWITH \"Active activity attributions changed to \"";

    struct Running {
        shutdown: oneshot::Sender<()>,
        task: JoinHandle<()>,
    }

    /// Meeting detector backed by the macOS unified log; see the module docs.
    #[derive(Default)]
    pub struct MacosDetector {
        running: Mutex<Option<Running>>,
    }

    impl MacosDetector {
        pub fn new() -> Self {
            Self::default()
        }
    }

    #[async_trait]
    impl MeetingDetector for MacosDetector {
        async fn start(&self) -> Result<mpsc::Receiver<MeetingEvent>> {
            let mut guard = self.running.lock().await;
            if guard.is_some() {
                return Err(anyhow!("MacosDetector already started"));
            }
            let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            let task = tokio::spawn(run(tx, shutdown_rx));
            *guard = Some(Running {
                shutdown: shutdown_tx,
                task,
            });
            Ok(rx)
        }

        async fn stop(&self) -> Result<()> {
            let running = self.running.lock().await.take();
            if let Some(Running { shutdown, task }) = running {
                let _ = shutdown.send(());
                let _ = task.await;
            }
            Ok(())
        }
    }

    async fn run(tx: mpsc::Sender<MeetingEvent>, mut shutdown: oneshot::Receiver<()>) {
        let mut child = match Command::new("/usr/bin/log")
            .args([
                "stream",
                "--type",
                "log",
                "--level",
                "default",
                "--predicate",
                LOG_PREDICATE,
                "--style",
                "ndjson",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                let _ = tx
                    .send(MeetingEvent::HealthDegraded {
                        reason: format!("failed to spawn log stream: {e}"),
                    })
                    .await;
                return;
            }
        };

        let stdout = child.stdout.take().expect("log stream stdout is piped");
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let mut tracker = Tracker::new();
        let mut pending: Option<Vec<MeetingApp>> = None;
        let mut deadline: Option<Instant> = None;

        loop {
            // Recreated each iteration so a new line resets the window.
            let debounce = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending::<()>().await,
                }
            };

            tokio::select! {
                _ = &mut shutdown => break,
                line = lines.next_line() => match line {
                    Ok(Some(line)) => {
                        if let Some(mic_apps) = parse_attribution_line(&line) {
                            let apps = mic_apps
                                .into_iter()
                                .filter(|b| allowlist::is_allowlisted(b))
                                .map(|b| {
                                    let display_name = allowlist::display_name(&b)
                                        .unwrap_or(b.as_str())
                                        .to_string();
                                    MeetingApp { bundle_id: b, display_name }
                                })
                                .collect();
                            pending = Some(apps);
                            deadline = Some(Instant::now() + Duration::from_millis(DEBOUNCE_MS));
                        }
                    }
                    Ok(None) => {
                        let _ = tx
                            .send(MeetingEvent::HealthDegraded {
                                reason: "log stream ended unexpectedly".to_string(),
                            })
                            .await;
                        break;
                    }
                    Err(e) => {
                        let _ = tx
                            .send(MeetingEvent::HealthDegraded {
                                reason: format!("log stream read error: {e}"),
                            })
                            .await;
                        break;
                    }
                },
                _ = debounce, if deadline.is_some() => {
                    deadline = None;
                    if let Some(apps) = pending.take() {
                        if let Some(event) = tracker.observe(apps, Utc::now()) {
                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        }

        let _ = child.start_kill();
    }
}

#[cfg(target_os = "macos")]
pub use detector::MacosDetector;
