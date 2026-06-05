// Settings window. Opened from Rust as a second OS window loading
// settings.html. Today it hosts a single "Enable meeting detection" toggle.
//
// The toggle is purely presentational: this script flips its checked/unchecked
// visual state locally. It does NOT `invoke` anything, does NOT persist, and
// does NOT start or stop detection — that behavior is a separate card.

const $ = <T extends HTMLElement = HTMLElement>(id: string) =>
  document.getElementById(id) as T | null;

window.addEventListener("DOMContentLoaded", () => {
  const toggle = $<HTMLButtonElement>("toggle-meeting");
  if (!toggle) return;

  toggle.addEventListener("click", () => {
    const on = toggle.getAttribute("aria-checked") === "true";
    toggle.setAttribute("aria-checked", on ? "false" : "true");
  });
});
