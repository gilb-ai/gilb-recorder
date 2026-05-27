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
no telemetry, and uploads nothing. Querying the recorded activity
later happens through a separate read-only MCP server (`gilb-mcp`)
that you start explicitly from your LLM tooling.

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
   `gilb.app` and a shortcut to `/Applications`.
2. Drag `gilb.app` onto the `/Applications` shortcut.
3. Eject the DMG (right-click the disk image in Finder → Eject).
4. Open `/Applications` and double-click `gilb`.

The build is notarized, so Gatekeeper should let it open directly.
If macOS still refuses ("cannot be opened because Apple cannot check
it for malicious software"), right-click `gilb.app` → **Open** →
**Open** in the dialog. You only need to do this once.

## Grant permissions

gilb needs two permissions before it can record anything. On first
launch you'll see a splash screen listing them — both must be on.

Open **System Settings → Privacy & Security**, then for each item
below, toggle `gilb` to on:

- **Accessibility** — required to read the focused window, control
  IDs, and element text under the cursor.
- **Input Monitoring** — required to observe mouse and keyboard
  events.

The buttons on the splash screen open the right pane directly. The
splash auto-dismisses once both toggles are on; if not, quit and
relaunch `gilb`.

If macOS later asks for **Automation** permission (to control other
apps), allow it — gilb uses it to read titles and identifiers from
the frontmost app.

## Start and stop recording

The main window has:

- A **Status** panel showing whether permissions are granted, whether
  a session is running, and how many actions were captured today.
- **Start** / **Stop** buttons.
- **Connect to MCP client** — opens a dialog that helps you register
  `gilb-mcp` with Claude Code or another MCP client, so an LLM can
  read the recorded activity.

Click **Start** to begin recording. The status row will switch to
"Recording" and the action counter will start climbing. Click **Stop**
to end the session. Quitting the app also ends the current session
cleanly.

There is no Dock icon — gilb runs as a menu-bar-less utility (the
window is the only entry point). Closing the window keeps the app
running; quit it from **gilb → Quit gilb** in the menu bar, or with
⌘Q while the window is focused.

## Where your data lives

| Path                          | What it is                                    |
|-------------------------------|-----------------------------------------------|
| `~/.gilb/db.sqlite`           | All recorded actions and sessions (SQLite).   |
| `~/.gilb/db.sqlite-wal`, `-shm` | SQLite write-ahead-log companion files.     |
| `~/.gilb/logs/gilb.log.*`     | Daily-rotated app logs (text).                |

Nothing is written outside `~/.gilb/` and nothing leaves your machine
unless you explicitly export it.

To wipe the recording history, quit `gilb` and delete `~/.gilb/`.
A new database is created on next launch.

## Storage format caveat

The database schema is currently considered unstable: future versions
of `gilb` may change column names, add tables, or evolve the format
without a migration path for data captured by older builds. If you
need long-term archival of recordings, export to a separate format
before upgrading.

## Uninstall

1. Quit gilb.
2. Drag `gilb.app` from `/Applications` to the Trash.
3. Optionally, remove `~/.gilb/` to delete all recorded data and logs.
4. Optionally, revoke Accessibility / Input Monitoring permissions
   in System Settings (the entries remain even after the app is
   removed).

## Reporting problems

Please include:

- The version (visible in **gilb → About gilb** or in the bottom of
  the window).
- macOS version and architecture (Apple menu → About This Mac).
- The relevant tail of `~/.gilb/logs/gilb.log.YYYY-MM-DD`.
- A description of what you were doing when the problem occurred.
