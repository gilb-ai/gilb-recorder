//! macOS Privacy & Security pane shortcuts.
//!
//! For each requested pane we (a) ask macOS for the corresponding TCC
//! permission — this registers gilb in System Settings so it shows up in
//! the list and the user has a toggle to flip — and (b) open the pane
//! itself. The native consent prompt (if any) layers on top of the
//! settings UI; this is the documented `request_*` + URL-scheme pairing
//! for the modern TCC APIs.

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
            "input_monitoring" => {
                let _ = permissions::request_input_monitoring();
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
