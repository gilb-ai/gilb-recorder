// Settings window. Opened from Rust as a second OS window loading
// settings.html. It hosts the "Enable meeting detection" toggle (presentational)
// and the functional BYOK OpenAI API-key field.
//
// The toggle stays purely presentational: this script flips its visual state
// locally and does NOT persist or start/stop detection (a separate card).
//
// The API-key field IS functional: it loads the persisted key on open
// (get_openai_key), persists/clears it on Save (set_openai_key), and validates
// it against OpenAI on Test (test_openai_key).

import { invoke } from "@tauri-apps/api/core";

const $ = <T extends HTMLElement = HTMLElement>(id: string) =>
  document.getElementById(id) as T | null;

function initMeetingToggle() {
  const toggle = $<HTMLButtonElement>("toggle-meeting");
  if (!toggle) return;

  toggle.addEventListener("click", () => {
    const on = toggle.getAttribute("aria-checked") === "true";
    toggle.setAttribute("aria-checked", on ? "false" : "true");
  });
}

type StatusKind = "ok" | "warn" | "error" | "muted";

function initOpenAiKey() {
  const input = $<HTMLInputElement>("openai-key");
  const saveBtn = $<HTMLButtonElement>("btn-save-key");
  const testBtn = $<HTMLButtonElement>("btn-test-key");
  const status = $<HTMLSpanElement>("key-status");
  if (!input || !saveBtn || !testBtn || !status) return;

  const setStatus = (text: string, kind: StatusKind = "muted") => {
    status.textContent = text;
    status.dataset.kind = kind;
  };

  // Load the persisted key (if any) into the field on open.
  invoke<string | null>("get_openai_key")
    .then((key) => {
      if (key) input.value = key;
    })
    .catch((e) => setStatus(`Couldn't load saved key: ${e}`, "error"));

  saveBtn.addEventListener("click", async () => {
    saveBtn.disabled = true;
    try {
      await invoke("set_openai_key", { key: input.value });
      setStatus(input.value.trim() ? "Saved." : "Cleared.", "ok");
    } catch (e) {
      setStatus(`Save failed: ${e}`, "error");
    } finally {
      saveBtn.disabled = false;
    }
  });

  testBtn.addEventListener("click", async () => {
    const key = input.value.trim();
    if (!key) {
      setStatus("Enter a key to test.", "warn");
      return;
    }
    testBtn.disabled = true;
    setStatus("Testing…", "muted");
    try {
      const result = await invoke<string>("test_openai_key", { key });
      if (result === "valid") setStatus("Key is valid.", "ok");
      else if (result === "invalid") setStatus("Key was rejected.", "error");
      else setStatus("Couldn't verify — try again later.", "warn");
    } catch (e) {
      setStatus(`Test failed: ${e}`, "error");
    } finally {
      testBtn.disabled = false;
    }
  });
}

window.addEventListener("DOMContentLoaded", () => {
  initMeetingToggle();
  initOpenAiKey();
});
