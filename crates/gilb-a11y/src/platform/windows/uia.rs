//! UI Automation worker.
//!
//! COM (and the `IUIAutomation` object) is apartment-bound, so it lives on a
//! dedicated single-threaded-apartment (STA) thread. The worker mirrors the
//! macOS `AxWorker`: a bounded crossbeam channel of best-effort jobs, each
//! resolving the element under a click via `ElementFromPoint`, building an
//! [`ElementContext`], and updating the shared [`FocusState`] (role + secure
//! flag) so the next text flush can mask password fields.

use std::thread::JoinHandle;

use crossbeam_channel as cc;
use tracing::{debug, error};

use windows::core::Interface;
use windows::Win32::Foundation::POINT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationValuePattern,
    UIA_CONTROLTYPE_ID, UIA_ValuePatternId,
};

use gilb_core::{ElementContext, Frame};

use crate::focus::FocusState;
use crate::ElementResolver;

pub const QUEUE_CAPACITY: usize = 4;

struct UiaJob {
    x: f64,
    y: f64,
    reply: tokio::sync::oneshot::Sender<Option<ElementContext>>,
}

#[derive(Clone)]
pub struct UiaWorker {
    tx: cc::Sender<UiaJob>,
}

pub struct UiaWorkerHandle {
    join: Option<JoinHandle<()>>,
    shutdown: cc::Sender<()>,
}

impl UiaWorker {
    pub fn spawn(focus: FocusState) -> (UiaWorker, UiaWorkerHandle) {
        let (tx, rx) = cc::bounded::<UiaJob>(QUEUE_CAPACITY);
        let (shutdown_tx, shutdown_rx) = cc::bounded::<()>(1);

        let join = std::thread::Builder::new()
            .name("gilb-uia-worker".into())
            .spawn(move || run_loop(rx, shutdown_rx, focus))
            .expect("spawn gilb-uia-worker");

        (
            UiaWorker { tx },
            UiaWorkerHandle {
                join: Some(join),
                shutdown: shutdown_tx,
            },
        )
    }
}

impl ElementResolver for UiaWorker {
    fn submit(&self, x: f64, y: f64, reply: tokio::sync::oneshot::Sender<Option<ElementContext>>) {
        if let Err(err) = self.tx.try_send(UiaJob { x, y, reply }) {
            debug!(?err, "uia queue full, dropping job");
        }
    }
}

impl Drop for UiaWorkerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn run_loop(rx: cc::Receiver<UiaJob>, shutdown: cc::Receiver<()>, focus: FocusState) {
    unsafe {
        // STA is the apartment model UIA expects. S_FALSE (already
        // initialised) is fine; we still balance with CoUninitialize.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let automation: IUIAutomation =
            match CoCreateInstance(&CUIAutomation, None, CLSCTX_ALL) {
                Ok(a) => a,
                Err(e) => {
                    error!(?e, "CoCreateInstance(CUIAutomation) failed");
                    CoUninitialize();
                    return;
                }
            };

        debug!("ready");
        loop {
            cc::select! {
                recv(shutdown) -> _ => break,
                recv(rx) -> job => {
                    let Ok(job) = job else { break };
                    match resolve(&automation, job.x, job.y) {
                        Some((ctx, secure)) => {
                            focus.set_focused_role(ctx.role.clone());
                            focus.set_focused_secure(secure);
                            let _ = job.reply.send(Some(ctx));
                        }
                        None => {
                            let _ = job.reply.send(None);
                        }
                    }
                }
            }
        }

        drop(automation);
        CoUninitialize();
    }
    debug!("shut down");
}

/// Resolve the element at screen `(x, y)`. Returns the context plus whether
/// the element is a secure/password field.
fn resolve(automation: &IUIAutomation, x: f64, y: f64) -> Option<(ElementContext, bool)> {
    unsafe {
        let pt = POINT {
            x: x as i32,
            y: y as i32,
        };
        let el = automation.ElementFromPoint(pt).ok()?;

        let role = el.CurrentControlType().ok().map(role_name);
        let name = bstr_opt(el.CurrentName().ok());
        let identifier = bstr_opt(el.CurrentAutomationId().ok());
        let help = bstr_opt(el.CurrentHelpText().ok());
        let value = value_of(&el);
        let secure = el.CurrentIsPassword().map(|b| b.as_bool()).unwrap_or(false);
        let frame = el.CurrentBoundingRectangle().ok().map(|r| Frame {
            x: r.left as f64,
            y: r.top as f64,
            w: (r.right - r.left) as f64,
            h: (r.bottom - r.top) as f64,
        });

        Some((
            ElementContext {
                role,
                name,
                value,
                help,
                identifier,
                frame,
            },
            secure,
        ))
    }
}

/// Read the Value pattern's current value, if the element supports it.
fn value_of(el: &IUIAutomationElement) -> Option<String> {
    unsafe {
        let pattern = el.GetCurrentPattern(UIA_ValuePatternId).ok()?;
        let value_pattern: IUIAutomationValuePattern = pattern.cast().ok()?;
        bstr_opt(value_pattern.CurrentValue().ok())
    }
}

fn bstr_opt(b: Option<windows::core::BSTR>) -> Option<String> {
    let s = b?.to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Map a UIA control-type id to a short role string. Falls back to the raw id
/// for the long tail we don't name explicitly.
fn role_name(id: UIA_CONTROLTYPE_ID) -> String {
    let name = match id.0 {
        50000 => "button",
        50001 => "calendar",
        50002 => "checkbox",
        50003 => "combobox",
        50004 => "edit",
        50005 => "hyperlink",
        50006 => "image",
        50007 => "listitem",
        50008 => "list",
        50009 => "menu",
        50010 => "menubar",
        50011 => "menuitem",
        50012 => "progressbar",
        50013 => "radiobutton",
        50014 => "scrollbar",
        50015 => "slider",
        50016 => "spinner",
        50017 => "statusbar",
        50018 => "tab",
        50019 => "tabitem",
        50020 => "text",
        50021 => "toolbar",
        50022 => "tooltip",
        50023 => "tree",
        50024 => "treeitem",
        50025 => "custom",
        50026 => "group",
        50028 => "datagrid",
        50029 => "dataitem",
        50030 => "document",
        50031 => "splitbutton",
        50032 => "window",
        50033 => "pane",
        50036 => "table",
        50037 => "titlebar",
        50038 => "separator",
        other => return format!("control:{other}"),
    };
    name.to_string()
}
