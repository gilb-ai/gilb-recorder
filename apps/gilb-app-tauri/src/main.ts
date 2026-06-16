import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { enable as enableAutostart, isEnabled as isAutostartEnabled } from "@tauri-apps/plugin-autostart";
import { check } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { getVersion } from "@tauri-apps/api/app";
import { applyI18n, t } from "./i18n";

// How often to poll for updates while the app is running.
const UPDATE_CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000; // 6h

// Default gilb-web workspace URL prefilled in the Connect field, baked in per
// build: dev builds point at the local server, release builds at the hosted
// workspace. Set via Vite env (.env.development / .env.production); falls back
// to localhost in dev and an empty field in release if unset.
const DEFAULT_WORKSPACE_URL =
  import.meta.env.VITE_GILB_WEB_URL ??
  (import.meta.env.DEV ? "http://localhost:3000" : "");

// Build-time feature switches (default on; set to "0" to drop a subsystem).
// A meetings-only shell hides activity tracking (and its Accessibility splash
// step) and/or the transcription section, and never auto-starts the engine.
const FEATURE_TRACKING = import.meta.env.VITE_FEATURE_TRACKING !== "0";
const FEATURE_TRANSCRIPTION = import.meta.env.VITE_FEATURE_TRANSCRIPTION !== "0";

// Set once per launch after the first successful `start_capture` (manual or
// auto). Prevents the refresh loop from re-arming a recording the user
// explicitly stopped.
let hasAutoStarted = false;
// Last-known activity-tracking state (engine `recording`), so the Pause/Resume
// button knows which way to flip without re-fetching.
let tracking = false;
// Persisted pause flag, loaded on launch. When true, the app does NOT auto-resume
// tracking — a deliberate pause survives restarts (subsystem A, see ui-design.md).
let trackingPaused = false;

type Permissions = {
  accessibility: boolean;
  // Reported by the backend but no longer gated on — Apple's
  // CGPreflightListenEventAccess (and the underlying CGEventTap) honour
  // the Accessibility grant, so once AX is on the recorder has what it
  // needs. We leave the field on the type for protocol compatibility.
  input_monitoring: boolean;
  // Screen Recording + Microphone: required by the meeting recorder
  // (ScreenCaptureKit video + cpal mic). Gated on alongside Accessibility.
  screen_recording: boolean;
  microphone: boolean;
};

// Privacy panes the splash can open, in display order.
type PrivacyPane = "accessibility" | "screen-recording" | "microphone";

type EngineStatus = {
  recording: boolean;
  session_id: number | null;
  permissions: Permissions;
  platform: string;
};

type AuthStatus = {
  signed_in: boolean;
  employee: string | null;
  gilb_web_url: string | null;
};

// Emitted by the meeting bridge (`meeting-recording`) when a meeting capture
// arms or stops. `app`/`started_at_ms` are set while recording, null otherwise.
type MeetingRecording = {
  recording: boolean;
  meeting_id: number | null;
  app: string | null;
  started_at_ms: number | null;
};

const $ = <T extends HTMLElement = HTMLElement>(id: string) =>
  document.getElementById(id) as T | null;

function setDot(id: string, ok: boolean) {
  const el = $(id);
  if (!el) return;
  el.classList.toggle("ok", ok);
  el.classList.toggle("warn", !ok);
}

function setText(id: string, text: string) {
  const el = $(id);
  if (el) el.textContent = text;
}

function setMessage(text: string, kind: "info" | "error" = "info") {
  const el = $("message");
  if (!el) return;
  el.textContent = text;
  el.dataset.kind = kind;
}

// "Recording a meeting now" indicator: a red pill with the meeting app name
// and a live elapsed timer, driven by the `meeting-recording` event.
let recTimer: ReturnType<typeof setInterval> | undefined;
let recStartMs: number | null = null;
// Meeting id of the active recording, so the indicator's Stop button can
// target it; null when nothing is recording.
let recMeetingId: number | null = null;

