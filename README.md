# Gilb (Gilbreth)

A desktop app that records what you do — clicks, keystrokes, focus
changes, clipboard — through OS accessibility APIs, into a local
SQLite database that you (and your LLM tools) can query later.

## Status

Early. macOS and Windows both have working capture backends (macOS via
CGEventTap + the Accessibility API; Windows via UI Automation + event
hooks). Signed installers (macOS `.dmg`, Windows NSIS) with auto-update
are published via GitHub Releases. The storage schema is not yet stable —
migrations are additive, but queries written against it may need updating.

What exists is capture, meeting recording, on-device transcription and
real-time suggestions. Pattern mining over the captured stream — the
therbligs the name refers to — is the next layer and is not implemented.

## What it captures

One row per atomic action in `actions`:

- `click` — mouse button down, enriched with the AX element (role,
  name, value) under the cursor when available
- `text` — typed text after a 300 ms debounce (one row = one "burst")
- `key` — non-printable navigation/editing keys (Enter, Tab, arrows,
  Backspace, …)
- `scroll` — wheel events
- `clipboard` — clipboard text
- `focus_change` — frontmost app changed

Every row carries the foreground app context (`app_name`,
`app_bundle_id`, `window_title`) and a session ID. Rows captured
while a password field had focus are masked at the SQL layer
(`text_content` and `element_value` become `'[masked]'`,
`password_flag = true`).

Meetings are captured in parallel: when a video-conference app
starts a call, Gilb records the meeting (start/end, app, audio/video
paths) into a `meetings` table and links subsequent actions to it via
`actions.meeting_id`. After the call ends, the audio is transcribed
fully on-device with whisper.cpp into `meeting_transcripts`.

## Real-time suggestions (optional)

While a call is running, Gilb can transcribe it live and show short
suggestions in a small always-on-top panel — powered by a coding agent
you already have installed (`claude`, `gemini`, or any [ACP](https://agentclientprotocol.com)
adapter), talked to over stdio. Nothing leaves the machine except what
that agent itself sends.

It is off by default at three levels: a Cargo feature, a switch in the
app, and a ~570 MB speech model downloaded only if you turn it on. The
prompt lives in `~/.gilb/assist-prompt.md` and is yours to edit. See
[`docs/assist.md`](./docs/assist.md) for how the pipeline fits together.

## Requirements

- macOS (Apple Silicon or Intel) or Windows. Linux is out of scope.
- Rust toolchain (stable, edition 2021).
- Node + npm (only for the Tauri shell).
- On macOS: Accessibility and Input Monitoring permissions, granted
  in *System Settings → Privacy & Security*. The app exposes an
  `open_privacy_pane` command that jumps you to the relevant pane.

## Build and run

```sh
# Tauri shell with the recorder UI.
cd apps/gilb-app-tauri
npm install
npm run tauri dev

# Read-only MCP server over ~/.gilb/db.sqlite (stdio transport).
# Point an MCP client (e.g. Claude Code) at this binary to query
# recorded activity. See apps/gilb-mcp/help.md for the tool catalog.
cargo run -p gilb-mcp
```

Build options live in `RecordingSettings::from_env`:

| Env var                  | Default | Effect                                  |
|--------------------------|---------|-----------------------------------------|
| `CAPTURE_EVENTS`         | `true`  | Toggle the entire capture pipeline      |
| `CAPTURE_MOUSE_MOVE`     | `false` | Record raw mouse-move events (noisy)    |
| `CAPTURE_CLIPBOARD`      | `true`  | Record clipboard text                   |
| `CAPTURE_TREE_SNAPSHOTS` | `true`  | Periodic full AX tree dumps             |
| `RUST_LOG`               | varies  | Standard `tracing` filter               |

## Querying recorded activity from Claude Code

If you installed Gilb from a release (the macOS `.dmg`), the read-only
MCP server ships inside the app bundle — no build step needed. The
binary lives at:

```
/Applications/Gilb.app/Contents/MacOS/gilb-mcp
```

Register it with Claude Code:

```sh
claude mcp add gilb --scope user /Applications/Gilb.app/Contents/MacOS/gilb-mcp
```

Use `--scope user` so the server is available in every project, since
Gilb records activity regardless of which repo you're working in. Drop
the flag to register it for the current project only. Confirm it
registered and connected:

```sh
claude mcp list
```

Inside a Claude Code session the `gilb_*` tools are now available (see
[`apps/gilb-mcp/help.md`](./apps/gilb-mcp/help.md) for the full
catalog). The server reads `~/.gilb/db.sqlite` over stdio; `Gilb.app`
itself does not need to be running. If you built Gilb from source, point
the same command at the built binary instead (`cargo run -p gilb-mcp`,
or `target/release/gilb-mcp`). See [`INSTALL.md`](./INSTALL.md) for the
end-user install and permissions guide.

## Architecture

Cargo workspace with three runnable apps and fourteen library crates
under `crates/`:

- **`apps/gilb-app-tauri`** — the desktop app (tray + one window).
- **`apps/gilb-mcp`** — read-only MCP server over the recorded database.
- **`apps/gilb-analyzer`** — runs prompt-jobs against your own recorded
  activity and pushes findings to a server. Requires credentials most
  users will not have; nothing else depends on it.

The capture pipeline is platform-gated behind a `CapturePlatform` trait;
macOS uses CGEventTap + the Accessibility API, Windows uses UI Automation
+ event hooks. A no-op backend keeps the workspace compiling elsewhere,
which is what CI builds on Linux.

See [CLAUDE.md](./CLAUDE.md) for the full crate graph, capture →
DB data flow, and macOS-specific notes (entitlements, signing,
permission prompts).

## Privacy

Capture, storage and transcription are entirely local: the database is
a file in `~/.gilb/`, whisper.cpp runs on-device, and nothing is
uploaded. The capture pipeline drops events from a fixed block-list of
password managers — 1Password, Bitwarden, KeePassXC, and macOS Keychain
Access — at the source, so those apps never produce rows. For everything
else, rows captured while a password field had focus are masked in-place
as described above.

Two optional parts do leave the machine, and only if you enable them:

- **Real-time suggestions** send the transcribed conversation to the
  agent you configured. Where that goes is that agent's business — a
  cloud model if it is `claude`, nowhere if it is a local one.
- **`gilb-analyzer`** posts its findings to a server, and needs
  credentials you must supply. Without them it does nothing.

Meeting recording captures screen and audio to disk while a call is
running. That is the point of the feature, but it is worth saying
plainly: those files are as sensitive as the calls themselves.

## License

MIT — see [LICENSE](./LICENSE).
