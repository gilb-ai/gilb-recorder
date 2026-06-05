import { invoke } from "@tauri-apps/api/core";

// Stop-countdown popup, the mirror of countdown.ts. Created from Rust as a
// second OS window with `stop-countdown.html?app=&meeting_id=&seconds=` when a
// meeting ends. The Stop button doubles as a progress bar: its fill animates
// 0->100% over `seconds`. Reaching 100% and an explicit Stop click are the same
// outcome — both stop the recording. "Keep recording"/Esc backs out and leaves
// capture running (the meeting end was premature). Every path funnels through
// one `resolve_stop_countdown` call.

const params = new URLSearchParams(window.location.search);
const appName = params.get("app") ?? "this meeting";
const meetingId = Number(params.get("meeting_id"));
const seconds = Number(params.get("seconds")) || 5;

const $ = <T extends HTMLElement = HTMLElement>(id: string) =>
  document.getElementById(id) as T | null;

// Guard so the auto-fire timer, a Stop click, Keep and Esc can never resolve
// the same popup twice.
let resolved = false;
let fillTimer: ReturnType<typeof setTimeout> | undefined;

// `keep === false` stops the recording now; `keep === true` keeps it running.
async function resolve(keep: boolean) {
  if (resolved) return;
  resolved = true;
  if (fillTimer !== undefined) clearTimeout(fillTimer);
  try {
    await invoke("resolve_stop_countdown", { meetingId, keep });
  } catch (err) {
    // The window is torn down by the backend on success; a failure here only
    // leaves it open, so surface it for the manual smoke and re-allow retry.
    console.error("resolve_stop_countdown failed", err);
    resolved = false;
  }
}

window.addEventListener("DOMContentLoaded", () => {
  $("countdown-app")!.textContent = appName;

  const stop = $<HTMLButtonElement>("btn-stop");
  const fill = $("countdown-fill");
  if (stop && fill) {
    fill.style.setProperty("--seconds", `${seconds}s`);
    // Kick the fill transition on the next frame so the 0%->100% width change
    // is actually animated rather than applied instantly.
    requestAnimationFrame(() => stop.classList.add("filling"));
    stop.addEventListener("click", () => resolve(false));
  }

  // Auto-stop when the fill completes, independent of the CSS transition firing.
  fillTimer = setTimeout(() => resolve(false), seconds * 1000);

  $("btn-keep")?.addEventListener("click", () => resolve(true));
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") resolve(true);
  });
});