function fmtElapsed(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const mm = String(Math.floor(total / 60)).padStart(2, "0");
  const ss = String(total % 60).padStart(2, "0");
  return `${mm}:${ss}`;
}

function tickRecTimer() {
  if (recStartMs === null) return;
  setText("rec-timer", fmtElapsed(Date.now() - recStartMs));
}

function updateRecIndicator(st: MeetingRecording | undefined) {
  const el = $("rec-indicator");
  if (!el) return;
  if (st?.recording) {
    recMeetingId = st.meeting_id;
    const stopBtn = $<HTMLButtonElement>("btn-rec-stop");
    if (stopBtn) stopBtn.disabled = false;
    // A detected meeting carries its app name; a manual recording has none
    // (the pipeline sends app=null), so it gets a standalone "recording
    // screen" label instead of the "recording meeting — {app}" form.
    const prefix = $("rec-prefix");
    if (st.app) {
      if (prefix) prefix.textContent = t("capture.recordingMeeting");
      setText("rec-app", st.app);
    } else {
      if (prefix) prefix.textContent = t("capture.recordingManual");
      setText("rec-app", "");
    }
    recStartMs = st.started_at_ms ?? Date.now();
    tickRecTimer();
    el.hidden = false;
    if (recTimer === undefined) recTimer = setInterval(tickRecTimer, 1000);
  } else {
    el.hidden = true;
    recMeetingId = null;
    recStartMs = null;
    if (recTimer !== undefined) {
      clearInterval(recTimer);
      recTimer = undefined;
    }
  }
}

async function stopMeetingRecording() {
  if (recMeetingId === null) return;
  const btn = $<HTMLButtonElement>("btn-rec-stop");
  if (btn) btn.disabled = true; // re-enabled implicitly when the pill hides
  try {
    await invoke("stop_meeting_recording", { meetingId: recMeetingId });
  } catch (err) {
    setMessage(t("capture.stopError", { error: String(err) }), "error");
    if (btn) btn.disabled = false;
  }
}

// The macOS TCC permissions this build needs, all required before capture
// starts. Accessibility is only needed by activity tracking, so a
// meetings-only build drops that step (its <li> is hidden on load too).
const SPLASH_STEPS: {
  key: "accessibility" | "screen_recording" | "microphone";
  dot: string;
  status: string;
  step: string;
}[] = [
  ...(FEATURE_TRACKING
    ? [
        {
          key: "accessibility" as const,
          dot: "splash-ax-dot",
          status: "splash-ax-status",
          step: "splash-step-ax",
        },
      ]
    : []),
  {
    key: "screen_recording",
    dot: "splash-screen-dot",
    status: "splash-screen-status",
    step: "splash-step-screen",
  },
  {
    key: "microphone",
    dot: "splash-mic-dot",
    status: "splash-mic-status",
    step: "splash-step-mic",
  },
];

// True once every macOS permission the app needs is granted. On non-macOS
// platforms there is no TCC gate, so the backend reports them all granted.
function allPermissionsGranted(perms: Permissions): boolean {
  return SPLASH_STEPS.every(({ key }) => perms[key]);
}

function updateSplash(perms: Permissions, platform: string) {
  const splash = $("splash");
  if (!splash) return;

  // Splash only makes sense on macOS; do nothing on other platforms.
  const macOnly = platform === "macos";
  const visible = macOnly && !allPermissionsGranted(perms);

  splash.hidden = !visible;
  if (!visible) return;

  for (const { key, dot, status, step } of SPLASH_STEPS) {
    const granted = perms[key];
    setDot(dot, granted);
    setText(status, granted ? t("splash.granted") : t("splash.notGranted"));
    const el = $(step);
    if (el) el.classList.toggle("granted", granted);
  }
}

