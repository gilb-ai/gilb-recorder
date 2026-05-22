//! macOS Privacy & Security pane shortcuts.
//!
//! Opens `x-apple.systempreferences:…` через Rust-сторону `tauri-plugin-opener`
//! — это не требует разрешений в capabilities (они нужны только для прямого
//! вызова opener из JS), но даёт нам нормальный exit-code check и
//! кросс-платформенную диспетчеризацию через системные APIs вместо ручного
//! `Command::new("open")`.

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
