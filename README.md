# Gilb (Gilbreth)

A desktop app that records what you do — clicks, keystrokes, focus
changes, clipboard — through OS accessibility APIs, into a local
SQLite database that you (and your LLM tools) can query later.

## Status

Early. Only the capture layer (Layer 1) is implemented. macOS is the
only working backend; the Windows backend is a stub. There is no
hosted release, no installer, and the storage schema is not stable.

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

## Requirements

- macOS (Apple Silicon or Intel). Linux is out of scope.
- Rust toolchain (stable, edition 2021).
- Node + npm (only for the Tauri shell).
- Accessibility and Input Monitoring permissions, granted in
  *System Settings → Privacy & Security*. The app exposes an
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

## Architecture

Cargo workspace with two runnable apps (`apps/gilb-app-tauri`,
`apps/gilb-mcp`) and seven library crates under `crates/`. The
capture pipeline is platform-gated behind a `CapturePlatform`
trait; macOS uses CGEventTap + the Accessibility API.

See [CLAUDE.md](./CLAUDE.md) for the full crate graph, capture →
DB data flow, and macOS-specific notes (entitlements, signing,
permission prompts).

## Privacy

Everything stays on your machine. The capture pipeline drops events
from a fixed block-list of password managers — 1Password, Bitwarden,
KeePassXC, and macOS Keychain Access — at the source, so those apps
never produce rows. For everything else, rows captured while a
password field had focus are masked in-place as described above.

## License

MIT — see [LICENSE](./LICENSE).
