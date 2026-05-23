//! macOS Privacy & Security pane shortcuts.
//!
//! Opens `x-apple.systempreferences:…` via the Rust side of
//! `tauri-plugin-opener`. The Rust API doesn't require an IPC capability
//! (capabilities are only enforced for JS-side opener calls), but it does
//! give us a proper exit-code check and cross-platform dispatch through the
//! OS APIs instead of a hand-rolled `Command::new("open")`.

#[tauri::command]
pub async fn open_privacy_pane(app: tauri::AppHandle, pane: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use tauri_plugin_opener::OpenerExt;
        let url = match pane.as_str() {
            "accessibility" => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
            "input_monitoring" => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
            }
            other => return Err(format!("unknown privacy pane: {other}")),
        };
        app.opener()
            .open_url(url, None::<&str>)
            .map_err(|e| format!("failed to open privacy pane: {e}"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, pane);
        Err("open_privacy_pane is only supported on macOS".to_string())
    }
}
