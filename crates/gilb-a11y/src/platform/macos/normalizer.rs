//! Single async task that consumes raw inputs and produces [`Action`]s.
//!
//! Owns:
//! - [`TextBuffer`] with 300ms debounce,
//! - flushes on click / focus change / navigation key / timeout / stop.
//!
//! Key→text translation lives in the CGEventTap callback (see
//! `event_tap.rs::extract_unicode`) — the OS gives us composed chars
//! directly on the event, so we don't need a TIS/UCKeyTranslate detour.
//!
//! Reads:
//! - `raw_rx`: [`RawEvent`] from the CGEventTap thread,
//! - `clip_rx`: [`ClipboardChange`] from the pasteboard poller,
//! - shared [`FocusState`] (foreground app + AX role).
//!
//! Writes:
//! - `action_tx`: [`Action`]s for the engine writer,
//! - `ax_worker`: best-effort [`AxJob`]s to enrich clicks with element context.

use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use gilb_config::RecordingSettings;
use gilb_core::{Action, ActionKind, AppInfo, ElementContext, SessionId};
use gilb_events::{EventBus, HealthEvent};

use crate::password_masking::{is_excluded_app, is_password_field, is_secure_role, redact_pii};
use crate::text_buffer::{FlushReason, FlushedText, TextBuffer};

use super::ax_worker::{AxJob, AxWorker};
use super::event_tap::{MouseButton, RawEvent};
use super::focus::{frontmost_app, frontmost_app_with_window, FocusSnapshot, FocusState};
use super::keyboard::SpecialKey;
use super::pasteboard::ClipboardChange;

/// How long we wait between drop-stat publishes on the health bus.
const DROP_REPORT_INTERVAL: Duration = Duration::from_secs(1);

pub struct Normalizer {
    pub session_id: SessionId,
    pub action_tx: mpsc::Sender<Action>,
    pub event_bus: EventBus,
    pub settings: RecordingSettings,
    pub focus: FocusState,
    pub ax_worker: AxWorker,
}

