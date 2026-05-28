# Installing gilb

This guide is for people receiving a signed `gilb.dmg` and installing
it on their own Mac. For build instructions, see
[`RELEASING.md`](./RELEASING.md).

## What gilb does

gilb is a macOS desktop app that records your on-screen activity —
clicks, typed text, navigation keys, scrolls, clipboard, focus
changes — through the macOS Accessibility API. Each event is written
to a local SQLite database at `~/.gilb/db.sqlite`.

Everything stays on your machine. gilb makes no network calls, sends
no telemetry, and uploads nothing. The `.app` also ships a
read-only MCP server (`gilb-mcp`) bundled inside it — see the
"Querying recorded activity" section below for how to point an LLM
client at it.

After the first launch, gilb registers itself as a macOS Login Item
and starts at every login. When both permissions are granted it
begins recording automatically — you do not need to keep clicking
the start button each session. Disabling either behaviour is one
toggle away (see below).

Password fields are detected and masked at the database layer; events
from known password managers (1Password, Bitwarden, KeePassXC,
Keychain Access) are dropped at the source.

## Requirements

- macOS 13 (Ventura) or later.
- Apple Silicon or Intel.
- Roughly 100 MB free disk for the app; the database grows with usage
  (typically a few MB per hour of active recording).

## Install

1. Open the `.dmg` you received. A Finder window appears with
   `Gilb.app` and a shortcut to `/Applications`.
2. Drag `Gilb.app` onto the `/Applications` shortcut.
3. Eject the DMG (right-click the disk image in Finder → Eject).
4. Open `/Applications` and double-click `Gilb`.

The build is notarized, so Gatekeeper should let it open directly.
If macOS still refuses ("cannot be opened because Apple cannot check
it for malicious software"), right-click `Gilb.app` → **Open** →
**Open** in the dialog. You only need to do this once.

## Grant permissions

gilb needs two permissions before it can record anything. On first
launch you'll see a splash screen listing them — both must be on:

- **Accessibility** — required to read the focused window, control
  IDs, and element text under the cursor.
- **Input Monitoring** — required to observe mouse and keyboard
  events.

The buttons on the splash open the matching System Settings pane
*and* register `Gilb` with macOS so it appears in the list with its
own toggle — you do not need to drag the app into the list manually
or click the `+` button. Flip the toggle to on, return to Gilb, and
the splash will dismiss itself once both permissions are granted.

If a toggle was already on but the splash still complains, quit and
relaunch `Gilb`.

If macOS later asks for **Automation** permission (to control other
apps), allow it — gilb uses it to read titles and identifiers from
the frontmost app.

## Recording

Once both permissions are granted, gilb starts recording on its own
the moment it launches — including at login. The main window shows
two buttons:

- **Record screen** — start a new recording session manually.
- **Stop record** — end the current session.

A status line under the buttons reports the current session id and
any errors. Quitting the app also ends the current session cleanly.

If you press **Stop record**, gilb will *not* auto-resume in the same
session — press **Record screen** again, or relaunch the app, to
start recording again.

There is no Dock icon — gilb runs as a menu-bar-less utility (the
window is the only entry point). Closing the window keeps the app
running; quit it from **Gilb → Quit Gilb** in the menu bar, or with
⌘Q while the window is focused.

### Disabling autostart at login

Gilb registers itself as a Login Item on first launch (it writes
`~/Library/LaunchAgents/app.farol.gilb.plist`). To stop it from
launching automatically:

- **From the UI:** System Settings → General → Login Items → uncheck
  `Gilb`.
- **From the terminal:**
  ```sh
  launchctl unload ~/Library/LaunchAgents/app.farol.gilb.plist
  rm ~/Library/LaunchAgents/app.farol.gilb.plist
  ```

Note that the next time you launch `Gilb` manually it will re-enable
itself; if you want autostart off permanently, leave the app closed.

## Querying recorded activity

The `.app` bundle ships a read-only MCP server, `gilb-mcp`, at:

```
/Applications/Gilb.app/Contents/MacOS/gilb-mcp
```

It reads `~/.gilb/db.sqlite` directly over stdio MCP transport. It
does not need `Gilb.app` itself to be running. To wire it into
Claude Desktop, add to `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "gilb": {
      "command": "/Applications/Gilb.app/Contents/MacOS/gilb-mcp"
    }
  }
}
```

For Claude Code or other MCP clients, point the same command at the
binary. Restart the client after editing the config.

## Where your data lives

| Path                            | What it is                                  |
|---------------------------------|---------------------------------------------|
| `~/.gilb/db.sqlite`             | All recorded actions and sessions (SQLite). |
| `~/.gilb/db.sqlite-wal`, `-shm` | SQLite write-ahead-log companion files.     |

Nothing is written outside `~/.gilb/` and nothing leaves your machine
unless you explicitly export it. Release builds intentionally do not
write a log file; if you need verbose diagnostics, the build can be
re-cut from source as a dev build.

To wipe the recording history, quit `Gilb` and delete `~/.gilb/`.
A new database is created on next launch.

## Storage format caveat

The database schema is currently considered unstable: future versions
of `Gilb` may change column names, add tables, or evolve the format
without a migration path for data captured by older builds. If you
need long-term archival of recordings, export to a separate format
before upgrading.

## Uninstall

1. Quit gilb.
2. Remove the Login Item so the app does not relaunch at next login:
   ```sh
   launchctl unload ~/Library/LaunchAgents/app.farol.gilb.plist 2>/dev/null
   rm -f ~/Library/LaunchAgents/app.farol.gilb.plist
   ```
   (Or System Settings → General → Login Items → uncheck `Gilb`.)
3. Drag `Gilb.app` from `/Applications` to the Trash.
4. Optionally, remove `~/.gilb/` to delete all recorded data.
5. Optionally, revoke Accessibility / Input Monitoring permissions
   in System Settings (the entries remain even after the app is
   removed).

## Reporting problems

Please include:

- macOS version and architecture (Apple menu → About This Mac).
- A description of what you were doing when the problem occurred,
  ideally step by step.
- If the bug is reproducible, the action kind and the foreground
  app where it happens.
