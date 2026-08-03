# Security policy

gilb reads keystrokes, mouse events, focused-window contents and
clipboard text, records meetings (screen and audio), and writes all of
it to local storage. Any flaw that lets that data escape the machine,
lets an unprivileged process read the database or the recordings, or
expands gilb's permission surface beyond what the user granted, is in
scope.

## Reporting a vulnerability

**Please do not open a public GitHub issue.**

Email **leonid@dinershtein.com** with:

- A description of the issue.
- Steps to reproduce, or a proof-of-concept, if you have one.
- The version of gilb, your OS version, and architecture.
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
  the MCP sidecar (`apps/gilb-mcp`), the analyzer (`apps/gilb-analyzer`),
  all `crates/*`.
- Build artefacts we publish — the signed `.app` / `.dmg` and the
  Windows installer.

## Out of scope

- The OS itself, the Tauri framework, the Rust toolchain, third-party
  crates. Forward those upstream.
- Whatever a locally installed agent does with the conversation when
  real-time suggestions are enabled — that is between you and that
  agent's vendor. A flaw in *how gilb hands it over* is in scope.
- Issues that require an attacker who already has root or full disk
  access on the same machine.