// Sequence counter — refresh() calls can race in parallel (poll, explicit
// call after start/stop, listen("permission"/"health")). Apply to the DOM
// only the result of the most recently started call — out-of-order
// responses are discarded.
let refreshSeq = 0;

async function refresh() {
  const mySeq = ++refreshSeq;
  let s: EngineStatus;
  try {
    s = await invoke<EngineStatus>("status");
  } catch (err) {
    if (mySeq !== refreshSeq) return;
    setMessage(t("capture.statusError", { error: String(err) }), "error");
    return;
  }
  if (mySeq !== refreshSeq) return;

  // Activity tracking (subsystem A): calm status + Pause/Resume. Never
  // "recording". A meetings-only build hides the row and never auto-starts.
  if (FEATURE_TRACKING) {
    tracking = s.recording;
    setText("track-label", tracking ? t("capture.trackingOn") : t("capture.trackingPaused"));
    const dot = $("track-dot");
    if (dot) {
      dot.classList.toggle("on", tracking);
      dot.classList.toggle("paused", !tracking);
    }
    const toggleBtn = $<HTMLButtonElement>("btn-track-toggle");
    if (toggleBtn) toggleBtn.textContent = tracking ? t("capture.pause") : t("capture.resume");
  }

  updateSplash(s.permissions, s.platform);

  // Auto-resume on launch only if the user hasn't deliberately paused, and
  // only once every required permission is granted.
  if (
    FEATURE_TRACKING &&
    !hasAutoStarted &&
    !s.recording &&
    allPermissionsGranted(s.permissions) &&
    !trackingPaused
  ) {
    hasAutoStarted = true;
    applyStart().then(() => refresh());
  } else if (s.recording) {
    hasAutoStarted = true;
  }
}

async function refreshAuth() {
  let s: AuthStatus;
  try {
    s = await invoke<AuthStatus>("auth_status");
  } catch (err) {
    console.warn("auth_status failed", err);
    return;
  }
  const signedOut = $("auth-signed-out");
  const signedIn = $("auth-signed-in");
  if (signedOut) signedOut.hidden = s.signed_in;
  if (signedIn) signedIn.hidden = !s.signed_in;
  if (s.signed_in) {
    setText("auth-employee", s.employee ?? t("auth.thisDevice"));
    setText("auth-ws-url", s.gilb_web_url ?? "");
  }
}

async function connect() {
  // URL is baked in per build (DEFAULT_WORKSPACE_URL); the backend may further
  // override it via the GILB_WEB_URL env var. No field to read.
  try {
    await invoke("start_login", { gilbWebUrl: DEFAULT_WORKSPACE_URL });
    setMessage(t("auth.continueInBrowser"));
  } catch (err) {
    setMessage(t("auth.signInError", { error: String(err) }), "error");
  }
}

async function signOut() {
  try {
    await invoke("sign_out");
    setMessage(t("auth.signedOut"));
  } catch (err) {
    setMessage(t("auth.signOutError", { error: String(err) }), "error");
  }
  refreshAuth();
}

// Pre-open snapshot of the meeting toggle, so Cancel can revert it without
// persisting (Save is the only path that persists + applies).
let settingsToggleSnapshot = false;

// Settings open as a modal overlay inside the main window — no second OS
// window. Open loads the persisted state fresh; Save persists, Cancel discards.
async function openSettings() {
  const overlay = $("settings-overlay");
  const toggle = $<HTMLButtonElement>("toggle-meeting");
  if (!overlay) return;
  if (toggle) {
    try {
      const on = await invoke<boolean>("get_meeting_detection");
      toggle.setAttribute("aria-checked", on ? "true" : "false");
    } catch (e) {
      console.warn("get_meeting_detection failed", e);
    }
  }
  settingsToggleSnapshot = toggle?.getAttribute("aria-checked") === "true";
  if (FEATURE_TRANSCRIPTION) await loadTranscription();
  overlay.hidden = false;
  $<HTMLButtonElement>("btn-settings-save")?.focus();
}

