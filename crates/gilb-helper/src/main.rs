//! gilb-helper daemon entry point.
//!
//! Skeleton implementation per `research/08-helper-daemon.md`: a user-process
//! tokio binary that listens on an `interprocess` local socket and answers
//! a single `Ping` request with `Pong`. Full request set (`StartCapture`,
//! `StopCapture`, `Status`), SMAppService registration, and the SQLite
//! writer move land in follow-up cards ([GILB-19] / [GILB-20] / [GILB-21]).

mod protocol;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use interprocess::local_socket::{
    tokio::{prelude::*, Stream},
    GenericFilePath, ListenerOptions, ToFsName,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing_subscriber::EnvFilter;

use crate::protocol::{Request, Response};

const SOCKET_FILE_NAME: &str = "helper.sock";
const ALIVE_TICK: Duration = Duration::from_secs(1);
const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// Resolve the socket path. The `GILB_HELPER_SOCKET` override exists so
/// integration tests can isolate themselves from a real installation.
fn socket_path() -> Result<PathBuf> {
    if let Ok(custom) = std::env::var("GILB_HELPER_SOCKET") {
        return Ok(PathBuf::from(custom));
    }
    let dir = gilb_config::ensure_data_dir()?;
    Ok(dir.join(SOCKET_FILE_NAME))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let path = socket_path()?;
    // A previous run may have left the socket file behind on Unix.
    let _ = std::fs::remove_file(&path);

    let name = path
        .as_path()
        .to_fs_name::<GenericFilePath>()
        .with_context(|| format!("invalid socket path: {}", path.display()))?;
    let listener = ListenerOptions::new()
        .name(name)
        .create_tokio()
        .with_context(|| format!("failed to bind helper socket at {}", path.display()))?;

    tracing::info!(socket = %path.display(), "gilb-helper listening");

    tokio::spawn(alive_loop());

    loop {
        match listener.accept().await {
            Ok(stream) => {
                tokio::spawn(async move {
                    if let Err(err) = handle_connection(stream).await {
                        tracing::warn!(error = %err, "client connection ended with error");
                    }
                });
            }
            Err(err) => {
                tracing::warn!(error = %err, "accept failed");
            }
        }
    }
}

async fn alive_loop() {
    let mut ticker = tokio::time::interval(ALIVE_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        tracing::info!("alive");
    }
}

async fn handle_connection(mut stream: Stream) -> Result<()> {
    loop {
        let mut len_buf = [0u8; 4];
        match stream.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err.into()),
        }
        let payload_len = u32::from_be_bytes(len_buf);
        if payload_len > MAX_FRAME_BYTES {
            anyhow::bail!("frame too large: {payload_len} bytes");
        }
        let mut payload = vec![0u8; payload_len as usize];
        stream.read_exact(&mut payload).await?;

        let request: Request =
            rmp_serde::from_slice(&payload).context("failed to decode request frame")?;
        let response = match request {
            Request::Ping => Response::Pong,
        };

        let body = rmp_serde::to_vec(&response).context("failed to encode response frame")?;
        let body_len: u32 = body
            .len()
            .try_into()
            .context("response payload exceeds u32::MAX")?;
        stream.write_all(&body_len.to_be_bytes()).await?;
        stream.write_all(&body).await?;
        stream.flush().await?;
    }
}
