//! Walk the UI Automation tree of the foreground window.
//!
//! Mirrors `walker_macos` (same `Node` output, same caps) but uses UIA.
//! Crucially it caches the whole subtree in a single `BuildCache` COM call
//! (`TreeScope_Subtree`) and then traverses the cached children — a live
//! walk would issue several cross-process COM calls per node and blow the
//! budget on large trees.
//!
//! Runs on the snapshotter's blocking-pool thread, so COM is initialised
//! (MTA) here and deliberately never uninitialised — the pool thread is
//! reused and UIA stays usable for the next walk.

use std::time::{Duration, Instant};

use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    AutomationElementMode_None, CUIAutomation, IUIAutomation, IUIAutomationElement,
    TreeScope_Subtree, UIA_ControlTypePropertyId, UIA_NamePropertyId, UIA_CONTROLTYPE_ID,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use super::node::Node;

const MAX_DEPTH: usize = 12;
const MAX_NODES: usize = 800;
const TOTAL_BUDGET: Duration = Duration::from_millis(800);

/// Walk the foreground window's UIA tree and return its nodes. The `pid`
/// from the focus event guards against a race: if the foreground window no
/// longer belongs to that process (focus moved on while this was queued) we
/// return `None` rather than mislabel another app's tree.
pub fn walk_focused_window(pid: i32) -> Option<Vec<Node>> {
    unsafe {
        // MTA is fine for synchronous UIA use (only event callbacks need STA).
        // Result intentionally ignored: S_OK / S_FALSE / RPC_E_CHANGED_MODE all
        // leave COM usable on this thread.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut fg_pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut fg_pid));
        if pid >= 0 && fg_pid != pid as u32 {
            return None;
        }

        let automation: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_ALL).ok()?;

        // Cache name + control type for the whole subtree in one COM call.
        let cache = automation.CreateCacheRequest().ok()?;
        cache.AddProperty(UIA_NamePropertyId).ok()?;
        cache.AddProperty(UIA_ControlTypePropertyId).ok()?;
        if let Ok(control_view) = automation.ControlViewCondition() {
            let _ = cache.SetTreeFilter(&control_view);
        }
        let _ = cache.SetTreeScope(TreeScope_Subtree);
        // Cached-only: we never touch live properties, so don't hold COM refs.
        let _ = cache.SetAutomationElementMode(AutomationElementMode_None);

        let root = automation.ElementFromHandleBuildCache(hwnd, &cache).ok()?;

        let mut state = WalkState {
            nodes: Vec::with_capacity(64),
            deadline: Instant::now() + TOTAL_BUDGET,
        };
        walk_cached(&root, 0, &mut state);

        if state.nodes.is_empty() {
            None
        } else {
            Some(state.nodes)
        }
    }
}

struct WalkState {
    nodes: Vec<Node>,
    deadline: Instant,
}

impl WalkState {
    fn should_stop(&self) -> bool {
        self.nodes.len() >= MAX_NODES || Instant::now() >= self.deadline
    }
}

/// Traverse the cached subtree. All reads are `Cached*` — no COM round trips.
fn walk_cached(elem: &IUIAutomationElement, depth: usize, state: &mut WalkState) {
    if depth >= MAX_DEPTH || state.should_stop() {
        return;
    }

    let role = role_name(unsafe { elem.CachedControlType() }.unwrap_or_default());

    // Decorative roles carry no signal; skip the node and its subtree.
    if matches!(role.as_str(), "image" | "separator") {
        return;
    }

    let name = bstr_opt(unsafe { elem.CachedName() }.ok());

    state.nodes.push(Node {
        role,
        name,
        // Value would need the Value pattern (extra COM / VARIANT handling);
        // deferred — name + role carry the bulk of the structural signal.
        value: None,
        depth: depth as u8,
    });

    if let Ok(children) = unsafe { elem.GetCachedChildren() } {
        let len = unsafe { children.Length() }.unwrap_or(0);
        for i in 0..len {
            if state.should_stop() {
                break;
            }
            if let Ok(child) = unsafe { children.GetElement(i) } {
                walk_cached(&child, depth + 1, state);
            }
        }
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

/// Map a UIA control-type id to a short role string (canonical, English),
/// falling back to the raw id for the long tail.
pub fn role_name(id: UIA_CONTROLTYPE_ID) -> String {
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
        50027 => "thumb",
        50028 => "datagrid",
        50029 => "dataitem",
        50030 => "document",
        50031 => "splitbutton",
        50032 => "window",
        50033 => "pane",
        50034 => "header",
        50035 => "headeritem",
        50036 => "table",
        50037 => "titlebar",
        50038 => "separator",
        other => return format!("control:{other}"),
    };
    name.to_string()
}
