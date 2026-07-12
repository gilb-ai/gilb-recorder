//! Recorder ↔ gilb-web authentication.
//!
//! Flow (see the project plan): `start_login` opens the gilb-web consent page
//! in the system browser with a one-shot `state` we keep in `AppState`. After
//! the employee signs in and confirms, gilb-web 302s back to
//! `gilb://auth/callback?token=…&url=…&employee=…&state=…`; the OS hands that
//! deep link to [`handle_callback`], which verifies `state`, writes
//! `~/.gilb/credentials.json`, and emits `auth` so the UI refreshes.
//!
//! The token arrives in the deep-link URL, so no HTTP client is needed here.

use gilb_config::{clear_credentials, load_credentials, save_credentials, Credentials};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_opener::OpenerExt;
use tracing::{info, warn};

use crate::state::{AppState, PendingAuth};

/// The fixed deep-link target gilb-web redirects the token to. Must match the
/// scheme registered in `tauri.conf.json` and the allowlist in gilb-web's
/// `RecorderAuthController`.
const REDIRECT_URI: &str = "gilb://auth/callback";

/// Auth state surfaced to the UI.
#[derive(Debug, Clone, Serialize)]
pub struct AuthStatus {
    pub signed_in: bool,
    pub employee: Option<String>,
    pub gilb_web_url: Option<String>,
}

/// Begin sign-in: stash a fresh `state`, then open the gilb-web consent page in
/// the system browser. The reply comes back asynchronously via [`handle_callback`].
#[tauri::command]
pub async fn start_login(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    gilb_web_url: String,
) -> Result<(), String> {
    // The workspace URL is baked into the build (the UI shows only a Connect
    // button), but a `GILB_WEB_URL` in the app's environment overrides it — a
    // hidden escape hatch for QA / staging without a separate build.
    let requested = std::env::var("GILB_WEB_URL").unwrap_or(gilb_web_url);
    let base = requested.trim().trim_end_matches('/');
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err("Enter a full gilb-web URL (https://…)".to_string());
    }

    let csrf = uuid::Uuid::new_v4().to_string();
    let url = format!(
        "{base}/recorder/authorize?redirect_uri={redirect}&state={csrf}",
        redirect = encode(REDIRECT_URI),
    );

    *state.pending_auth.lock() = Some(PendingAuth {
        state: csrf,
        gilb_web_url: base.to_string(),
    });

    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| format!("failed to open browser: {e}"))
}

/// Current credentials state (read from `~/.gilb/credentials.json`).
#[tauri::command]
pub async fn auth_status() -> Result<AuthStatus, String> {
    match load_credentials().map_err(|e| e.to_string())? {
        Some(c) => Ok(AuthStatus {
            signed_in: true,
            employee: c.employee,
            gilb_web_url: Some(c.gilb_web_url),
        }),
        None => Ok(AuthStatus {
            signed_in: false,
            employee: None,
            gilb_web_url: None,
        }),
    }
}

/// Sign out: delete the credentials file, returning the recorder to Tier-1.
#[tauri::command]
pub async fn sign_out(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    clear_credentials().map_err(|e| e.to_string())?;
    // Stop the running shipper NOW — it holds the token in memory and would
    // otherwise keep shipping the activity log until the next app restart,
    // after the user believes they've returned to local-only.
    state.engine.stop_shipper();
    // Drop the email line from the tray menu (the frontend refreshes itself).
    crate::tray::refresh(&app);
    Ok(())
}

/// Handle a `gilb://auth/callback` deep link: verify it matches the pending
/// login's `state`, persist the credentials, and emit `auth`. Errors are
/// logged (a deep link is fire-and-forget — there's no caller to return to).
pub fn handle_callback(app: &AppHandle, url: &url::Url) {
    if url.scheme() != "gilb" || url.host_str() != Some("auth") || url.path() != "/callback" {
        return; // not our auth callback
    }

    let mut token = None;
    let mut url_field = None;
    let mut employee = None;
    let mut state = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "token" => token = Some(v.into_owned()),
            "url" => url_field = Some(v.into_owned()),
            "employee" => employee = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            _ => {}
        }
    }

    let app_state = app.state::<AppState>();
    let pending = app_state.pending_auth.lock().take();
    let Some(pending) = pending else {
        warn!("auth callback with no login in progress; ignoring");
        return;
    };
    if state.as_deref() != Some(pending.state.as_str()) {
        warn!("auth callback state mismatch; ignoring");
        return;
    }

    let Some(token) = token.filter(|t| !t.is_empty()) else {
        warn!("auth callback missing token; ignoring");
        return;
    };

    // Trust the URL we sent the user to over whatever the callback echoes back.
    let creds = Credentials {
        gilb_web_url: url_field.unwrap_or(pending.gilb_web_url),
        token,
        employee,
    };

    if let Err(err) = save_credentials(&creds) {
        warn!(?err, "failed to save credentials from auth callback");
        let _ = app.emit(
            "auth",
            AuthStatus {
                signed_in: false,
                employee: None,
                gilb_web_url: None,
            },
        );
        return;
    }

    info!(employee = ?creds.employee, "recorder signed in");
    // Start shipping immediately — before this, a fresh sign-in didn't take
    // effect until the next app launch (Engine read credentials once at open).
    app_state
        .engine
        .start_shipper(creds.gilb_web_url.clone(), creds.token.clone());
    let _ = app.emit(
        "auth",
        AuthStatus {
            signed_in: true,
            employee: creds.employee,
            gilb_web_url: Some(creds.gilb_web_url),
        },
    );
    // Surface the signed-in email in the tray menu (reads it back from disk).
    crate::tray::refresh(app);
}

/// Percent-encode a query-parameter value (only the few chars we emit).
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
