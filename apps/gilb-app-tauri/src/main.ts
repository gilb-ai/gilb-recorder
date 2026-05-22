import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type Permissions = {
  accessibility: boolean;
  input_monitoring: boolean;
};

type EngineStatus = {
  recording: boolean;
  session_id: number | null;
  permissions: Permissions;
  actions_today: number;
  platform: string;
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

  // Splash имеет смысл только на macOS; на других платформах ничего не делаем.
  const macOnly = platform === "macos";
  const needsAx = macOnly && !perms.accessibility;
  const needsIm = macOnly && !perms.input_monitoring;
  const visible = needsAx || needsIm;

  splash.hidden = !visible;
  if (!visible) return;

  setDot("splash-ax-dot", perms.accessibility);
  setDot("splash-im-dot", perms.input_monitoring);
  setText(
    "splash-ax-status",
    perms.accessibility ? "выдано" : "не выдано",
  );
  setText(
    "splash-im-status",
    perms.input_monitoring ? "выдано" : "не выдано",
  );

  for (const step of ["splash-step-ax", "splash-step-im"]) {
    const el = $(step);
    if (!el) continue;
    const granted =
      step === "splash-step-ax" ? perms.accessibility : perms.input_monitoring;
    el.classList.toggle("granted", granted);
  }
}

// Sequence counter — refresh()-вызовы могут гоняться параллельно (poll, явный
// вызов после start/stop, и listen("permission"/"health")). Применяем к DOM
// только результат последнего стартовавшего вызова — out-of-order ответы
// отбрасываются.
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

  setText("status-platform", s.platform);
  setDot("status-platform-dot", true);

  setText("status-ax", s.permissions.accessibility ? "granted" : "not granted");
  setDot("status-ax-dot", s.permissions.accessibility);

  setText(
    "status-im",
    s.permissions.input_monitoring ? "granted" : "not granted",
  );
  setDot("status-im-dot", s.permissions.input_monitoring);

  setText("status-rec", s.recording ? "recording" : "stopped");
  setDot("status-rec-dot", s.recording);

  setText("status-session", s.session_id ? String(s.session_id) : "—");
  setText("status-actions", String(s.actions_today));

  const startBtn = $<HTMLButtonElement>("btn-start");
  const stopBtn = $<HTMLButtonElement>("btn-stop");
  if (startBtn) startBtn.disabled = s.recording;
  if (stopBtn) stopBtn.disabled = !s.recording;

  updateSplash(s.permissions, s.platform);
}

async function openPrivacyPane(pane: "accessibility" | "input_monitoring") {
  try {
    await invoke("open_privacy_pane", { pane });
  } catch (err) {
    setMessage(`open_privacy_pane error: ${String(err)}`, "error");
  }
}

async function startCapture() {
  try {
    const id = await invoke<number>("start_capture");
    setMessage(`Запись началась, session_id=${id}`);
  } catch (err) {
    setMessage(`start_capture error: ${String(err)}`, "error");
  }
  refresh();
}

async function stopCapture() {
  try {
    await invoke("stop_capture");
    setMessage("Запись остановлена");
  } catch (err) {
    setMessage(`stop_capture error: ${String(err)}`, "error");
  }
  refresh();
}

window.addEventListener("DOMContentLoaded", () => {
  $("btn-start")?.addEventListener("click", startCapture);
  $("btn-stop")?.addEventListener("click", stopCapture);

  for (const btn of document.querySelectorAll<HTMLButtonElement>(".splash-btn")) {
    btn.addEventListener("click", () => {
      const pane = btn.dataset.pane;
      if (pane === "accessibility" || pane === "input_monitoring") {
        openPrivacyPane(pane);
      }
    });
  }

  // Бэкенд проксирует EventBus сюда — permission/health-события сразу
  // дёргают refresh(). Permissions, recording, session_id обновляются по
  // событию; медленный 5-секундный poll нужен только для счётчика
  // actions_today и как fallback на случай пропущенного broadcast'а.
  listen("permission", () => refresh());
  listen("health", () => refresh());

  refresh();
  setInterval(refresh, 5000);
});
