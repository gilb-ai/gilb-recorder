//! Single async task that consumes raw inputs and produces [`Action`]s.
//!
//! Platform-neutral: it reads [`RawEvent`]s and [`ClipboardChange`]s off
//! channels fed by an OS-specific capture source, and reaches the foreground
//! app + per-click element context through the [`FocusProvider`] /
//! [`ElementResolver`] traits. Everything else — the 300ms text debounce,
//! flush-on-click/focus/navigation-key/timeout, password masking, PII
//! redaction — is shared across platforms.
//!
//! Owns:
//! - [`TextBuffer`] with 300ms debounce,
//! - flushes on click / focus change / navigation key / timeout / stop.
//!
//! Reads:
//! - `raw_rx`: [`RawEvent`] from the platform input source,
//! - `clip_rx`: [`ClipboardChange`] from the platform clipboard source,
//! - shared [`FocusState`] (foreground app + focused-element role/secure flag).
//!
//! Writes:
//! - `writer_tx`: [`Action`]s for the engine writer,
//! - `element_resolver`: best-effort per-click element context.

use std::time::{Duration, Instant};

use chrono::Utc;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use gilb_config::RecordingSettings;
use gilb_core::{Action, ActionKind, AppInfo, ElementContext, Modifiers, SessionId, WriterMessage};
use gilb_events::{EventBus, HealthEvent};

use crate::events::{ClipboardChange, MouseButton, RawEvent};
use crate::focus::{FocusSnapshot, FocusState};
use crate::keyboard::SpecialKey;
use crate::password_masking::{is_excluded_app, is_password_field, is_secure_role, redact_pii};
use crate::text_buffer::{FlushReason, FlushedText, TextBuffer};
use crate::{ElementResolver, FocusProvider};

/// How long we wait between drop-stat publishes on the health bus.
const DROP_REPORT_INTERVAL: Duration = Duration::from_secs(1);

pub struct Normalizer {
    pub session_id: SessionId,
    pub writer_tx: mpsc::Sender<WriterMessage>,
    pub event_bus: EventBus,
    pub settings: RecordingSettings,
    pub focus: FocusState,
    /// Looks up the frontmost app; platform-specific.
    pub focus_provider: Box<dyn FocusProvider>,
    /// Best-effort per-click element context; platform-specific.
    pub element_resolver: Box<dyn ElementResolver>,
    /// `Some` when `settings.capture_tree_snapshots` is enabled — the
    /// platform wires this to the snapshot worker's focus-event channel.
    /// We `try_send` on focus change (lossy: a backed-up worker drops
    /// the hop rather than parking the normalizer task).
    pub snapshot_tx: Option<mpsc::Sender<AppInfo>>,
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
        self.focus.update_app(self.focus_provider.frontmost());

