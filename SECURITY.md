# Security policy

gilb reads keystrokes, mouse events, focused-window contents, and
clipboard text on the user's Mac and writes them to a local SQLite
database. Any flaw that lets that data escape the machine, lets an
unprivileged process read the database, or expands gilb's permission
surface beyond what the user granted, is in scope.

## Reporting a vulnerability

**Please do not open a public GitHub issue.**

Email **leonid@dinershtein.com** with:

- A description of the issue.
- Steps to reproduce, or a proof-of-concept, if you have one.
- The version of gilb (visible in the `.app` info pane), macOS
  version, and architecture.
- Whether the report is already public elsewhere.

You should get an acknowledgement within 7 days. If you do not, feel
free to send a polite reminder.

## What happens next

- We confirm or reject the report and let you know either way.
- If confirmed, we ship a fix in a new tagged release and credit you
  in the release notes (unless you ask to stay anonymous).
- We do not currently run a bug-bounty program.

## In scope

- Anything in this repository: the Tauri shell (`apps/gilb-app-tauri`),
  the MCP sidecar (`apps/gilb-mcp`), all `crates/*`.
- Build artefacts produced by `npm run tauri build` — the signed
  `.app` and `.dmg`.

## Out of scope

- macOS itself, the Tauri framework, the Rust toolchain, third-party
  crates. Forward those upstream.
- Issues that require an attacker who already has root or full disk
  access on the same machine.