impl Normalizer {
    pub async fn run(
        self,
        mut raw_rx: mpsc::Receiver<RawEvent>,
        mut clip_rx: mpsc::Receiver<ClipboardChange>,
        mut shutdown: tokio::sync::oneshot::Receiver<()>,
    ) {
        let mut buffer = TextBuffer::default();

        // Prime focus once so the first event already carries app context.
        self.focus.update_app(frontmost_app());

        let mut focus_tick = tokio::time::interval(Duration::from_millis(500));
        focus_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut text_tick = tokio::time::interval(Duration::from_millis(100));
        text_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut drops_since_report: u64 = 0;
        let mut last_drop_report = Instant::now();

        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    self.flush_text(&mut buffer, FlushReason::Stopping).await;
                    self.event_bus.publish_health(HealthEvent::Stopped {
                        reason: "shutdown".into(),
                    });
                    break;
                }
                _ = focus_tick.tick() => {
                    // Read app + focused-window title together so a window
                    // change inside the same app still emits a focus_change.
                    // Bounded by AX_TITLE_TIMEOUT inside `frontmost_app_with_window`.
                    let app = frontmost_app_with_window();
                    let prev = self.focus.current();
                    let app_changed = prev.app.bundle_id != app.bundle_id
                        || prev.app.pid != app.pid;
                    let window_changed = prev.app.window_title != app.window_title;
                    if app_changed || window_changed {
                        self.flush_text(&mut buffer, FlushReason::FocusChange).await;
                        self.focus.update_app(app.clone());
                        self.emit_focus_change(app).await;
                    } else {
                        self.focus.update_app(app);
                    }
                }
                _ = text_tick.tick() => {
                    if buffer.should_timeout(Instant::now()) {
                        self.flush_text(&mut buffer, FlushReason::Timeout).await;
                    }
                    if last_drop_report.elapsed() >= DROP_REPORT_INTERVAL
                        && drops_since_report > 0
                    {
                        self.event_bus.publish_health(HealthEvent::DroppedEvent {
                            reason: "action_tx_full".into(),
                            count: drops_since_report,
                        });
                        drops_since_report = 0;
                        last_drop_report = Instant::now();
                    }
                }
                Some(ev) = raw_rx.recv() => {
                    self.handle_raw(ev, &mut buffer, &mut drops_since_report).await;
                }
                Some(change) = clip_rx.recv() => {
                    if self.settings.capture_clipboard {
                        self.handle_clipboard(change, &mut drops_since_report).await;
                    }
                }
                else => break,
            }
        }
    }

    async fn handle_raw(&self, ev: RawEvent, buffer: &mut TextBuffer, drops: &mut u64) {
        let snap = self.focus.current();
        if let Some(bid) = snap.app.bundle_id.as_deref() {
            if is_excluded_app(bid) {
                // Drop on the floor — but still flush any buffered text so we
                // don't leak it across the focus boundary.
                self.flush_text(buffer, FlushReason::FocusChange).await;
                return;
            }
        }

        match ev {
            RawEvent::KeyDown { keycode, text, .. } => {
                if let Some(special) = SpecialKey::from_keycode(keycode) {
                    if special.is_navigation() {
                        self.flush_text(buffer, FlushReason::NavigationKey).await;
                        self.emit_key(special, &snap, drops).await;
                        return;
                    }
                    if matches!(special, SpecialKey::Backspace | SpecialKey::Delete) {
                        // Drop the most recent grapheme from the buffer so
                        // undo/edit operations don't leak the final string.
                        // For Phase 1 we just flush, which over-counts edits
                        // but never leaks more than typed.
                        self.flush_text(buffer, FlushReason::NavigationKey).await;
                        self.emit_key(special, &snap, drops).await;
                        return;
                    }
                }

                let Some(s) = text else { return };
                let masked = snap
                    .focused_role
                    .as_deref()
                    .map(is_secure_role)
                    .unwrap_or(false);
                buffer.push(&s, masked);
            }
            RawEvent::FlagsChanged { .. } => {
                // Modifier-only changes don't produce text; ignored.
            }
            RawEvent::MouseDown { button, x, y, .. } => {
                self.flush_text(buffer, FlushReason::Click).await;
                self.emit_click(button, x, y, &snap, drops).await;
            }
            RawEvent::Scroll { delta_x, delta_y } => {
                self.emit_scroll(delta_x, delta_y, &snap, drops).await;
            }
        }
    }

    async fn flush_text(&self, buffer: &mut TextBuffer, reason: FlushReason) {
        if let Some(flushed) = buffer.flush(reason) {
            let snap = self.focus.current();
            if let Some(bid) = snap.app.bundle_id.as_deref() {
                if is_excluded_app(bid) {
                    return;
                }
            }
            self.emit_text(flushed, &snap).await;
        }
    }

    async fn emit_text(&self, flushed: FlushedText, snap: &FocusSnapshot) {
        let role = snap.focused_role.as_deref().unwrap_or("");
        let masked = flushed.masked || is_secure_role(role);
        let content = if masked {
            "[masked]".to_string()
        } else {
            redact_pii(&flushed.text)
        };
        let elem = ElementContext {
            role: snap.focused_role.clone(),
            ..ElementContext::default()
        };
        let action = Action {
            session_id: self.session_id,
            captured_at: Utc::now(),
            kind: ActionKind::Text,
            app: snap.app.clone(),
            element: elem,
            text_content: Some(content),
            password_flag: masked,
            tree_snapshot_id: None,
            extra_json: Some(serde_json::json!({
                "flush_reason": format!("{:?}", flushed.reason),
                "char_count": flushed.text.chars().count(),
            })),
        };
        self.emit_blocking(action).await;
    }

    async fn emit_click(
        &self,
        button: MouseButton,
        x: f64,
        y: f64,
        snap: &FocusSnapshot,
        drops: &mut u64,
    ) {
        // Best-effort: ask the AX worker for element context. Block briefly so
        // most clicks get enriched, but fall through fast on timeout.
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.ax_worker.submit(AxJob { x, y, reply: tx });
        let element_ctx = tokio::time::timeout(Duration::from_millis(160), rx)
            .await
            .ok()
            .and_then(|res| res.ok())
            .flatten()
            .unwrap_or_default();

        let pwd = is_password_field(
            element_ctx.name.as_deref(),
            element_ctx.identifier.as_deref(),
        ) || element_ctx
            .role
            .as_deref()
            .map(is_secure_role)
            .unwrap_or(false);

        let action = Action {
            session_id: self.session_id,
            captured_at: Utc::now(),
            kind: ActionKind::Click,
            app: snap.app.clone(),
            element: element_ctx,
            text_content: None,
            password_flag: pwd,
            tree_snapshot_id: None,
            extra_json: Some(serde_json::json!({
                "button": format!("{:?}", button),
                "x": x,
                "y": y,
            })),
        };
        self.emit_lossy(action, drops);
    }

    async fn emit_key(&self, special: SpecialKey, snap: &FocusSnapshot, drops: &mut u64) {
        let action = Action {
            session_id: self.session_id,
            captured_at: Utc::now(),
            kind: ActionKind::Key,
            app: snap.app.clone(),
            element: ElementContext {
                role: snap.focused_role.clone(),
                ..ElementContext::default()
            },
            text_content: None,
            password_flag: false,
            tree_snapshot_id: None,
            extra_json: Some(serde_json::json!({
                "key": format!("{:?}", special),
            })),
        };
        self.emit_lossy(action, drops);
    }

    async fn emit_scroll(&self, dx: i64, dy: i64, snap: &FocusSnapshot, drops: &mut u64) {
        let action = Action {
            session_id: self.session_id,
            captured_at: Utc::now(),
            kind: ActionKind::Scroll,
            app: snap.app.clone(),
            element: ElementContext::default(),
            text_content: None,
            password_flag: false,
            tree_snapshot_id: None,
            extra_json: Some(serde_json::json!({
                "delta_x": dx,
                "delta_y": dy,
            })),
        };
        self.emit_lossy(action, drops);
    }

    async fn emit_focus_change(&self, app: AppInfo) {
        let action = Action {
            session_id: self.session_id,
            captured_at: Utc::now(),
            kind: ActionKind::FocusChange,
            app,
            element: ElementContext::default(),
            text_content: None,
            password_flag: false,
            tree_snapshot_id: None,
            extra_json: None,
        };
        self.emit_blocking(action).await;
    }

    async fn handle_clipboard(&self, change: ClipboardChange, drops: &mut u64) {
        let snap = self.focus.current();
        if let Some(bid) = snap.app.bundle_id.as_deref() {
            if is_excluded_app(bid) {
                return;
            }
        }
        let text = change
            .text
            .map(|t| redact_pii(&t))
            .filter(|t| !t.is_empty());
        let action = Action {
            session_id: self.session_id,
            captured_at: Utc::now(),
            kind: ActionKind::Clipboard,
            app: snap.app.clone(),
            element: ElementContext::default(),
            text_content: text,
            password_flag: false,
            tree_snapshot_id: None,
            extra_json: Some(serde_json::json!({
                "change_count": change.change_count,
            })),
        };
        self.emit_lossy(action, drops);
    }

    fn log_emit(action: &Action) {
        debug!(
            kind = action.kind.as_str(),
            app = action.app.name.as_deref().unwrap_or("-"),
            role = action.element.role.as_deref().unwrap_or("-"),
            masked = action.password_flag,
            "normalizer: emit"
        );
    }

    async fn emit_blocking(&self, action: Action) {
        Self::log_emit(&action);
        if let Err(err) = self.action_tx.send(action).await {
            warn!(?err, "normalizer: action channel closed");
        }
    }

    fn emit_lossy(&self, action: Action, drops: &mut u64) {
        Self::log_emit(&action);
        if self.action_tx.try_send(action).is_err() {
            *drops += 1;
        }
    }
}
