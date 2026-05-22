//! macOS Privacy & Security pane shortcuts.
//!
//! Currently shells out via `open x-apple.systempreferences:…`. [B5] will
//! replace this with `tauri-plugin-shell` + URL allowlist.

/// Открывает соответствующий раздел System Settings → Privacy & Security.
#[tauri::command]
pub async fn open_privacy_pane(pane: String) -> Result<(), String> {
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
