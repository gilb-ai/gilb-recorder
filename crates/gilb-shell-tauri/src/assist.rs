//! Real-time meeting suggestions, host side: the overlay window, its commands
//! and hotkey, the user-facing switch, the whisper-model gate with its
//! download, and the wiring of the audio pipeline into webview events.
//!
//! Everything here is the same for any product that ships the feature, which is
//! why it stopped living in Rodnik. What differs is small and arrives through
//! [`AssistHost`]: whether the feature is available at all (Rodnik needs a
//! signed-in workspace, gilb will need the agent binary), which
//! [`AssistConfig`]/[`AssistBackend`] pair to run, and the strings a user sees.
//!
//! ## Contract with the webview
//!
//! The overlay (`assist.html`, shared frontend behind `VITE_FEATURE_ASSIST`)
//! listens for:
//!
//! ```text
//! assist://update  { text }      markdown, ready to render
//! assist://state   { loading }
//! assist://error   { message }
//! ```
//!
//! and the main window listens for `assist-status` ([`AssistStatus`]), which is
//! pushed on every transition so a settings card follows sign-in, download
//! progress and teardown without polling.
//!
//! The window is created hidden at wiring time so those listeners are
//! registered before the first suggestion, and only surfaces when the model
//! actually says something — silence never opens it.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use anyhow::Result;
use gilb_assist::{AssistBackend, AssistConfig, AssistEvent, AssistHandle, EngineParams};
use gilb_assist_audio::{
    spawn_assist_pipeline, AssistPipeline, AssistPipelineConfig, WhisperTranscriber,
};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tracing::{info, warn};

use crate::AudioTapHandle;

pub const ASSIST_WINDOW: &str = "assist";

/// Show/hide the overlay. Deliberately not the record hotkey's neighbour: on
/// macOS `Cmd+M` is already manual recording in Rodnik's shell.
const ASSIST_SHORTCUT: &str = "CmdOrCtrl+Backslash";

/// The whisper build both products ship, from gilb-config so transcription and
/// suggestions cannot drift apart. Gating the feature on a ~570 MB download
/// rather than bundling it keeps the installer light (D9).
pub const DEFAULT_MODEL_URL: &str = gilb_config::TRANSCRIBE_MODEL_URL;

/// What the product brings to the shared machinery.
pub trait AssistHost: Send + Sync + 'static {
    /// Can the feature run at all right now? Rodnik answers "signed in" — the
    /// prompt and the model endpoint come from its server; gilb will answer
    /// "the agent binary is installed". A `false` here hides the feature in the
    /// UI and refuses to wire it, without touching the user's preference.
    fn available(&self) -> bool {
        true
    }

    /// The two halves of the engine. Called on every wiring, so a product may
    /// rebuild them when its own state changed (a new session, a new agent).
    fn engine(&self) -> Result<(Box<dyn AssistConfig>, Box<dyn AssistBackend>)>;

    /// Language passed to the local transcriber. `"auto"` costs an extra probe
    /// pass per buffer, so name the language when you know it.
    fn language(&self) -> String {
        "auto".to_string()
    }

    /// Where the whisper model is fetched from when the user turns the feature
    /// on without it.
    fn model_url(&self) -> String {
        DEFAULT_MODEL_URL.to_string()
    }

    /// User-visible text. Kept out of the shared code because it carries the
    /// product's name and voice.
    fn strings(&self) -> AssistStrings;
}

/// The few strings the shared glue has to show.
#[derive(Clone)]
pub struct AssistStrings {
    /// Overlay window title (invisible with `decorations(false)`, but it is
    /// what the OS window list shows).
    pub window_title: String,
    /// Notification title.
    pub app_name: String,
    pub model_downloaded: String,
    pub model_failed: String,
    /// Refusal returned to the webview when the switch is flipped on while the
    /// feature is unavailable.
    pub unavailable: String,
}

/// The wired stack. Lives in a slot so the switch can tear it down (dropping
/// the pipeline aborts its tasks and, once every handle is gone, ends the
/// engine) and build it back up without a restart.
pub struct AssistState {
    host: Box<dyn AssistHost>,
    wired: parking_lot::Mutex<Option<Wired>>,
    shortcut_registered: AtomicBool,
    /// The user hid the panel by hand. Suggestions keep arriving into it, but
    /// it stops popping itself back up — until they reopen it or the next
    /// meeting starts. Fighting an explicit close is worse than a missed
    /// suggestion.
    auto_show_suppressed: AtomicBool,
    download: ModelDownload,
}

struct Wired {
    handle: AssistHandle,
    _pipeline: AssistPipeline,
}

