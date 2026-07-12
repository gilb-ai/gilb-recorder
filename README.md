# Gilb (Gilbreth)

A desktop app that records what you do — clicks, keystrokes, focus
changes, clipboard — through OS accessibility APIs, into a local
SQLite database that you (and your LLM tools) can query later.
When (and only when) the device is signed in to a gilb-web
workspace, a background shipper uploads the recorded activity to
that server — see **What leaves the device** below.

## Status

Early. Only the capture layer (Layer 1) is implemented. macOS and
Windows both have working backends (macOS via CGEventTap + the
Accessibility API; Windows via UI Automation + event hooks). Signed
installers (macOS `.dmg`, Windows NSIS) with auto-update are
published via GitHub Releases. The storage schema is not yet stable.

## What it captures

One row per atomic action in `actions`:

- `click` — mouse button down, enriched with the AX element (role,
  name, value) under the cursor when available
- `text` — typed text after a 300 ms debounce (one row = one "burst")
- `key` — non-printable navigation/editing keys (Enter, Tab, arrows,
  Backspace, …)
- `scroll` — wheel events
- `clipboard` — clipboard text (PII-redacted, capped at 64 KiB)
- `focus_change` — frontmost app changed
- `system` — lifecycle markers (`idle_start` / `idle_end` / `alive`)

With `CAPTURE_SCREENSHOTS=1` (opt-in, **off by default**) the recorder
also samples JPEG screenshots of the main display on focus-change
boundaries — debounced, idle-suppressed, capped per hour, suppressed
for excluded apps / secure fields and on multi-display setups, and
gated on the macOS Screen Recording permission. Screenshots are the
heaviest-PII modality; enabling them is a deliberate choice.

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
| `CAPTURE_SCREENSHOTS`    | `false` | Boundary-sampled screenshots (opt-in)   |
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

Cargo workspace with two runnable apps (`apps/gilb-app-tauri`,
`apps/gilb-mcp`) and thirteen library crates under `crates/`. The
capture pipeline is platform-gated behind a `CapturePlatform` trait;
macOS uses CGEventTap + the Accessibility API, Windows uses UI
Automation + event hooks.

See [CLAUDE.md](./CLAUDE.md) for the full crate graph, capture →
DB data flow, and macOS-specific notes (entitlements, signing,
permission prompts).

## What leaves the device

**Nothing, by default.** Without a sign-in, all data stays in
`~/.gilb/db.sqlite` on your machine.

After you sign in to a gilb-web workspace (Connect → browser consent →
`gilb://auth/callback` deep link writes `~/.gilb/credentials.json`), a
background shipper starts and, every 60 seconds:

- uploads buffered `actions` rows as JSONL to
  `POST {workspace}/api/v1/ingest`;
- uploads sampled screenshots (if you opted in) to
  `POST {workspace}/api/v1/ingest/screenshots`.

Rows from password-field contexts are masked **again at egress**
(`[masked]`), mirroring the local masking — secure-field values never
leave the device even if the local DB is read by other tools. Shipped
rows are pruned locally after 7 days; sign-out stops the shipper
immediately and deletes the credentials file. The full shipping
implementation is open source in `crates/gilb-shipper` — audit what
gets sent there.

## Privacy

The capture pipeline drops events from a fixed block-list of password
managers — 1Password, Bitwarden, KeePassXC, and macOS Keychain Access —
at the source, so those apps never produce rows. For everything else,
rows captured while a password field had focus are masked in-place as
described above. Screenshots (opt-in) are additionally suppressed for
excluded apps and secure fields at capture-decision time *and*
re-checked at grab time.

## License

MIT — see [LICENSE](./LICENSE).