async function closeSettings(save: boolean) {
  const overlay = $("settings-overlay");
  const toggle = $<HTMLButtonElement>("toggle-meeting");
  const lang = $<HTMLSelectElement>("select-language");
  if (save) {
    const enabled = toggle?.getAttribute("aria-checked") === "true";
    try {
      await invoke("set_meeting_detection", { enabled });
    } catch (e) {
      console.warn("set_meeting_detection failed", e);
    }
    // The model download/delete are immediate; only the language is part of
    // Save/Cancel. Persist it only when it actually changed.
    if (lang && lang.value !== settingsLangSnapshot) {
      try {
        await invoke("set_transcription_language", { language: lang.value });
      } catch (e) {
        console.warn("set_transcription_language failed", e);
      }
    }
  } else {
    if (toggle) {
      // Cancel: revert the toggle to its pre-open state (nothing persisted).
      toggle.setAttribute("aria-checked", settingsToggleSnapshot ? "true" : "false");
    }
    if (lang) lang.value = settingsLangSnapshot; // revert the language picker
  }
  if (overlay) overlay.hidden = true;
}

// ----- transcription model (settings) -------------------------------------

interface TranscriptionStatus {
  model_downloaded: boolean;
  model_bytes: number;
  language: string;
}
interface ModelProgress {
  status: "progress" | "done" | "cancelled" | "error";
  downloaded: number;
  total: number;
  error?: string;
}

let settingsLangSnapshot = "auto";
// Tracks an in-flight download so reopening Settings keeps showing progress
// (the backend exposes no "downloading" status — only presence of the model).
let modelDownloading = false;

function setModelStatus(text: string, kind?: "ok" | "warn" | "error") {
  const el = $("model-status");
  if (!el) return;
  el.textContent = text;
  if (kind) el.setAttribute("data-kind", kind);
  else el.removeAttribute("data-kind");
}

// Reflect model state in the buttons + status. Downloading wins over presence.
function renderModelState(downloaded: boolean) {
  const dl = $("btn-model-download");
  const cancel = $("btn-model-cancel");
  const del = $("btn-model-delete");
  const prog = $("model-progress");
  if (modelDownloading) {
    dl?.setAttribute("hidden", "");
    del?.setAttribute("hidden", "");
    cancel?.removeAttribute("hidden");
    prog?.removeAttribute("hidden");
    return;
  }
  prog?.setAttribute("hidden", "");
  cancel?.setAttribute("hidden", "");
  if (downloaded) {
    dl?.setAttribute("hidden", "");
    del?.removeAttribute("hidden");
    setModelStatus(t("model.ready"), "ok");
  } else {
    del?.setAttribute("hidden", "");
    dl?.removeAttribute("hidden");
    if (dl) dl.textContent = t("model.download");
    setModelStatus(t("model.notDownloaded"));
  }
}

async function loadTranscription() {
  try {
    const s = await invoke<TranscriptionStatus>("get_transcription_status");
    settingsLangSnapshot = s.language;
    const sel = $<HTMLSelectElement>("select-language");
    if (sel) sel.value = s.language;
    renderModelState(s.model_downloaded);
  } catch (e) {
    console.warn("get_transcription_status failed", e);
  }
}

function setProgressBar(downloaded: number, total: number) {
  const bar = $("model-progress-bar");
  const pct = total > 0 ? Math.min(100, Math.round((downloaded / total) * 100)) : 0;
  if (bar) bar.style.width = `${pct}%`;
  setModelStatus(total > 0 ? t("model.downloadingPct", { pct }) : t("model.downloading"));
}

async function downloadModel() {
  modelDownloading = true;
  setProgressBar(0, 0);
  renderModelState(false);
  // Resolves only when the download finishes; progress + terminal state arrive
  // via the `model-download` event, so we don't block the UI on the result.
  invoke("download_model").catch((e) => console.warn("download_model failed", e));
}

