import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import DOMPurify from "dompurify";
import { marked } from "marked";
import { applyI18n, t } from "./i18n";

// Real-time assist overlay. A borderless, transparent, always-on-top window
// created from Rust; everything it shows arrives as events, everything it does
// goes through commands — the webview knows nothing about providers or
// prompts (REALTIME_ASSIST.md §4.4):
//   assist://update  { text }     ready-to-render markdown
//   assist://state   { loading }  spinner on/off
//   assist://error   { message }
// Commands: assist_ask(question), assist_set_click_through(on), assist_hide.

const $ = <T extends HTMLElement = HTMLElement>(id: string) =>
  document.getElementById(id) as T | null;

/// Model output is untrusted markdown: render with marked, sanitize the HTML.
function render(md: string): string {
  return DOMPurify.sanitize(marked.parse(md, { async: false }));
}

const responses: string[] = [];
let shown = -1;
let ghost = false;

function show(idx: number) {
  const content = $("assist-content");
  const pos = $("assist-pos");
  if (!content || idx < 0 || idx >= responses.length) return;
  shown = idx;
  content.innerHTML = render(responses[idx]);
  content.scrollTop = 0;
  if (pos) pos.textContent = `${idx + 1}/${responses.length}`;
}

window.addEventListener("DOMContentLoaded", async () => {
  applyI18n();
  const input = $<HTMLInputElement>("assist-input");
  if (input) input.placeholder = t("assist.askPlaceholder");

  await listen<{ text: string }>("assist://update", (e) => {
    responses.push(e.payload.text);
    show(responses.length - 1);
    $("assist-error")?.setAttribute("hidden", "");
  });

  await listen<{ loading: boolean }>("assist://state", (e) => {
    $("assist-dot")?.classList.toggle("loading", e.payload.loading);
  });

  await listen<{ message: string }>("assist://error", (e) => {
    const el = $("assist-error");
    if (el) {
      el.textContent = t("assist.error", { error: e.payload.message });
      el.removeAttribute("hidden");
    }
  });

  // Dragging by the title bar. data-tauri-drag-region does this on its own,
  // but its invoke is fire-and-forget: if the host has not granted
  // core:window:allow-start-dragging, the window simply refuses to move with
  // nothing to show for it. Calling startDragging() ourselves surfaces that
  // in the panel instead of failing silently.
  $("assist-bar-region")?.addEventListener("mousedown", async (e) => {
    const target = e.target as HTMLElement | null;
    if (target?.closest("button, input, a")) return; // let controls work
    try {
      await getCurrentWindow().startDragging();
    } catch (err) {
      const el = $("assist-error");
      if (el) {
        el.textContent = t("assist.error", { error: String(err) });
        el.removeAttribute("hidden");
      }
    }
  });

  $("assist-prev")?.addEventListener("click", () => show(shown - 1));
  $("assist-next")?.addEventListener("click", () => show(shown + 1));

  // Ghost mode = click-through. The window stops taking mouse events, so the
  // only way back is the same global shortcut / tray — the button reflects
  // the state it can no longer toggle off; Rust owns the real state.
  $("assist-ghost")?.addEventListener("click", async () => {
    ghost = !ghost;
    document.body.classList.toggle("ghost", ghost);
    await invoke("assist_set_click_through", { on: ghost });
  });

  $("assist-hide")?.addEventListener("click", () => invoke("assist_hide"));
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") invoke("assist_hide");
  });

  $("assist-form")?.addEventListener("submit", async (e) => {
    e.preventDefault();
    const question = input?.value.trim();
    if (!question) return;
    input!.value = "";
    await invoke("assist_ask", { question });
  });
});