#[derive(Default)]
struct ModelDownload {
    active: AtomicBool,
    percent: AtomicU8,
    /// Raised when the user turns the feature off mid-download.
    cancel: AtomicBool,
}

/// What a settings card renders: whether the feature is available at all, what
/// it is still missing, and whether it is actually running.
#[derive(Clone, serde::Serialize)]
pub struct AssistStatus {
    /// Product-level availability ([`AssistHost::available`]). `false` hides
    /// the whole control: there is nothing to turn on.
    pub available: bool,
    pub model_ready: bool,
    pub downloading: bool,
    /// Download progress; meaningless unless `downloading`.
    pub percent: u8,
    /// Effective state of the switch: on only when everything it needs is in
    /// place, so a stale "on" preference never renders as a working feature.
    pub enabled: bool,
}

fn state(app: &AppHandle) -> Option<tauri::State<'_, AssistState>> {
    app.try_state::<AssistState>()
}

fn model_ready() -> bool {
    gilb_config::transcribe_model_path()
        .map(|p| p.exists())
        .unwrap_or(false)
}

/// Whether the user-level switch is on (persisted in gilb-config prefs, so it
/// survives restarts and is shared with any other surface that shows it).
fn is_enabled() -> bool {
    gilb_config::load_preferences().assist_enabled
}

pub fn status(app: &AppHandle) -> AssistStatus {
    let Some(state) = state(app) else {
        return AssistStatus {
            available: false,
            model_ready: model_ready(),
            downloading: false,
            percent: 0,
            enabled: false,
        };
    };
    let available = state.host.available();
    let downloading = state.download.active.load(Ordering::SeqCst);
    AssistStatus {
        available,
        model_ready: model_ready(),
        downloading,
        percent: if downloading {
            state.download.percent.load(Ordering::SeqCst)
        } else {
            0
        },
        enabled: available && model_ready() && is_enabled(),
    }
}

/// Push the current state to the main window. Every transition goes through
/// here so a settings card follows progress without polling.
fn emit_status(app: &AppHandle) {
    let _ = app.emit_to("main", "assist-status", status(app));
}

/// Set up the shared slots and, when the switch and the gates allow, wire the
/// stack. Call once from the Tauri setup hook.
pub fn init(app: &AppHandle, host: impl AssistHost) {
    if state(app).is_none() {
        app.manage(AssistState {
            host: Box::new(host),
            wired: parking_lot::Mutex::new(None),
            shortcut_registered: AtomicBool::new(false),
            auto_show_suppressed: AtomicBool::new(false),
            download: ModelDownload::default(),
        });
    }
    if is_enabled() {
        wire(app);
    } else {
        info!("assist off: disabled by user preference");
    }
    emit_status(app);
}

/// Product-level availability changed (Rodnik: sign-in or sign-out). Wires or
/// tears down without touching the user's preference.
pub fn refresh(app: &AppHandle) {
    let available = state(app).is_some_and(|s| s.host.available());
    if available && is_enabled() {
        wire(app);
    } else {
        cancel_download(app);
        teardown(app);
    }
    emit_status(app);
}

/// Flip the user-level switch: persist it, then wire or tear down. Switching on
/// without the model starts its download and wiring follows when it lands, so
/// the UI needs no separate "download" button.
fn set_enabled(app: &AppHandle, on: bool) {
    let mut prefs = gilb_config::load_preferences();
    prefs.assist_enabled = on;
    if let Err(err) = gilb_config::save_preferences(&prefs) {
        warn!(error = %err, "failed to persist assist toggle");
    }
    if on {
        if model_ready() {
            wire(app);
        } else {
            start_model_download(app);
        }
    } else {
        // Stop paying for a model the user just declined.
        cancel_download(app);
        teardown(app);
    }
    emit_status(app);
}

fn cancel_download(app: &AppHandle) {
    if let Some(state) = state(app) {
        state.download.cancel.store(true, Ordering::SeqCst);
    }
}

/// Drop the pipeline (its tasks abort with it) and hide the overlay. The
/// preference is untouched — losing availability uses this too.
fn teardown(app: &AppHandle) {
    if let Some(state) = state(app) {
        if state.wired.lock().take().is_some() {
            info!("assist pipeline torn down");
        }
    }
    if let Some(window) = app.get_webview_window(ASSIST_WINDOW) {
        let _ = window.hide();
    }
}