async function cancelModelDownload() {
  try {
    await invoke("cancel_model_download");
  } catch (e) {
    console.warn("cancel_model_download failed", e);
  }
}

async function deleteModel() {
  try {
    await invoke("delete_model");
  } catch (e) {
    console.warn("delete_model failed", e);
  }
  modelDownloading = false;
  renderModelState(false);
}

// Flips the toggle's visual state; the value is persisted/applied on Save.
function initMeetingToggle() {
  const toggle = $<HTMLButtonElement>("toggle-meeting");
  if (!toggle) return;
  toggle.addEventListener("click", () => {
    const on = toggle.getAttribute("aria-checked") === "true";
    toggle.setAttribute("aria-checked", on ? "false" : "true");
  });
}

async function openPrivacyPane(pane: PrivacyPane) {
  try {
    await invoke("open_privacy_pane", { pane });
  } catch (err) {
    setMessage(`open_privacy_pane: ${String(err)}`, "error");
  }
}

// Start/stop the engine capture without touching the persisted pause flag —
// used for the auto-resume path and as primitives for the user toggle.
async function applyStart(): Promise<boolean> {
  try {
    await invoke<number>("start_capture");
    return true;
  } catch (err) {
    setMessage(t("capture.cantResume", { error: String(err) }), "error");
    return false;
  }
}

async function applyStop(): Promise<boolean> {
  try {
    await invoke("stop_capture");
    return true;
  } catch (err) {
    setMessage(t("capture.cantPause", { error: String(err) }), "error");
    return false;
  }
}

async function persistPaused(paused: boolean) {
  trackingPaused = paused;
  try {
    await invoke("set_tracking_paused", { paused });
  } catch (err) {
    console.warn("set_tracking_paused failed", err);
  }
}

// User Pause/Resume. Persists the choice so it survives restarts.
async function toggleTracking() {
  const btn = $<HTMLButtonElement>("btn-track-toggle");
  if (btn) btn.disabled = true;
  if (tracking) {
    if (await applyStop()) {
      await persistPaused(true);
      setMessage(t("capture.trackingPausedMsg"));
    }
  } else {
    if (await applyStart()) {
      await persistPaused(false);
      setMessage(t("capture.trackingOnMsg"));
    }
  }
  if (btn) btn.disabled = false;
  refresh();
}

// Silent auto-update: check, install, and relaunch with no prompt. Before
// relaunching we stop any active recording so the session is flushed and its
// row closed cleanly (the installer / relaunch would otherwise kill the app
// mid-write). The relaunched build resumes capture via the autostart flow.
let updateInProgress = false;
async function checkForUpdates() {
  if (updateInProgress) return;
  // Never install + relaunch while a meeting is being recorded — that would
  // kill the capture mid-write (unfinalized video, recording lost). The next
  // periodic check (or the next launch) picks the update up instead.
  if (recMeetingId !== null) return;
  let update;
  try {
    update = await check();
  } catch (err) {
    console.warn("update check failed", err);
    return;
  }
  if (!update) return;

  updateInProgress = true;
  try {
    try {
      const s = await invoke<EngineStatus>("status");
      if (s.recording) await invoke("stop_capture");
    } catch (err) {
      console.warn("pre-update stop_capture failed", err);
    }
    setMessage(t("update.installing", { version: update.version }));
    await update.downloadAndInstall();
    await relaunch();
  } catch (err) {
    updateInProgress = false;
    console.warn("update install failed", err);
  }
}

