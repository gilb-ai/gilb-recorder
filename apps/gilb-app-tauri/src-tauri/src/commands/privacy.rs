//! macOS Privacy & Security pane shortcuts.
//!
//! Asks macOS for the Accessibility TCC permission (registering Gilb in
//! System Settings so it shows up with a toggle) and opens the pane so
//! the user can flip it. The native consent prompt, if shown, layers on
//! top of the settings UI.
//!
//! Input Monitoring is intentionally not exposed: CGEventTap honours the
//! Accessibility grant, so we don't ask for two permissions when one
//! covers the recorder's needs.

#[tauri::command]
pub async fn open_privacy_pane(app: tauri::AppHandle, pane: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use gilb_a11y::platform::macos::permissions;
        use tauri_plugin_opener::OpenerExt;

        let url = match pane.as_str() {
            "accessibility" => {
                let _ = permissions::request_accessibility();
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
            other => return Err(format!("unknown privacy pane: {other}")),
        };
        app.opener()
            .open_url(url, None::<&str>)
            .map_err(|e| format!("failed to open privacy pane: {e}"))
    }
    #[cfg(target_os = "windows")]
    {
        // Windows does not gate low-level hooks or UI Automation behind a
        // user-granted permission, so there is no pane to open — capture is
        // always available. Treat this as a no-op success.
        let _ = (app, pane);
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (app, pane);
        Err("open_privacy_pane is not supported on this platform".to_string())
    }
}
