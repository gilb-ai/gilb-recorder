//! Tracing setup for the Tauri shell.
//!
//! Only initialised in dev builds (`cfg(dev)`, set by `tauri-build` during
//! `tauri dev`). In release builds (`tauri build`) we deliberately skip
//! tracing init entirely — no stdout, no file in `~/.gilb/logs/`.

use tracing_appender::non_blocking::WorkerGuard;

#[cfg(dev)]
use gilb_config::ensure_logs_dir;
#[cfg(dev)]
use tracing::info;
#[cfg(dev)]
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Initialise `tracing` for the Tauri shell.
///
/// Dev builds (`cfg(dev)`): writes to both stdout and a daily-rotated file
/// in `$HOME/.gilb/logs/`. The returned [`WorkerGuard`] **must be held** for
/// the lifetime of the process — dropping it flushes the file writer. If the
/// logs directory cannot be created we fall back to stdout-only and return
/// `None`.
///
/// Release builds: no subscriber is installed and `None` is returned.
#[cfg(not(dev))]
pub fn init_tracing() -> Option<WorkerGuard> {
    None
}

#[cfg(dev)]
pub fn init_tracing() -> Option<WorkerGuard> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,gilb=debug"));

    let stdout_layer = fmt::layer().with_target(true);

    match ensure_logs_dir() {
        Ok(dir) => {
            let appender = tracing_appender::rolling::daily(&dir, "gilb.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let file_layer = fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_thread_names(true)
                .with_writer(writer);

            let _ = tracing_subscriber::registry()
                .with(env_filter)
                .with(stdout_layer)
                .with(file_layer)
                .try_init();
            info!(logs_dir = %dir.display(), "file appender attached");
            Some(guard)
        }
        Err(err) => {
            let _ = tracing_subscriber::registry()
                .with(env_filter)
                .with(stdout_layer)
                .try_init();
            tracing::warn!(?err, "could not create logs dir, file logging disabled");
            None
        }
    }
}
