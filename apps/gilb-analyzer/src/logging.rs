//! Tracing setup for the analyzer.
//!
//! Logs go to **both** stderr (handy when an operator runs `find`/`backfill`
//! by hand) and a daily-rotated file in the data folder's `logs/` so the long-lived
//! `run` daemon leaves an auditable trail on disk. The file mirrors the Tauri
//! shell's appender (`gilb_config::ensure_logs_dir`), but under its own
//! `analyzer.log` prefix.

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use gilb_config::ensure_logs_dir;

/// Initialise `tracing`: stderr plus a daily-rotated `analyzer.log` in
/// the data folder's `logs/`. The returned [`WorkerGuard`] **must be held** for the
/// lifetime of the process — dropping it flushes the file writer. If the logs
/// directory cannot be created we fall back to stderr-only and return `None`.
pub fn init_tracing() -> Option<WorkerGuard> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let stderr_layer = fmt::layer().with_writer(std::io::stderr);

    match ensure_logs_dir() {
        Ok(dir) => {
            let appender = tracing_appender::rolling::daily(&dir, "analyzer.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let file_layer = fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_thread_names(true)
                .with_writer(writer);

            let _ = tracing_subscriber::registry()
                .with(env_filter)
                .with(stderr_layer)
                .with(file_layer)
                .try_init();
            tracing::info!(logs_dir = %dir.display(), "file appender attached");
            Some(guard)
        }
        Err(err) => {
            let _ = tracing_subscriber::registry()
                .with(env_filter)
                .with(stderr_layer)
                .try_init();
            tracing::warn!(?err, "could not create logs dir, file logging disabled");
            None
        }
    }
}