/// Wire the stack. Quietly does nothing while the product says the feature is
/// unavailable or the whisper model is missing — those are the two gates, and
/// the status the UI renders explains both.
fn wire(app: &AppHandle) {
    let Some(state) = state(app) else { return };
    if !state.host.available() {
        info!("assist off: unavailable for this product state");
        return;
    }
    let mut wired = state.wired.lock();
    if wired.is_some() {
        return; // already running
    }
    let model = match gilb_config::transcribe_model_path() {
        Ok(path) if path.exists() => path,
        Ok(path) => {
            info!(model = %path.display(), "assist off: whisper model not downloaded");
            return;
        }
        Err(err) => {
            warn!(error = %err, "assist off: cannot resolve model path");
            return;
        }
    };
    let (config, backend) = match state.host.engine() {
        Ok(halves) => halves,
        Err(err) => {
            warn!(error = %err, "assist off: product could not build the engine");
            return;
        }
    };

    // Everything below starts tokio tasks with a bare `tokio::spawn` (the
    // engine, the audio pipeline, the STT worker), and wire() is called from
    // places that are NOT inside the runtime: the Tauri setup hook, a
    // synchronous command, an auth callback. Without entering the runtime here
    // the first spawn aborts the process with "there is no reactor running".
    let runtime = tauri::async_runtime::handle();
    let _runtime_guard = runtime.inner().enter();

    let (assist, mut events) = gilb_assist::spawn(config, backend, EngineParams::default());

    let tap = app.state::<AudioTapHandle>();
    // The recording bus marks meeting boundaries: each new meeting starts a
    // fresh conversation (and a fresh stream clock, echo canceller and voice
    // detector) instead of inheriting the previous client's context.
    let bus = (*app.state::<gilb_events::EventBus>()).clone();
    let bus_for_boundary = bus.clone();
    let pipeline = spawn_assist_pipeline(
        &tap.0,
        WhisperTranscriber::new(model, state.host.language()),
        assist.clone(),
        AssistPipelineConfig::default(),
        Some(bus),
    );
    *wired = Some(Wired {
        handle: assist,
        _pipeline: pipeline,
    });
    drop(wired);

    ensure_window(app);
    if !state.shortcut_registered.swap(true, Ordering::SeqCst) {
        register_shortcut(app);
    }

    // A new meeting is a fresh start for the panel too: a hide from the
    // previous call should not silence this one.
    {
        let app = app.clone();
        let mut rx = bus_for_boundary.subscribe_recording();
        tauri::async_runtime::spawn(async move {
            while let Ok(msg) = rx.recv().await {
                if let gilb_events::RecordingEvent::Armed { .. } = msg.payload {
                    set_auto_show_suppressed(&app, false);
                }
            }
        });
    }

    // Engine events → overlay webview. A suggestion surfaces the window;
    // loading/error only update it.
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                AssistEvent::Loading(loading) => {
                    let _ = app.emit_to(
                        ASSIST_WINDOW,
                        "assist://state",
                        serde_json::json!({ "loading": loading }),
                    );
                }
                AssistEvent::Update(text) => {
                    // Always delivered, so a reopened panel has the full
                    // history; only the pop-up respects an explicit hide.
                    if !auto_show_suppressed(&app) {
                        show_window(&app);
                    }
                    let _ = app.emit_to(
                        ASSIST_WINDOW,
                        "assist://update",
                        serde_json::json!({ "text": text }),
                    );
                }
                AssistEvent::Error(message) => {
                    let _ = app.emit_to(
                        ASSIST_WINDOW,
                        "assist://error",
                        serde_json::json!({ "message": message }),
                    );
                }
            }
        }
    });
    info!("assist pipeline wired");
}

/// Download the model in the background (D9). On success the stack wires
/// itself up — no restart needed.
fn start_model_download(app: &AppHandle) {
    use tauri_plugin_notification::NotificationExt;

    let Some(state) = state(app) else { return };
    if state.download.active.swap(true, Ordering::SeqCst) {
        return; // already running
    }
    state.download.percent.store(0, Ordering::SeqCst);
    state.download.cancel.store(false, Ordering::SeqCst);
    let strings = state.host.strings();
    let url = state.host.model_url();
    emit_status(app);

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let result = async {
            gilb_config::ensure_models_dir()?;
            let path = gilb_config::transcribe_model_path()?;
            let progress_app = app.clone();
            let state = app.state::<AssistState>();
            crate::model::download(&url, &path, &state.download.cancel, move |done, total| {
                if total == 0 {
                    return;
                }
                let pct = (done * 100 / total).min(100) as u8;
                let Some(state) = progress_app.try_state::<AssistState>() else {
                    return;
                };
                if state.download.percent.swap(pct, Ordering::SeqCst) != pct {
                    emit_status(&progress_app);
                }
            })
            .await
        }
        .await;

        if let Some(state) = app.try_state::<AssistState>() {
            state.download.active.store(false, Ordering::SeqCst);
        }
        match result {
            Ok(crate::model::Downloaded::Cancelled) => info!("assist model download cancelled"),
            Ok(crate::model::Downloaded::Completed(_)) => {
                info!("assist model downloaded");
                let _ = app
                    .notification()
                    .builder()
                    .title(&strings.app_name)
                    .body(&strings.model_downloaded)
                    .show();
                if is_enabled() {
                    wire(&app);
                }
            }
            Err(err) => {
                warn!(error = %err, "assist model download failed");
                let _ = app
                    .notification()
                    .builder()
                    .title(&strings.app_name)
                    .body(&strings.model_failed)
                    .show();
            }
        }
        emit_status(&app);
    });
}

