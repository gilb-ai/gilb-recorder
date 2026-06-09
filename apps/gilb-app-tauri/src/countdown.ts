import { invoke } from "@tauri-apps/api/core";
import { applyI18n, t } from "./i18n";

// Pre-record countdown popup. Created from Rust as a second OS window with
// `countdown.html?app=&meeting_id=&seconds=`. The Record button doubles as a
// progress bar: its fill animates 0->100% over `seconds`. Reaching 100% and an
// explicit Record click are the same outcome — both arm. Cancel/Esc backs out.
// Every path funnels through one `resolve_countdown` call.

const params = new URLSearchParams(window.location.search);
const appName = params.get("app") ?? t("capture.thisMeeting");
const meetingId = Number(params.get("meeting_id"));
const seconds = Number(params.get("seconds")) || 5;

const $ = <T extends HTMLElement = HTMLElement>(id: string) =>
  document.getElementById(id) as T | null;

// Guard so the auto-fire timer, a Record click, Cancel and Esc can never
// resolve the same popup twice.
let resolved = false;
let fillTimer: ReturnType<typeof setTimeout> | undefined;

async function resolve(armed: boolean) {
  if (resolved) return;
  resolved = true;
  if (fillTimer !== undefined) clearTimeout(fillTimer);
  try {
    await invoke("resolve_countdown", { meetingId, armed });
  } catch (err) {
    // The window is torn down by the backend on success; a failure here only
    // leaves it open, so surface it for the manual smoke and re-allow retry.
    console.error("resolve_countdown failed", err);
    resolved = false;
  }
}

window.addEventListener("DOMContentLoaded", () => {
  applyI18n();
  $("countdown-app")!.textContent = appName;

  const record = $<HTMLButtonElement>("btn-record");
  const fill = $("countdown-fill");
  if (record && fill) {
    fill.style.setProperty("--seconds", `${seconds}s`);
    // Kick the fill transition on the next frame so the 0%->100% width change
    // is actually animated rather than applied instantly.
    requestAnimationFrame(() => record.classList.add("filling"));
    record.addEventListener("click", () => resolve(true));
  }

  // Auto-arm when the fill completes, independent of the CSS transition firing.
  fillTimer = setTimeout(() => resolve(true), seconds * 1000);

  $("btn-cancel")?.addEventListener("click", () => resolve(false));
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") resolve(false);
  });
});
