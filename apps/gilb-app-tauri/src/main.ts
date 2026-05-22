import { invoke } from "@tauri-apps/api/core";

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

async function refresh() {
  try {
    const s = await invoke<EngineStatus>("status");
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
  } catch (err) {
    setMessage(`status error: ${String(err)}`, "error");
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
  refresh();
  setInterval(refresh, 1500);
});