/// Create the overlay if it doesn't exist yet — hidden, transparent,
/// always-on-top, excluded from our own screen capture, and never focused
/// (stealing focus from the meeting app mid-call is worse than a second click).
fn ensure_window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    if let Some(window) = app.get_webview_window(ASSIST_WINDOW) {
        return Some(window);
    }
    let title = state(app)
        .map(|s| s.host.strings().window_title)
        .unwrap_or_default();
    let builder =
        WebviewWindowBuilder::new(app, ASSIST_WINDOW, WebviewUrl::App("assist.html".into()))
            .title(title)
            .inner_size(380.0, 520.0)
            .resizable(true)
            .decorations(false)
            .transparent(true)
            .always_on_top(true)
            .skip_taskbar(true)
            .visible_on_all_workspaces(true)
            .content_protected(true)
            .focused(false)
            .visible(false);

    // No NSVisualEffectView here: the vibrancy material paints the window
    // opaque, which defeats transparent(true) — tried, and the panel came out
    // solid. Translucency stays a plain CSS alpha in assist.html.

    match builder.build() {
        Ok(window) => Some(window),
        Err(err) => {
            warn!(error = %err, "failed to create assist window");
            None
        }
    }
}

fn show_window(app: &AppHandle) {
    if let Some(window) = ensure_window(app) {
        let _ = window.show();
    }
}

fn auto_show_suppressed(app: &AppHandle) -> bool {
    state(app).is_some_and(|s| s.auto_show_suppressed.load(Ordering::SeqCst))
}

/// Remember (or forget) that the panel was closed on purpose.
fn set_auto_show_suppressed(app: &AppHandle, suppressed: bool) {
    if let Some(state) = state(app) {
        state
            .auto_show_suppressed
            .store(suppressed, Ordering::SeqCst);
    }
}

fn toggle_window(app: &AppHandle) {
    match app.get_webview_window(ASSIST_WINDOW) {
        Some(window) if window.is_visible().unwrap_or(false) => {
            let _ = window.hide();
            set_auto_show_suppressed(app, true);
        }
        // Reopening by hand is consent to see suggestions again.
        _ => {
            set_auto_show_suppressed(app, false);
            show_window(app);
        }
    }
}

fn register_shortcut(app: &AppHandle) {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
        let result = app
            .global_shortcut()
            .on_shortcut(ASSIST_SHORTCUT, |app, _shortcut, event| {
                if event.state() == ShortcutState::Pressed {
                    toggle_window(app);
                }
            });
        if let Err(err) = result {
            warn!(
                ?err,
                shortcut = ASSIST_SHORTCUT,
                "failed to register assist hotkey"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Commands — register them in the product's invoke_handler
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn assist_status(app: AppHandle) -> AssistStatus {
    status(&app)
}

/// The switch. Turning it on while the product says the feature is unavailable
/// is refused rather than silently persisting a preference that cannot take
/// effect.
#[tauri::command]
pub fn assist_set_enabled(app: AppHandle, on: bool) -> Result<(), String> {
    if on && !state(&app).is_some_and(|s| s.host.available()) {
        return Err(state(&app)
            .map(|s| s.host.strings().unavailable)
            .unwrap_or_else(|| "assist unavailable".into()));
    }
    set_enabled(&app, on);
    Ok(())
}

#[tauri::command]
pub fn assist_ask(app: AppHandle, question: String) -> Result<(), String> {
    let handle = state(&app).and_then(|s| s.wired.lock().as_ref().map(|w| w.handle.clone()));
    match handle {
        Some(handle) => {
            handle.ask(question);
            Ok(())
        }
        None => Err("assist is not active".into()),
    }
}

// No click-through command: a window with ignore_cursor_events(true) also
// ignores the control that would turn it off, so the overlay stayed inert until
// the app restarted. The panel simply takes the mouse; the hotkey hides it.

#[tauri::command]
pub fn assist_hide(app: AppHandle, window: tauri::WebviewWindow) {
    let _ = window.hide();
    set_auto_show_suppressed(&app, true);
}