window.addEventListener("DOMContentLoaded", () => {
  applyI18n();
  if (!FEATURE_TRACKING) {
    $("track-row")?.setAttribute("hidden", "");
    $("splash-step-ax")?.setAttribute("hidden", "");
  }
  if (!FEATURE_TRANSCRIPTION) {
    $("transcription-row")?.setAttribute("hidden", "");
  }
  $("btn-track-toggle")?.addEventListener("click", toggleTracking);
  $("btn-rec-stop")?.addEventListener("click", stopMeetingRecording);
  $("btn-connect")?.addEventListener("click", connect);
  $("btn-signout")?.addEventListener("click", signOut);
  $("btn-settings")?.addEventListener("click", openSettings);
  $("btn-settings-save")?.addEventListener("click", () => closeSettings(true));
  $("btn-settings-cancel")?.addEventListener("click", () => closeSettings(false));
  $("btn-model-download")?.addEventListener("click", downloadModel);
  $("btn-model-cancel")?.addEventListener("click", cancelModelDownload);
  $("btn-model-delete")?.addEventListener("click", deleteModel);
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      const ov = $("settings-overlay");
      if (ov && !ov.hidden) closeSettings(false);
    }
  });
  initMeetingToggle();

  for (const btn of document.querySelectorAll<HTMLButtonElement>(".splash-btn")) {
    btn.addEventListener("click", () => {
      const pane = btn.dataset.pane;
      if (
        pane === "accessibility" ||
        pane === "screen-recording" ||
        pane === "microphone"
      ) {
        openPrivacyPane(pane);
      }
    });
  }

  // Backend proxies EventBus events here — permission/health messages
  // trigger refresh() immediately. Permissions, recording state and
  // session_id update via events; the slow 5-second poll is only a fallback
  // for a missed broadcast.
  listen("permission", () => refresh());
  listen("health", () => refresh());
  // Local model download progress + terminal state (driven by download_model).
  listen<ModelProgress>("model-download", (e) => {
    const p = e.payload;
    if (p.status === "progress") {
      setProgressBar(p.downloaded, p.total);
      return;
    }
    modelDownloading = false;
    if (p.status === "done") {
      renderModelState(true);
    } else if (p.status === "cancelled") {
      renderModelState(false);
    } else {
      renderModelState(false);
      const btn = $("btn-model-download");
      if (btn) btn.textContent = t("model.retry");
      setModelStatus(
        p.error ? t("model.failedWith", { error: p.error }) : t("model.failed"),
        "error",
      );
    }
  });
  // Meeting capture arm/stop drives the in-app recording indicator.
  listen<MeetingRecording>("meeting-recording", (e) =>
    updateRecIndicator(e.payload),
  );
  // Backend emits `auth` after the gilb://auth/callback deep link is handled.
  // Clear the "continue in your browser" message with the outcome.
  listen<AuthStatus>("auth", (e) => {
    setMessage(
      e.payload?.signed_in ? t("auth.connectedLine") : t("auth.signInFailed"),
      e.payload?.signed_in ? "info" : "error",
    );
    refreshAuth();
  });

  // Register Gilb as a LaunchAgent on first run so it starts at login.
  // Idempotent — `enable()` is a no-op once the agent plist is in place.
  (async () => {
    try {
      if (!(await isAutostartEnabled())) {
        await enableAutostart();
      }
    } catch (err) {
      console.warn("autostart enable failed", err);
    }
  })();

  // Show the app version in the footer.
  getVersion()
    .then((v) => setText("app-version", t("app.version", { version: v })))
    .catch(() => {});

  // Load the persisted pause flag before the first status refresh so the
  // auto-resume decision honours a deliberate pause from a previous session.
  // Meetings-only shells don't register the tracking commands at all.
  (async () => {
    if (FEATURE_TRACKING) {
      try {
        trackingPaused = await invoke<boolean>("get_tracking_paused");
      } catch (err) {
        console.warn("get_tracking_paused failed", err);
      }
    }
    refresh();
  })();
  refreshAuth();
  setInterval(refresh, 5000);

  // Check for updates on launch and periodically.
  checkForUpdates();
  setInterval(checkForUpdates, UPDATE_CHECK_INTERVAL_MS);
});
