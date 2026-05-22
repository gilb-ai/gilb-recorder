//! gilb Tauri shell — wires the UI to `gilb-engine`.

use std::sync::Arc;

use anyhow::anyhow;
use gilb_config::{ensure_data_dir, db_path, RecordingSettings};
use gilb_engine::{Engine, EngineStatus};
use tauri::Manager;
use tracing::info;

struct AppState {
    engine: Arc<Engine>,
}

#[tauri::command]
async fn start_capture(state: tauri::State<'_, AppState>) -> Result<i64, String> {
    let settings = RecordingSettings::from_env();
    state
        .engine
        .start_capture(settings)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn stop_capture(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state
        .engine
        .stop_capture("user-stop")
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn status(state: tauri::State<'_, AppState>) -> Result<EngineStatus, String> {
    state.engine.status().await.map_err(|e| e.to_string())
}

/// Открывает соответствующий раздел System Settings → Privacy & Security.
/// На macOS используется `open x-apple.systempreferences:…` через
/// `std::process::Command`, потому что кастомные URL-схемы не входят в
/// default-разрешения opener-плагина.
#[tauri::command]
async fn open_privacy_pane(pane: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let url = match pane.as_str() {
            "accessibility" => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
            "input_monitoring" => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
            }
            other => return Err(format!("unknown privacy pane: {other}")),
        };
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|e| format!("failed to spawn `open`: {e}"))?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pane;
        Ok(())
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,gilb=debug")),
        )
        .try_init();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_tracing();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::block_on(async move {
                ensure_data_dir().map_err(|e| anyhow!("ensure_data_dir: {e}"))?;
                let path = db_path().map_err(|e| anyhow!("db_path: {e}"))?;
                info!(?path, "gilb-app: opening engine");
                let engine = Engine::open(path).await?;
                handle.manage(AppState {
                    engine: Arc::new(engine),
                });
                Ok::<_, anyhow::Error>(())
            })?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_capture,
            stop_capture,
            status,
            open_privacy_pane
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
