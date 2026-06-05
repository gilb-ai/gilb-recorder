# Gilb UI/UX conventions

The conventions every screen in the desktop app follows. New UI must match
these unless there's a documented reason not to. Canonical implementations are
referenced inline — copy those, don't reinvent.

## Two capture subsystems — keep them distinct

Gilb has **two independent capture subsystems**. They are different things with
different lifecycles, consent models, and vocabulary. The UI must never conflate
them — that confusion is what this section exists to prevent. Internally they
already map to different tables (`sessions` vs `meetings`).

### A — Activity tracking (always-on a11y)

The core Layer-1 capture: accessibility actions (keyboard/mouse/AX tree) streamed
to the DB (`sessions`). Backed by `start_capture` / `stop_capture` on the engine.

- **Word: "Activity tracking"** — never "recording". Status reads
  "Activity tracking — on / Paused".
- **Control: Pause / Resume.** The user can stop and restart the always-on
  capture at any time (honest for a privacy tool). Pause maps to `stop_capture`,
  Resume to `start_capture`.
- **Persistence:** the paused state is persisted. On launch Gilb auto-resumes
  tracking **only if not paused** — a deliberate pause survives restarts; it is
  never silently re-enabled.
- **Indicator: calm, non-pulsing** — a steady dot (green = on, gray = paused),
  with the `actions_today` count as the "data is flowing" signal. No pulse — the
  pulse is reserved for live meeting recording.
- **Consent:** first-run permission (it's the core), then persistent.

### B — Meeting recording (event-driven)

Captures a meeting to a screen-video `.mp4` + audio `.wav` (`meetings`), then
transcribes it. Triggered by the meeting detector (mic in use by an allowlisted
call app).

- **Word: "Recording" / "Rec"** — reserved for meetings (they produce a
  reviewable artifact). The green **pulsing** pill + countdown popups are this
  subsystem only.
- **Control: a master on/off** ("Enable meeting detection" in Settings) gates the
  detector; per-meeting the countdown popup is the consent.
- **Indicator: green pulsing pill** (`.rec-indicator`) while a meeting records.

### The rule

"Recording" and the green pulse mean **meetings**. The always-on a11y layer is
"Activity tracking" with a calm indicator and Pause/Resume. Don't reuse one
subsystem's vocabulary or visual language for the other.


## Window model — one window, in-app screens

There is **one** main OS window. All "inside Gilb" UI — settings, and any future
secondary views (history, detail panes, wizards) — renders **inside it as a
modal overlay**, never as a second `WebviewWindowBuilder` popup.

- Canonical overlay: the settings screen (`#settings-overlay` in `index.html`,
  `openSettings`/`closeSettings` in `src/main.ts`), and the permission
  `#splash` overlay. New screens follow the same shape.
- Do **not** add `WebviewWindowBuilder` windows for app configuration or
  navigation. A separate OS window is reserved for the one thing it's right for:

### The only legitimate separate windows: system-level prompts

The meeting **countdown** and **stop-countdown** popups (`countdown.html`,
`stop-countdown.html`) are borderless, `always_on_top`, decorationless windows.
They exist as separate windows **on purpose**: they must float over *other apps*
(Zoom, Slack…), not just Gilb, so the user sees the prompt while Gilb is in the
background. That system-level-prompt role is the bar for a new window. "It's a
different screen" is not — that's a modal overlay.

## Modal overlay pattern

Mirror `.settings-overlay` / `.splash` in `src/styles.css`:

- Full-window `position: fixed; inset: 0`, dim + `backdrop-filter: blur`,
  `z-index: 1000`, flex-centered card.
- Card: `role="dialog"`, `aria-modal="true"`, `aria-labelledby` the title.
- **Esc cancels.** Focus moves into the dialog on open (e.g. the primary button).
- Toggled via the `hidden` attribute (`[hidden] { display: none }`), not by
  building/destroying DOM.

## Save / Cancel semantics

Any screen that edits persisted state has explicit **Save** and **Cancel**, not
auto-apply:

- **Open** loads persisted state fresh, so the screen always reflects what's
  saved (`openSettings` re-reads via `get_openai_key`).
- **Save** persists, then closes; on error, stay open and show the error.
- **Cancel** (and Esc) discards: close without persisting, and revert any local
  presentational state to its pre-open snapshot.
- Never silently persist on edit or on close. Irreversible/outward actions are
  always explicit.

## Status & color language

- **Green** = active / healthy. The live recording indicator pill is green with
  a pulsing dot (`.rec-indicator` / `.rec-dot`).
- **Red** = destructive / stop. Stop buttons (`#btn-stop`, `.rec-stop`, the
  stop-countdown progress button `.countdown-record--stop`) are red.
- **Blue** = primary / go (`#btn-start`, `.key-btn-primary`, the start-countdown
  progress button).
- **Amber** = warning, **gray** = muted/neutral (`.key-status[data-kind=…]`).
- A pulsing dot signals a live/ongoing state; static UI does not pulse.

## Countdown progress-button pattern

Both countdown popups use one pattern: a button whose translucent fill sweeps
0→100% over `RecordingSettings::countdown_seconds`. Reaching 100% performs the
**default action** automatically; clicking does it now; a secondary button backs
out. The auto-action is always the **safe** default:

- Start: fill → arm ("Record now", blue) / "Cancel" backs out.
- Stop: fill → stop ("Stop now", red) / "Keep recording" backs out.

## Copy / voice

- User-facing strings are **English** (the app is open source).
- Name the product where a bare label would be ambiguous out of context:
  "Gilb Meeting Recording is about to start for …", not "Recording is about to…".
  Floating/notification surfaces especially must self-identify as Gilb.
- Rendered with `textContent`, never `innerHTML` — no HTML injection from
  meeting/app names or any dynamic string.

## Tech & structure

- Vanilla **TypeScript + Vite**, no framework. DOM via `getElementById`
  (`const $ = …`), state via direct DOM updates.
- Backend calls via `invoke`; backend→UI via Tauri `listen` (e.g.
  `meeting-recording`, `permission`, `health`). Prefer event-driven updates over
  polling; poll only as a slow fallback.
- Each window/page is its own HTML entry registered in `vite.config.ts`, with a
  matching capability under `src-tauri/capabilities/`. Custom commands are
  auto-allowed for local origins; capabilities grant event listen/plugin perms.
- Strict production CSP (`script-src 'self'`, no inline/remote scripts) — keep it
  that way; it's what makes `textContent` + local-only content safe.
