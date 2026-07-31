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
// To request Accessibility now but defer capture to a later release, keep
// FEATURE_TRACKING on and set FEATURE_TRACKING_AUTOSTART off (see below).
const FEATURE_TRACKING = import.meta.env.VITE_FEATURE_TRACKING !== "0";
const FEATURE_TRANSCRIPTION = import.meta.env.VITE_FEATURE_TRANSCRIPTION !== "0";
// Set to "0" to drop the Settings screen: hide the footer gear and never open
// the overlay. For shells with nothing user-tunable to show.
const FEATURE_SETTINGS = import.meta.env.VITE_FEATURE_SETTINGS !== "0";
// Hide the tracking *UI* (status row + Pause/Resume) but keep the engine. A
// headless-tracking brand sets this to "0": capture still auto-starts (gated on
// FEATURE_TRACKING) and the Accessibility splash step stays, but the user sees
// no tracking surface. The row only shows when both the engine and its UI are on.
const FEATURE_TRACKING_UI = import.meta.env.VITE_FEATURE_TRACKING_UI !== "0";
const SHOW_TRACKING_UI = FEATURE_TRACKING && FEATURE_TRACKING_UI;
// Auto-start the capture engine on launch (default on). Decouples "request the
// Accessibility permission" from "actually capture": a build that wants the
// Accessibility grant in place up front but NOT to capture yet sets this to
// "0" while keeping FEATURE_TRACKING on. The Accessibility splash step still
// shows and is gated on, so the permission is requested/granted now; capture
// just never auto-arms. Re-enabling later (flip back to "1") needs no new
// permission prompt since the grant already exists.
const FEATURE_TRACKING_AUTOSTART =
  import.meta.env.VITE_FEATURE_TRACKING_AUTOSTART !== "0";
// Real-time meeting suggestions. The switch sits on the main window next to
// meeting detection — both are capture subsystems the user turns on and off —
// and the backend decides whether to offer it at all (a shell without the
// assist commands simply keeps the row hidden).
const FEATURE_ASSIST = import.meta.env.VITE_FEATURE_ASSIST !== "0";

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
// call after start/stop, listen("health")). Apply to the DOM
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

  // Activity tracking (subsystem A): a switch, with the state spelled out
  // underneath and the calm dot next to the label — the switch says what the
  // user asked for, the dot says what the engine is actually doing. Never
  // "recording". A meetings-only build hides the row and never auto-starts; a
  // headless-tracking build keeps the engine but hides the row (SHOW_TRACKING_UI).
  if (SHOW_TRACKING_UI) {
    tracking = s.recording;
    const dot = $("track-dot");
    if (dot) {
      dot.classList.toggle("on", tracking);
      dot.classList.toggle("paused", !tracking);
    }
    $("btn-track-toggle")?.setAttribute("aria-checked", tracking ? "true" : "false");
  }

  updateSplash(s.permissions, s.platform);

  // Auto-resume on launch only if the user hasn't deliberately paused, and
  // only once every required permission is granted. FEATURE_TRACKING_AUTOSTART
  // lets a build request the Accessibility permission (FEATURE_TRACKING) but
  // skip actually capturing until a later release flips it on.
  if (
    FEATURE_TRACKING &&
    FEATURE_TRACKING_AUTOSTART &&
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
  // Availability can follow the session in a product whose prompt and model
  // come from a server; in gilb it follows the agent CLI and this is a no-op
  // refresh. Cheap either way, and it keeps the switch honest right after a
  // sign-in or sign-out.
  if (FEATURE_ASSIST) refreshAssist();
}

// ----- real-time suggestions (settings) -----------------------------------

type AssistStatus = {
  /// Product-level availability, decided by the host (gilb_shell_tauri::assist).
  /// Gilb answers "the agent CLI is installed"; a hosted product would answer
  /// "signed in".
  available: boolean;
  model_ready: boolean;
  downloading: boolean;
  percent: number;
  enabled: boolean;
  /// What the user asked for, whether or not it can run yet.
  wanted: boolean;
  /// Why it cannot run, when `available` is false — the product's words.
  unavailable: string | null;
  /// Whether the panel may appear in screen recordings and shares.
  visible_in_capture: boolean;
  /// What the user can pick from. Empty when the product decides itself.
  agents: { id: string; label: string; installed: boolean }[];
  /// What they picked, if anything.
  agent: string | null;
  /// An agent is being installed right now.
  preparing: boolean;
};

