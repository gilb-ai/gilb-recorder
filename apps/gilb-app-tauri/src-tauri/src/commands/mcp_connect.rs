//! Commands that wire the bundled `gilb-mcp` binary into common MCP clients
//! (Claude Code, Claude Desktop, Cursor) — see also [`MCP-CONNECT`] in the
//! project task list.
//!
//! The frontend renders a modal listing the four targets below; each target's
//! action is one `mcp_connect` invocation.

use std::path::PathBuf;

use base64::Engine;
use serde::Serialize;

const SERVER_NAME: &str = "gilb";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ServerInfo {
    /// Absolute path to the `gilb-mcp` binary, if it could be located.
    pub binary_path: Option<String>,
    /// JSON snippet for manual install / `Other` tab — already pretty-printed.
    pub config_json: String,
    /// Per-known-client status / hints (e.g. "claude CLI not found").
    pub clients: Vec<ClientStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClientStatus {
    pub id: String,
    pub label: String,
    pub available: bool,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConnectOutcome {
    Installed { details: String },
    OpenedDeeplink { url: String },
    NeedsManual { hint: String },
}

#[tauri::command]
pub async fn mcp_server_info() -> Result<ServerInfo, String> {
    let binary_path = locate_mcp_binary().map(|p| p.to_string_lossy().into_owned());
    let config_json = build_config_json(binary_path.as_deref().unwrap_or("/path/to/gilb-mcp"));
    let clients = vec![
        probe_claude_code(),
        probe_claude_desktop(),
        probe_cursor(),
    ];
    Ok(ServerInfo {
        binary_path,
        config_json,
        clients,
    })
}

#[tauri::command]
pub async fn mcp_connect(
    app: tauri::AppHandle,
    client: String,
) -> Result<ConnectOutcome, String> {
    let binary = locate_mcp_binary()
        .ok_or_else(|| "gilb-mcp binary not found — build it with `cargo build --release -p gilb-mcp`".to_string())?;
    let binary_str = binary.to_string_lossy().into_owned();
    match client.as_str() {
        "claude_code" => connect_claude_code(&binary_str),
        "claude_desktop" => connect_claude_desktop(&binary_str),
        "cursor" => connect_cursor(&app, &binary_str),
        other => Err(format!("unknown client: {other}")),
    }
}

// ---- Binary path resolution -------------------------------------------

fn locate_mcp_binary() -> Option<PathBuf> {
    // 1. Sidecar / sibling of the current executable.
    if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent() {
            let cand = parent.join("gilb-mcp");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    // 2. Workspace `target/release/gilb-mcp` and `target/debug/gilb-mcp`,
    //    found by walking up from CWD looking for `Cargo.toml`.
    if let Some(target_dir) = find_workspace_target_dir() {
        for profile in ["release", "debug"] {
            let cand = target_dir.join(profile).join("gilb-mcp");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

fn find_workspace_target_dir() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    for _ in 0..6 {
        if dir.join("Cargo.toml").is_file() && dir.join("target").is_dir() {
            return Some(dir.join("target"));
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

// ---- Per-client probes ------------------------------------------------

fn probe_claude_code() -> ClientStatus {
    let available = which("claude").is_some();
    ClientStatus {
        id: "claude_code".into(),
        label: "Claude Code".into(),
        available,
        hint: Some(
            if available {
                "Runs `claude mcp add`."
            } else {
                "`claude` CLI not in PATH."
            }
            .into(),
        ),
    }
}

fn probe_claude_desktop() -> ClientStatus {
    let path = claude_desktop_config_path();
    let available = path.as_ref().map(|p| p.parent().map(|d| d.exists()).unwrap_or(false)).unwrap_or(false);
    ClientStatus {
        id: "claude_desktop".into(),
        label: "Claude Desktop".into(),
        available,
        hint: Some("Edits `claude_desktop_config.json`.".into()),
    }
}

fn probe_cursor() -> ClientStatus {
    // We can always try to open the deeplink; Cursor handles it if installed.
    ClientStatus {
        id: "cursor".into(),
        label: "Cursor".into(),
        available: true,
        hint: Some("Opens an install deeplink.".into()),
    }
}

// ---- Per-client installers --------------------------------------------

fn connect_claude_code(binary: &str) -> Result<ConnectOutcome, String> {
    if which("claude").is_none() {
        return Ok(ConnectOutcome::NeedsManual {
            hint: "`claude` CLI not found in PATH. Install Claude Code, or copy the JSON config from the «Other» tab.".into(),
        });
    }
    let output = std::process::Command::new("claude")
        .args(["mcp", "add", "-s", "user", SERVER_NAME, binary])
        .output()
        .map_err(|e| format!("failed to spawn `claude`: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if output.status.success() {
        Ok(ConnectOutcome::Installed {
            details: stdout.trim().to_string(),
        })
    } else if stderr.to_lowercase().contains("already") || stdout.to_lowercase().contains("already") {
        // Treat re-install as success — `claude mcp add` is idempotent for our
        // purposes; the user just clicked twice.
        Ok(ConnectOutcome::Installed {
            details: format!("Already registered.\n{}", stderr.trim()),
        })
    } else {
        Err(format!("claude mcp add failed: {}", stderr.trim()))
    }
}

fn connect_claude_desktop(binary: &str) -> Result<ConnectOutcome, String> {
    let path = claude_desktop_config_path()
        .ok_or_else(|| "could not resolve Claude Desktop config path".to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create config dir {}: {e}", parent.display()))?;
    }

    let mut root: serde_json::Value = if path.exists() {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if text.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&text)
                .map_err(|e| format!("parse {}: {e}", path.display()))?
        }
    } else {
        serde_json::json!({})
    };

    let servers = root
        .as_object_mut()
        .ok_or_else(|| "config root is not a JSON object".to_string())?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| "mcpServers is not an object".to_string())?;
    servers.insert(
        SERVER_NAME.into(),
        serde_json::json!({
            "command": binary,
            "args": [],
        }),
    );

    let pretty = serde_json::to_string_pretty(&root)
        .map_err(|e| format!("serialize config: {e}"))?;
    std::fs::write(&path, pretty)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    tracing::info!(config = %path.display(), "claude-desktop config updated");

    Ok(ConnectOutcome::Installed {
        details: "Restart Claude Desktop to apply.".into(),
    })
}

fn connect_cursor(app: &tauri::AppHandle, binary: &str) -> Result<ConnectOutcome, String> {
    use tauri_plugin_opener::OpenerExt;
    let config = serde_json::json!({
        "command": binary,
        "args": [],
    });
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(config.to_string());
    let url = format!(
        "cursor://anysphere.cursor-deeplink/mcp/install?name={SERVER_NAME}&config={payload}"
    );
    app.opener()
        .open_url(&url, None::<&str>)
        .map_err(|e| format!("open cursor deeplink: {e}"))?;
    Ok(ConnectOutcome::OpenedDeeplink { url })
}

// ---- Helpers ----------------------------------------------------------

fn build_config_json(binary: &str) -> String {
    let value = serde_json::json!({
        "mcpServers": {
            SERVER_NAME: {
                "command": binary,
                "args": [],
            }
        }
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into())
}

fn claude_desktop_config_path() -> Option<PathBuf> {
    let base = directories::BaseDirs::new()?;
    #[cfg(target_os = "macos")]
    let p = base
        .home_dir()
        .join("Library/Application Support/Claude/claude_desktop_config.json");
    #[cfg(target_os = "windows")]
    let p = base.config_dir().join("Claude/claude_desktop_config.json");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let p = base.config_dir().join("Claude/claude_desktop_config.json");
    Some(p)
}

/// Minimal `which` — checks `$PATH` for an executable named `name`. Returns
/// the resolved path, or `None`. We do not depend on the `which` crate to
/// keep the surface small.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}