        let mut focus_tick = tokio::time::interval(Duration::from_millis(500));
        focus_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut text_tick = tokio::time::interval(Duration::from_millis(100));
        text_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut drops_since_report: u64 = 0;
        let mut last_drop_report = Instant::now();
        // Idle/alive lifecycle tracker — produces `system` actions the server
        // segments cases/sessions on (GILB-77). macOS lock/unlock/sleep/wake
        // signals come from platform/macos/session.rs (later).
        let mut idle = crate::idle::IdleTracker::new(Instant::now());

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
                    let app = self.focus_provider.frontmost_with_window();
                    let prev = self.focus.current();
                    let app_changed = prev.app.bundle_id != app.bundle_id
                        || prev.app.pid != app.pid;
                    // SPA navigation (Gmail / Linear / Crunchbase) often keeps the
                    // window title stable across URL transitions; URL change alone
                    // should still trigger a focus_change + fresh tree snapshot.
                    let window_changed = prev.app.window_title != app.window_title
                        || prev.app.browser_url != app.browser_url;
                    if app_changed || window_changed {
                        self.flush_text(&mut buffer, FlushReason::FocusChange).await;
                        self.focus.update_app(app.clone());
                        self.emit_focus_change(app.clone()).await;
                        if let Some(tx) = &self.snapshot_tx {
                            // Lossy: if the snapshot worker is still busy on
                            // the previous focus, we'd rather skip this hop
                            // than let the normalizer task stall on send.
                            if tx.try_send(app).is_err() {
                                self.event_bus.publish_health(HealthEvent::DroppedEvent {
                                    reason: "snapshot_tx_full".into(),
                                    count: 1,
                                });
                            }
                        }
                    } else {
                        self.focus.update_app(app);
                    }
                }
                _ = text_tick.tick() => {
                    for sig in idle.tick(Instant::now()) {
                        self.emit_system(sig).await;
                    }
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
                    if let Some(sig) = idle.on_input(Instant::now()) {
                        self.emit_system(sig).await;
                    }
                }
                Some(change) = clip_rx.recv() => {
                    if self.settings.capture_clipboard {
                        self.handle_clipboard(change, &mut drops_since_report).await;
                        if let Some(sig) = idle.on_input(Instant::now()) {
                            self.emit_system(sig).await;
                        }
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
            RawEvent::KeyDown {
                special,
                text,
                modifiers,
            } => {
                if let Some(special) = special {
                    if special.is_navigation() {
                        self.flush_text(buffer, FlushReason::NavigationKey).await;
                        self.emit_key(special, modifiers, &snap, drops).await;
                        return;
                    }
                    if matches!(special, SpecialKey::Backspace | SpecialKey::Delete) {
                        // Drop the most recent grapheme from the buffer so
                        // undo/edit operations don't leak the final string.
                        // For Phase 1 we just flush, which over-counts edits
                        // but never leaks more than typed.
                        self.flush_text(buffer, FlushReason::NavigationKey).await;
                        self.emit_key(special, modifiers, &snap, drops).await;
                        return;
                    }
                }

                let Some(s) = text else { return };
                let masked = snap
                    .focused_role
                    .as_deref()
                    .map(is_secure_role)
                    .unwrap_or(false)
                    || snap.focused_secure;
                buffer.push(&s, masked);
            }
            RawEvent::MouseDown {
                button,
                x,
                y,
                click_count,
                modifiers,
            } => {
                self.flush_text(buffer, FlushReason::Click).await;
                self.emit_click(button, x, y, click_count, modifiers, &snap, drops)
                    .await;
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
        let masked = flushed.masked || is_secure_role(role) || snap.focused_secure;
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
            clipboard_op: None,
            content_hash: None,
        };
        self.emit_blocking(action).await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn emit_click(
        &self,
        button: MouseButton,
        x: f64,
        y: f64,
        click_count: u32,
        modifiers: Modifiers,
        snap: &FocusSnapshot,
        drops: &mut u64,
    ) {
        // Best-effort: ask the resolver for element context. Block briefly so
        // most clicks get enriched, but fall through fast on timeout.
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.element_resolver.submit(x, y, tx);
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
                "click_count": click_count,
                "modifiers": modifiers.0,
            })),
            clipboard_op: None,
            content_hash: None,
        };
        self.emit_lossy(action, drops);
    }

    async fn emit_key(
        &self,
        special: SpecialKey,
        modifiers: Modifiers,
        snap: &FocusSnapshot,
        drops: &mut u64,
    ) {
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
                "modifiers": modifiers.0,
            })),
            clipboard_op: None,
            content_hash: None,
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
            clipboard_op: None,
            content_hash: None,
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
            clipboard_op: None,
            content_hash: None,
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
        // Hash the RAW clipboard text BEFORE redaction so copy↔paste linking
        // survives `redact_pii` on the shipped `text_content`. Step-1 op is
        // always "copy" (NSPasteboard can't distinguish copy/cut).
        let content_hash = change.text.as_deref().and_then(clipboard_content_hash);
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
            clipboard_op: Some("copy".to_string()),
            content_hash,
        };
        self.emit_lossy(action, drops);
    }

    fn log_emit(action: &Action) {
        debug!(
            kind = action.kind.as_str(),
            app = action.app.name.as_deref().unwrap_or("-"),
            role = action.element.role.as_deref().unwrap_or("-"),
            masked = action.password_flag,
        );
    }

    async fn emit_blocking(&self, action: Action) {
        Self::log_emit(&action);
        if let Err(err) = self.writer_tx.send(WriterMessage::Action(action)).await {
            warn!(?err, "normalizer: writer channel closed");
        }
    }

    /// Persist a lifecycle `system` action (idle/alive now; lock/unlock/recording
    /// arrive from the macOS session source later). Carries the foreground app so
    /// the server can correlate; no text/element PII.
    async fn emit_system(&self, signal: crate::idle::SystemSignal) {
        let snap = self.focus.current();
        let action = Action {
            session_id: self.session_id,
            captured_at: Utc::now(),
            kind: ActionKind::System,
            app: snap.app.clone(),
            element: ElementContext::default(),
            text_content: None,
            password_flag: false,
            tree_snapshot_id: None,
            extra_json: Some(serde_json::json!({ "system": signal.as_str() })),
            clipboard_op: None,
            content_hash: None,
        };
        self.emit_blocking(action).await;
    }

    fn emit_lossy(&self, action: Action, drops: &mut u64) {
        Self::log_emit(&action);
        if self
            .writer_tx
            .try_send(WriterMessage::Action(action))
            .is_err()
        {
            *drops += 1;
        }
    }
}

/// sha256 (hex) of a raw clipboard string, or `None` for empty input.
///
/// Computed from the PRE-redaction bytes so copy↔paste linking via
/// `content_hash` survives `redact_pii` on the shipped `text_content`.
fn clipboard_content_hash(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    Some(
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_content_hash_is_deterministic_and_skips_empty() {
        // sha256("hello") == 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        assert_eq!(
            clipboard_content_hash("hello").as_deref(),
            Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
        );
        // Empty input → None (no hash, no linking value).
        assert_eq!(clipboard_content_hash(""), None);
        // Same raw text → same hash (stable across calls; redaction of the
        // shipped text_content does not affect it).
        assert_eq!(
            clipboard_content_hash("invoice-123"),
            clipboard_content_hash("invoice-123")
        );
        // Different text → different hash.
        assert_ne!(
            clipboard_content_hash("invoice-123"),
            clipboard_content_hash("invoice-456")
        );
    }
}