function renderAssist(s: AssistStatus | null) {
  const row = $("assist-row");
  if (!row) return;
  // No status at all — a shell that does not ship the feature. Nothing to say.
  if (!s) {
    row.hidden = true;
    return;
  }
  row.hidden = false;

  const toggle = $<HTMLButtonElement>("toggle-assist");
  const progress = $("assist-progress");

  // Installing the agent the user just picked. The switch reads as on — they
  // asked for this — and stays put until it lands.
  if (s.preparing) {
    toggle?.setAttribute("aria-checked", s.wanted ? "true" : "false");
    if (toggle) toggle.disabled = true;
    // Indeterminate: npx tells us nothing we could turn into a percentage.
    progress?.classList.add("indeterminate");
    progress?.removeAttribute("hidden");
    renderAgentPicker(s, true);
    setText("assist-desc", t("assist.preparing"));
    return;
  }
  progress?.classList.remove("indeterminate");

  // Switched on, nothing chosen yet: this is the question, not an error. The
  // picker is the answer to it — never a dead end telling them to go install
  // something themselves.
  const needsChoice = !s.available && s.agents.length > 0;
  if (needsChoice) {
    // From `wanted`, not `enabled`: the switch shows what they asked for while
    // the setup step is outstanding.
    toggle?.setAttribute("aria-checked", s.wanted ? "true" : "false");
    if (toggle) toggle.disabled = false;
    progress?.setAttribute("hidden", "");
    renderAgentPicker(s, false);
  renderCaptureToggle(s);
    // "Choose an agent" is the wrong thing to say when there is nothing to
    // choose from — then the answer is which one to install.
    const anyInstalled = s.agents.some((a) => a.installed);
    setText(
      "assist-desc",
      anyInstalled ? t("assist.pickAgent") : (s.unavailable ?? t("assist.desc")),
    );
    return;
  }

  // Unavailable with nothing to pick — a product where the answer is
  // elsewhere (sign in). Say what is missing rather than hiding the control.
  if (!s.available) {
    toggle?.setAttribute("aria-checked", "false");
    if (toggle) toggle.disabled = true;
    progress?.setAttribute("hidden", "");
    renderAgentPicker(s, false);
    setText("assist-desc", s.unavailable ?? t("assist.desc"));
    return;
  }

  renderAgentPicker(s, false);
  if (toggle) toggle.disabled = false;
  const bar = $("assist-progress-bar");

  // While the model downloads the switch reads as on (the user asked for it)
  // but stays disabled — flipping it mid-download has nothing to act on.
  const on = s.enabled || s.downloading;
  toggle?.setAttribute("aria-checked", on ? "true" : "false");
  if (toggle) toggle.disabled = s.downloading;

  if (s.downloading) {
    progress?.removeAttribute("hidden");
    if (bar) bar.style.width = `${Math.min(100, Math.max(0, s.percent))}%`;
    setText("assist-desc", t("assist.descDownloading", { pct: s.percent }));
    return;
  }
  progress?.setAttribute("hidden", "");
  // Back to the static description. It does not change with the switch: a
  // label that rewrites itself as you flip it makes the control harder to
  // read, not easier — the switch already says which way it is.
  setText("assist-desc", t("assist.desc"));
}

/// The agent picker: one button per agent gilb knows, the current one marked,
/// the ones this machine lacks disabled.
///
/// It stays after the choice is made. A first-run wizard that disappears
/// leaves the user with a decision they cannot revisit — and this one is
/// "whose model hears my meetings", which is exactly the decision people
/// change their mind about. Switching re-installs and re-wires on the spot.
function renderAgentPicker(s: AssistStatus, busy: boolean) {
  const box = $("assist-agents");
  if (!box) return;
  if (s.agents.length === 0) {
    box.hidden = true;
    box.textContent = "";
    return;
  }
  box.textContent = "";
  for (const agent of s.agents) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "agent-option";
    btn.textContent = agent.installed
      ? agent.label
      : t("assist.agentMissing", { agent: agent.label });
    // Not installed is not "hidden": knowing Codex is an option you do not
    // have is worth more than not knowing Codex is an option.
    btn.disabled = busy || !agent.installed;
    if (s.agent === agent.id) btn.classList.add("chosen");
    btn.addEventListener("click", () => chooseAgent(agent.id));
    box.appendChild(btn);
  }
  box.hidden = false;
}

