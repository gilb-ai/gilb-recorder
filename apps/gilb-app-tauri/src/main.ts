import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { enable as enableAutostart, isEnabled as isAutostartEnabled } from "@tauri-apps/plugin-autostart";
import { check } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { getVersion } from "@tauri-apps/api/app";

// How often to poll for updates while the app is running.
const UPDATE_CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000; // 6h

// Set once per launch after the first successful `start_capture` (manual or
// auto). Prevents the refresh loop from re-arming a recording the user
// explicitly stopped.
let hasAutoStarted = false;

type Permissions = {
  accessibility: boolean;
  // Reported by the backend but no longer gated on — Apple's
  // CGPreflightListenEventAccess (and the underlying CGEventTap) honour
  // the Accessibility grant, so once AX is on the recorder has what it
  // needs. We leave the field on the type for protocol compatibility.
  input_monitoring: boolean;
};

type EngineStatus = {
  recording: boolean;
  session_id: number | null;
  permissions: Permissions;
  actions_today: number;
  platform: string;
};

type AuthStatus = {
  signed_in: boolean;
  employee: string | null;
  gilb_web_url: string | null;
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

function updateSplash(perms: Permissions, platform: string) {
  const splash = $("splash");
  if (!splash) return;

  // Splash only makes sense on macOS; do nothing on other platforms.
  const macOnly = platform === "macos";
  const visible = macOnly && !perms.accessibility;

  splash.hidden = !visible;
  if (!visible) return;

  setDot("splash-ax-dot", perms.accessibility);
  setText("splash-ax-status", perms.accessibility ? "granted" : "not granted");

  const step = $("splash-step-ax");
  if (step) step.classList.toggle("granted", perms.accessibility);
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
    setMessage(`status error: ${String(err)}`, "error");
    return;
  }
  if (mySeq !== refreshSeq) return;

  const startBtn = $<HTMLButtonElement>("btn-start");
  const stopBtn = $<HTMLButtonElement>("btn-stop");
  if (startBtn) startBtn.disabled = s.recording;
  if (stopBtn) stopBtn.disabled = !s.recording;

  updateSplash(s.permissions, s.platform);

  if (!hasAutoStarted && !s.recording && s.permissions.accessibility) {
    hasAutoStarted = true;
    startCapture();
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
    setText("auth-employee", s.employee ?? "this device");
    setText("auth-url-display", s.gilb_web_url ?? "");
  }
}

async function connect() {
  const input = $<HTMLInputElement>("auth-url");
  const url = input?.value.trim();
  if (!url) {
    setMessage("Enter your gilb-web URL first", "error");
    return;
  }
  try {
    await invoke("start_login", { gilbWebUrl: url });
    setMessage("Continue sign-in in your browser…");
  } catch (err) {
    setMessage(`sign-in error: ${String(err)}`, "error");
  }
}

async function signOut() {
  try {
    await invoke("sign_out");
    setMessage("Signed out");
  } catch (err) {
    setMessage(`sign-out error: ${String(err)}`, "error");
  }
  refreshAuth();
}

async function openPrivacyPane(pane: "accessibility") {
  try {
    await invoke("open_privacy_pane", { pane });
  } catch (err) {
    setMessage(`open_privacy_pane error: ${String(err)}`, "error");
  }
}

async function startCapture() {
  try {
    const id = await invoke<number>("start_capture");
    setMessage(`Recording started, session_id=${id}`);
  } catch (err) {
    setMessage(`start_capture error: ${String(err)}`, "error");
  }
  refresh();
}

async function stopCapture() {
  try {
    await invoke("stop_capture");
    setMessage("Recording stopped");
  } catch (err) {
    setMessage(`stop_capture error: ${String(err)}`, "error");
  }
  refresh();
}

// Silent auto-update: check, install, and relaunch with no prompt. Before
// relaunching we stop any active recording so the session is flushed and its
// row closed cleanly (the installer / relaunch would otherwise kill the app
// mid-write). The relaunched build resumes capture via the autostart flow.
let updateInProgress = false;
async function checkForUpdates() {
  if (updateInProgress) return;
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
    setMessage(`Installing update ${update.version}…`);
    await update.downloadAndInstall();
    await relaunch();
  } catch (err) {
    updateInProgress = false;
    console.warn("update install failed", err);
  }
}

window.addEventListener("DOMContentLoaded", () => {
  $("btn-start")?.addEventListener("click", startCapture);
  $("btn-stop")?.addEventListener("click", stopCapture);
  $("btn-connect")?.addEventListener("click", connect);
  $("btn-signout")?.addEventListener("click", signOut);

  for (const btn of document.querySelectorAll<HTMLButtonElement>(".splash-btn")) {
    btn.addEventListener("click", () => {
      if (btn.dataset.pane === "accessibility") {
        openPrivacyPane("accessibility");
      }
    });
  }

  // Backend proxies EventBus events here — permission/health messages
  // trigger refresh() immediately. Permissions, recording state and
  // session_id update via events; the slow 5-second poll is only for the
  // actions_today counter and as a fallback for a missed broadcast.
  listen("permission", () => refresh());
  listen("health", () => refresh());
  // Backend emits `auth` after the gilb://auth/callback deep link is handled.
  listen("auth", () => refreshAuth());

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
    .then((v) => setText("app-version", `Gilb v${v}`))
    .catch(() => {});

  refresh();
  refreshAuth();
  setInterval(refresh, 5000);

  // Check for updates on launch and periodically.
  checkForUpdates();
  setInterval(checkForUpdates, UPDATE_CHECK_INTERVAL_MS);
});
