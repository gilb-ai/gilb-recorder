//! Tracing setup for the Tauri shell.
//!
//! Both dev and release builds write a daily-rotated file to the data folder's `logs/`;
//! dev additionally logs to stdout and at a chattier level.
//!
//! Release builds used to install no subscriber at all, which meant that when a
//! recording failed in the field every `warn!` explaining why went nowhere: the
//! app has no console, so the only evidence left was whatever macOS happened to
//! keep in its own unified log. Diagnosing a capture that silently produced half
//! a meeting was then guesswork. The recorder's own logs are the cheapest
//! possible answer to "why is this recording short", so release keeps them —
//! at `info`, capped to a few days of files.

use tracing_appender::non_blocking::WorkerGuard;

use gilb_config::ensure_logs_dir;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Log files kept by the rotation, i.e. roughly this many days of history.
/// Enough to look into an incident reported a couple of days late without
/// letting the directory grow without bound.
const MAX_LOG_FILES: usize = 5;

/// Default filter when `RUST_LOG` says nothing.
///
/// Release stays at `info`: enough to see capture start/stop, stream re-targets
/// and every `warn!`, without the per-tick `trace!` detail. Dev turns `gilb=debug`
/// on.
#[cfg(dev)]
const DEFAULT_FILTER: &str = "info,gilb=debug";
#[cfg(not(dev))]
const DEFAULT_FILTER: &str = "info";

/// Initialise `tracing` for the Tauri shell.
///
/// Writes a daily-rotated file in the data directory's `logs/`, plus stdout in
/// dev builds.
/// The returned [`WorkerGuard`] **must be held** for the lifetime of the process
/// — dropping it flushes the file writer. Returns `None` if the logs directory
/// cannot be created (dev then falls back to stdout-only; release has nowhere
/// left to write and installs no subscriber).
pub fn init_tracing() -> Option<WorkerGuard> {
    match ensure_logs_dir() {
        Ok(dir) => init_with_logs_dir(&dir),
        Err(err) => {
            #[cfg(dev)]
            {
                let _ = tracing_subscriber::registry()
                    .with(EnvFilter::new(DEFAULT_FILTER))
                    .with(fmt::layer().with_target(true))
                    .try_init();
                tracing::warn!(?err, "could not create logs dir, file logging disabled");
            }
            #[cfg(not(dev))]
            let _ = err;
            None
        }
    }
}

/// [`init_tracing`] with the log directory supplied, so it can be exercised
/// against a temporary directory instead of the user's real logs folder.
fn init_with_logs_dir(dir: &std::path::Path) -> Option<WorkerGuard> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    let appender = match tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("gilb")
        .filename_suffix("log")
        .max_log_files(MAX_LOG_FILES)
        .build(dir)
    {
        Ok(appender) => appender,
        // Fall back to an uncapped daily file rather than losing logs entirely.
        Err(_) => tracing_appender::rolling::daily(dir, "gilb.log"),
    };
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_thread_names(true)
        .with_writer(writer);

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer);

    #[cfg(dev)]
    let _ = registry.with(fmt::layer().with_target(true)).try_init();
    #[cfg(not(dev))]
    let _ = registry.try_init();

    tracing::info!(logs_dir = %dir.display(), "file appender attached");
    Some(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Release builds install no stdout writer, so this file is the only record
    /// of why a recording went wrong in the field — assert it actually appears
    /// and receives events, rather than trusting the appender wiring.
    #[test]
    fn attaches_a_file_appender_that_receives_events() {
        let dir = std::env::temp_dir().join(format!("gilb-logging-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp logs dir");

        let guard = init_with_logs_dir(&dir).expect("appender attached");
        tracing::info!(marker = "canary", "smoke event");
        // Dropping the guard flushes the non-blocking writer.
        drop(guard);

        let written: Vec<String> = std::fs::read_dir(&dir)
            .expect("read logs dir")
            .filter_map(|e| e.ok())
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .collect();
        assert!(
            written.iter().any(|body| body.contains("canary")),
            "no log file carried the event: {written:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
