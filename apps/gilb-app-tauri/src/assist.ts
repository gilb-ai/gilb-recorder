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
// Commands: assist_ask(question), assist_hide.

const $ = <T extends HTMLElement = HTMLElement>(id: string) =>
  document.getElementById(id) as T | null;

/// Model output is untrusted markdown: render with marked, sanitize the HTML.
function render(md: string): string {
  return DOMPurify.sanitize(marked.parse(md, { async: false }));
}

const responses: string[] = [];
let shown = -1;

/// Counter, arrow states and the "there are newer ones" mark. The whole block
/// stays hidden until the first suggestion — an empty "‹ ›" in a fresh panel
/// explains nothing.
function renderNav() {
  const nav = $("assist-nav");
  const pos = $("assist-pos");
  const prev = $<HTMLButtonElement>("assist-prev");
  const next = $<HTMLButtonElement>("assist-next");
  const total = responses.length;
  if (nav) nav.hidden = total === 0;
  if (pos) pos.textContent = total === 0 ? "" : `${shown + 1}/${total}`;
  // Disabled at the ends: an arrow that silently does nothing reads as broken.
  if (prev) prev.disabled = shown <= 0;
  if (next) next.disabled = shown >= total - 1;
  // Unread suggestions sit past the one on screen.
  next?.classList.toggle("has-new", shown < total - 1);
}

function show(idx: number) {
  const content = $("assist-content");
  if (!content || idx < 0 || idx >= responses.length) return;
  shown = idx;
  content.innerHTML = render(responses[idx]);
  content.scrollTop = 0;
  renderNav();
}

window.addEventListener("DOMContentLoaded", async () => {
  applyI18n();
  const input = $<HTMLInputElement>("assist-input");
  if (input) input.placeholder = t("assist.askPlaceholder");

  renderNav();

  await listen<{ text: string }>("assist://update", (e) => {
    // Follow the newest one only while the newest is what's on screen;
    // otherwise reading an older suggestion would be interrupted by every
    // arrival. The counter and the lit ›-arrow say more is waiting.
    const wasAtLatest = shown === responses.length - 1;
    responses.push(e.payload.text);
    if (wasAtLatest) show(responses.length - 1);
    else renderNav();
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

  // No click-through toggle: a window that ignores the mouse also ignores the
  // button that would turn it back on, so the panel became inert until the app
  // restarted. Dropped rather than papered over.
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