async function chooseAgent(id: string) {
  try {
    await invoke("assist_choose_agent", { agent: id });
  } catch (err) {
    setMessage(t("assist.error", { error: String(err) }), "error");
  }
  refreshAssist();
}

async function refreshAssist() {
  try {
    renderAssist(await invoke<AssistStatus>("assist_status"));
  } catch (err) {
    // A shell without the assist commands (or before they're registered):
    // keep the row hidden rather than showing a switch that does nothing.
    console.warn("assist_status failed", err);
    renderAssist(null);
  }
}

// Turning on without the model starts its download; the backend pushes
// progress and the final state back as `assist-status`.
async function toggleAssist() {
  const toggle = $<HTMLButtonElement>("toggle-assist");
  const on = toggle?.getAttribute("aria-checked") === "true";
  if (toggle) toggle.disabled = true;
  try {
    await invoke("assist_set_enabled", { on: !on });
  } catch (err) {
    setMessage(t("assist.error", { error: String(err) }), "error");
  }
  if (toggle) toggle.disabled = false;
  refreshAssist();
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

// Settings open as a modal overlay inside the main window — no second OS
// window. Open loads the persisted state fresh; Save persists, Cancel discards.
// What is left in here is transcription: a model to download and a language to
// pick, which is editing, not switching.
async function openSettings() {
  const overlay = $("settings-overlay");
  if (!overlay) return;
  // Only edited values live in here — the capture switches are on the main
  // window, where they apply on the spot.
  if (FEATURE_TRANSCRIPTION) await loadTranscription();
  // Async on purpose: the first ask starts the agent, and the overlay should
  // open now, not in three seconds.
  if (FEATURE_ASSIST) void loadAssistOptions();
  overlay.hidden = false;
  $<HTMLButtonElement>("btn-settings-save")?.focus();
}

// Surface a specific screen when the tray asks (`tray-navigate`, emitted by
// shells whose UI lives in the tray). The shell has already shown the window; we only pick
// the view. Any open settings overlay is closed first so it can't hide the
// requested screen. Unknown targets are ignored.
function navigateTray(target: string) {
  const overlay = $("settings-overlay");
  switch (target) {
    case "settings":
      if (FEATURE_SETTINGS) void openSettings();
      break;
    case "permissions": {
      if (overlay && !overlay.hidden) void closeSettings(false);
      // Force the permissions splash up even if everything is granted — the
      // user explicitly asked to check.
      const splash = $("splash");
      if (splash) splash.hidden = false;
      break;
    }
    case "login": {
      if (overlay && !overlay.hidden) void closeSettings(false);
      $<HTMLButtonElement>("btn-connect")?.focus();
      break;
    }
    default:
      break;
  }
}

async function closeSettings(save: boolean) {
  const overlay = $("settings-overlay");
  const lang = $<HTMLSelectElement>("select-language");
  if (save) {
    // The model download/delete are immediate; only the language is part of
    // Save/Cancel. Persist it only when it actually changed.
    if (lang && lang.value !== settingsLangSnapshot) {
      try {
        await invoke("set_transcription_language", { language: lang.value });
      } catch (e) {
        console.warn("set_transcription_language failed", e);
      }
    }
    const model = $<HTMLSelectElement>("select-assist-model");
    const effort = $<HTMLSelectElement>("select-assist-effort");
    for (const [el, key, configId] of [
      [model, "model", "model"],
      [effort, "effort", "effort"],
    ] as const) {
      if (el && el.value !== assistOptSnapshot[key]) {
        try {
          await invoke("assist_set_session_option", {
            configId,
            value: el.value,
          });
        } catch (e) {
          console.warn("assist_set_session_option failed", e);
        }
      }
    }
  } else {
    if (lang) lang.value = settingsLangSnapshot; // Cancel: revert
    const model = $<HTMLSelectElement>("select-assist-model");
    const effort = $<HTMLSelectElement>("select-assist-effort");
    if (model) model.value = assistOptSnapshot.model;
    if (effort) effort.value = assistOptSnapshot.effort;
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
// Pre-open snapshot of the suggestion-session knobs ("" = agent default), so
// Cancel reverts and Save only persists what actually changed.
let assistOptSnapshot = { model: "", effort: "" };

type SessionOptionsPayload = {
  options: {
    id: string;
    name: string;
    agent_default: string;
    choices: { value: string; label: string }[];
  }[];
  model: string | null;
  effort: string | null;
};

/// The "show it on screen shares" switch in Settings.
///
/// Hidden until the feature is actually usable: a switch that governs the
/// visibility of a panel the user cannot yet open explains nothing. Flipping
/// it takes effect immediately, on a panel that is already open — this is a
/// setting people reach for mid-demo.
function renderCaptureToggle(s: AssistStatus) {
  const row = $("assist-capture-row");
  const toggle = $<HTMLButtonElement>("toggle-assist-capture");
  if (!row || !toggle) return;
  row.hidden = !s.available;
  toggle.setAttribute("aria-checked", s.visible_in_capture ? "true" : "false");
}

function initCaptureToggle() {
  const toggle = $<HTMLButtonElement>("toggle-assist-capture");
  if (!toggle) return;
  toggle.addEventListener("click", async () => {
    const was = toggle.getAttribute("aria-checked") === "true";
    const on = !was;
    toggle.setAttribute("aria-checked", on ? "true" : "false");
    toggle.disabled = true;
    try {
      await invoke("assist_set_visible_in_capture", { on });
    } catch (e) {
      console.warn("assist_set_visible_in_capture failed", e);
      // Put it back rather than lie about what the other side can see.
      toggle.setAttribute("aria-checked", was ? "true" : "false");
    } finally {
      toggle.disabled = false;
    }
  });
}

/// The suggestions-model row in Settings. The list is the agent's own,
/// fetched over ACP (and cached backend-side), so opening the screen is what
/// asks the question — a row that guessed at model names would be wrong the
/// day the agent updates.
async function loadAssistOptions() {
  const row = $("assist-model-row");
  if (!row) return;
  // The first ask starts the agent — seconds, not milliseconds. An empty
  // select for that long reads as broken, so say what is happening: the row
  // shows immediately with a disabled "Asking the agent…" placeholder, and
  // the real choices replace it when they land.
  const placeholder = (selectId: string) => {
    const select = $<HTMLSelectElement>(selectId);
    if (!select) return;
    select.textContent = "";
    const el = document.createElement("option");
    el.value = "";
    el.textContent = t("assist.optionsLoading");
    select.appendChild(el);
    select.disabled = true;
  };
  placeholder("select-assist-model");
  $("assist-effort-wrap")?.setAttribute("hidden", "");
  row.setAttribute("aria-busy", "true");
  row.hidden = false;
  let payload: SessionOptionsPayload;
  try {
    payload = await invoke<SessionOptionsPayload>("assist_session_options");
  } catch (e) {
    // No agent set up (or it failed to answer): nothing to configure.
    console.warn("assist_session_options failed", e);
    row.hidden = true;
    row.removeAttribute("aria-busy");
    return;
  }
  row.removeAttribute("aria-busy");
  const fill = (
    selectId: string,
    optionId: string,
    chosen: string | null,
  ): boolean => {
    const select = $<HTMLSelectElement>(selectId);
    const option = payload.options.find((o) => o.id === optionId);
    if (!select || !option || option.choices.length === 0) return false;
    select.textContent = "";
    const def = document.createElement("option");
    def.value = "";
    def.textContent = t("assist.agentDefault", { value: option.agent_default });
    select.appendChild(def);
    for (const c of option.choices) {
      const el = document.createElement("option");
      el.value = c.value;
      el.textContent = c.label;
      select.appendChild(el);
    }
    select.value = chosen ?? "";
    select.disabled = false;
    return true;
  };
  const hasModel = fill("select-assist-model", "model", payload.model);
  const hasEffort = fill("select-assist-effort", "effort", payload.effort);
  const effortWrap = $("assist-effort-wrap");
  if (effortWrap) effortWrap.hidden = !hasEffort;
  row.hidden = !hasModel;
  assistOptSnapshot = { model: payload.model ?? "", effort: payload.effort ?? "" };
}
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
/// Meeting detection: a switch on the main window, applied the moment it is
/// flipped. No Save step — the effect (the detector starting or stopping) is
/// immediate and visible, so a pending "unsaved" state would only be a way to
/// be wrong about what the app is doing. If the backend refuses, the switch
/// goes back where it was rather than lying about what is on.
function initMeetingToggle() {
  const toggle = $<HTMLButtonElement>("toggle-meeting");
  if (!toggle) return;
  void refreshMeetingToggle();
  toggle.addEventListener("click", async () => {
    const was = toggle.getAttribute("aria-checked") === "true";
    const enabled = !was;
    toggle.setAttribute("aria-checked", enabled ? "true" : "false");
    toggle.disabled = true;
    try {
      await invoke("set_meeting_detection", { enabled });
    } catch (e) {
      console.warn("set_meeting_detection failed", e);
      toggle.setAttribute("aria-checked", was ? "true" : "false");
      setMessage(t("settings.meetingFailed"), "error");
    } finally {
      toggle.disabled = false;
    }
  });
}

async function refreshMeetingToggle() {
  const toggle = $<HTMLButtonElement>("toggle-meeting");
  if (!toggle) return;
  try {
    const on = await invoke<boolean>("get_meeting_detection");
    toggle.setAttribute("aria-checked", on ? "true" : "false");
  } catch (e) {
    console.warn("get_meeting_detection failed", e);
  }
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

// The activity-tracking switch. Persists the choice so it survives restarts —
// a deliberate pause is never silently undone by the next launch. The switch
// moves optimistically and `refresh()` has the final word: if the engine
// refused to start or stop, the next status snapshot puts it back.
async function toggleTracking() {
  const btn = $<HTMLButtonElement>("btn-track-toggle");
  if (btn) {
    btn.disabled = true;
    btn.setAttribute("aria-checked", tracking ? "false" : "true");
  }
  // No "…paused" / "…on" message: the switch and the line under it already
  // say the state, and a status line that stays on screen for the rest of the
  // session reads as a problem long after it stopped being news. Errors still
  // speak up — those the user has not already seen.
  if (tracking) {
    if (await applyStop()) await persistPaused(true);
  } else {
    if (await applyStart()) await persistPaused(false);
  }
  setMessage("");
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
    $("splash-step-ax")?.setAttribute("hidden", "");
  }
  if (!SHOW_TRACKING_UI) {
    $("track-row")?.setAttribute("hidden", "");
  }
  if (!FEATURE_TRACKING_UI) {
    // Engine still runs; only the tracking surface goes. Drop the "Capture"
    // caption and let the card collapse when it has nothing live to show (no
    // meeting recording, no message) — see `.capture-ui-hidden` in styles.css.
    $("capture-head")?.setAttribute("hidden", "");
    document.querySelector(".card.capture")?.classList.add("capture-ui-hidden");
  }
  if (!FEATURE_TRANSCRIPTION) {
    $("transcription-row")?.setAttribute("hidden", "");
  }
  if (!FEATURE_SETTINGS) {
    // No Settings screen: hide the footer gear so there's nothing to open.
    $("btn-settings")?.setAttribute("hidden", "");
  }
  $("btn-track-toggle")?.addEventListener("click", toggleTracking);
  $("btn-rec-stop")?.addEventListener("click", stopMeetingRecording);
  $("btn-connect")?.addEventListener("click", connect);
  $("btn-signout")?.addEventListener("click", signOut);
  if (FEATURE_ASSIST) {
    $("toggle-assist")?.addEventListener("click", toggleAssist);
    // Model download progress, sign-in/out and the pipeline coming up or down.
    listen<AssistStatus>("assist-status", (e) => renderAssist(e.payload));
  }
  if (FEATURE_SETTINGS) {
    $("btn-settings")?.addEventListener("click", openSettings);
  }
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
  initCaptureToggle();

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

  // Backend proxies EventBus events here — a health message refreshes the UI
  // immediately instead of waiting for the poll. Permission grants have no
  // event: they change in System Settings, outside our process, so the
  // 5-second poll is what notices them.
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
  // Clear the "continue in your browser" message with the outcome. On success
  // clear it entirely — the signed-in card already shows the workspace + account,
  // so repeating "connected" in the status line just duplicates it.
  listen<AuthStatus>("auth", (e) => {
    setMessage(e.payload?.signed_in ? "" : t("auth.signInFailed"), "error");
    refreshAuth();
  });
  // Tray items ask to surface a specific screen.
  listen<string>("tray-navigate", (e) => navigateTray(e.payload));

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
