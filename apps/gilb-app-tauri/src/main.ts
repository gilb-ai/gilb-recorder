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

  // Splash only makes sense on macOS; do nothing on other platforms.
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
    perms.accessibility ? "granted" : "not granted",
  );
  setText(
    "splash-im-status",
    perms.input_monitoring ? "granted" : "not granted",
  );

  for (const step of ["splash-step-ax", "splash-step-im"]) {
    const el = $(step);
    if (!el) continue;
    const granted =
      step === "splash-step-ax" ? perms.accessibility : perms.input_monitoring;
    el.classList.toggle("granted", granted);
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

// ---- MCP connect modal --------------------------------------------------

type McpClientStatus = {
  id: string;
  label: string;
  available: boolean;
  hint: string | null;
};

type McpServerInfo = {
  binary_path: string | null;
  config_json: string;
  clients: McpClientStatus[];
};

type ConnectOutcome =
  | { kind: "installed"; details: string }
  | { kind: "opened_deeplink"; url: string }
  | { kind: "needs_manual"; hint: string };

function setMcpResult(text: string, kind: "info" | "error" = "info") {
  const el = $("mcp-result");
  if (!el) return;
  el.textContent = text;
  el.dataset.kind = kind;
}

async function openMcpModal() {
  const modal = $("mcp-modal");
  if (!modal) return;
  setMcpResult("");
  modal.hidden = false;

  try {
    const info = await invoke<McpServerInfo>("mcp_server_info");
    renderMcpModal(info);
  } catch (err) {
    setMcpResult(`mcp_server_info error: ${String(err)}`, "error");
  }
}

function closeMcpModal() {
  const modal = $("mcp-modal");
  if (modal) modal.hidden = true;
}

function renderMcpModal(info: McpServerInfo) {
  const list = $<HTMLUListElement>("mcp-clients");
  if (list) {
    list.innerHTML = "";
    for (const client of info.clients) {
      list.appendChild(renderClientRow(client, !info.binary_path));
    }
  }

  const json = $("mcp-other-json");
  if (json) json.textContent = info.config_json;

  if (!info.binary_path) {
    setMcpResult("gilb-mcp binary not found.", "error");
  }
}

function renderClientRow(client: McpClientStatus, binaryMissing: boolean): HTMLLIElement {
  const li = document.createElement("li");
  if (!client.available) li.classList.add("unavailable");

  const text = document.createElement("div");
  text.className = "mcp-client-text";
  const title = document.createElement("strong");
  title.textContent = client.label;
  text.appendChild(title);
  if (client.hint) {
    const hint = document.createElement("span");
    hint.className = "mcp-client-hint";
    hint.textContent = client.hint;
    text.appendChild(hint);
  }
  li.appendChild(text);

  const btn = document.createElement("button");
  btn.type = "button";
  btn.textContent = "Connect";
  btn.disabled = binaryMissing;
  btn.addEventListener("click", () => connectClient(client.id));
  li.appendChild(btn);

  return li;
}

async function connectClient(clientId: string) {
  setMcpResult("Connecting…");
  try {
    const outcome = await invoke<ConnectOutcome>("mcp_connect", { client: clientId });
    if (outcome.kind === "installed") {
      setMcpResult(`✓ ${outcome.details}`);
    } else if (outcome.kind === "opened_deeplink") {
      setMcpResult("Opened deeplink — confirm in your client.");
    } else if (outcome.kind === "needs_manual") {
      setMcpResult(outcome.hint, "error");
    }
  } catch (err) {
    setMcpResult(String(err), "error");
  }
}

async function copyMcpJson() {
  const json = $("mcp-other-json")?.textContent || "";
  try {
    await navigator.clipboard.writeText(json);
    setMcpResult("✓ Copied to clipboard.");
  } catch (err) {
    setMcpResult(`Copy failed: ${String(err)}`, "error");
  }
}

window.addEventListener("DOMContentLoaded", () => {
  $("btn-start")?.addEventListener("click", startCapture);
  $("btn-stop")?.addEventListener("click", stopCapture);
  $("btn-connect-mcp")?.addEventListener("click", openMcpModal);
  $("mcp-modal-close")?.addEventListener("click", closeMcpModal);
  $("mcp-other-copy")?.addEventListener("click", copyMcpJson);

  // Close the modal on a backdrop click.
  $("mcp-modal")?.addEventListener("click", (ev) => {
    if (ev.target === ev.currentTarget) closeMcpModal();
  });

  for (const btn of document.querySelectorAll<HTMLButtonElement>(".splash-btn")) {
    btn.addEventListener("click", () => {
      const pane = btn.dataset.pane;
      if (pane === "accessibility" || pane === "input_monitoring") {
        openPrivacyPane(pane);
      }
    });
  }

  // Backend proxies EventBus events here — permission/health messages
  // trigger refresh() immediately. Permissions, recording state and
  // session_id update via events; the slow 5-second poll is only for the
  // actions_today counter and as a fallback for a missed broadcast.
  listen("permission", () => refresh());
  listen("health", () => refresh());

  refresh();
  setInterval(refresh, 5000);
});
