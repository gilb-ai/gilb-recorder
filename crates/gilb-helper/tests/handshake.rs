//! End-to-end handshake test: spawn the helper binary, send a `Ping`
//! frame over the Unix socket, and assert that `Pong` comes back within
//! two seconds.

#[path = "../src/protocol.rs"]
mod protocol;

use std::time::Duration;

use anyhow::{anyhow, Result};
use interprocess::local_socket::{
    tokio::{prelude::*, Stream},
    GenericFilePath, ToFsName,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use crate::protocol::{Request, Response};

const HANDSHAKE_BUDGET: Duration = Duration::from_secs(2);

#[tokio::test]
async fn ping_pong_handshake() -> Result<()> {
    let socket_path = std::env::temp_dir().join(format!(
        "gilb-helper-test-{}-{}.sock",
        std::process::id(),
        chrono_like_nanos()
    ));
    let _ = std::fs::remove_file(&socket_path);

    let bin = env!("CARGO_BIN_EXE_gilb-helper");
    let mut child = tokio::process::Command::new(bin)
        .env("GILB_HELPER_SOCKET", &socket_path)
        .env("RUST_LOG", "info")
        .kill_on_drop(true)
        .spawn()?;

    let connect_path = socket_path.clone();
    let connect = async move {
        loop {
            if connect_path.exists() {
                let name = connect_path.as_path().to_fs_name::<GenericFilePath>()?;
                if let Ok(stream) = Stream::connect(name).await {
                    return Ok::<Stream, anyhow::Error>(stream);
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };

    let mut stream = timeout(HANDSHAKE_BUDGET, connect)
        .await
        .map_err(|_| anyhow!("helper socket never became reachable"))??;

    let request = rmp_serde::to_vec(&Request::Ping)?;
    let req_len: u32 = request.len() as u32;
    stream.write_all(&req_len.to_be_bytes()).await?;
    stream.write_all(&request).await?;
    stream.flush().await?;

    let mut len_buf = [0u8; 4];
    timeout(HANDSHAKE_BUDGET, stream.read_exact(&mut len_buf))
        .await
        .map_err(|_| anyhow!("no response length within 2s"))??;
    let resp_len = u32::from_be_bytes(len_buf) as usize;

    let mut body = vec![0u8; resp_len];
    timeout(HANDSHAKE_BUDGET, stream.read_exact(&mut body))
        .await
        .map_err(|_| anyhow!("no response body within 2s"))??;

    let response: Response = rmp_serde::from_slice(&body)?;
    assert_eq!(response, Response::Pong);

    let _ = child.start_kill();
    let _ = child.wait().await;
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

fn chrono_like_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
