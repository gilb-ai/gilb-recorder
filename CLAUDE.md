# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

**Gilb (Gilbreth)** — a desktop app that records the user's actions
via accessibility APIs (macOS today; Windows backend is a stub).
Cargo workspace + Tauri 2.

The product is structured in three layers, of which only the first is
implemented:

1. **Raw a11y capture** — current focus.
2. Pattern mining (therbligs) — deferred.
3. Agent skill — deferred.

## Conventions

- **Commit messages: English** (subject + body). Terse subject ≤72
  chars, body wraps ~72 cols. Match the style of recent commits.
- **User-visible strings: English** — HTML, TypeScript messages,
  Rust dialog/error text, `Info.plist` usage descriptions, READMEs.
  Frontend strings live in `apps/gilb-app-tauri/src/i18n.ts` (en + ru
  dictionaries; static markup carries `data-i18n` attributes). The
  locale and product name are baked in per build via `VITE_LOCALE` /
  `VITE_BRAND_NAME` (default: en / Gilb) so differently-branded builds
  reuse this frontend without forking. New user-facing strings go
  through `t()` / `data-i18n`, with both dictionary entries filled.
- **CLAUDE.md and any other docs read by an agent as instructions:
  English.**
- **UI/UX: follow `docs/ui-design.md`.** Single main window with in-app
  modal overlays (not popup windows) for app screens, explicit
  Save/Cancel, the green=active / red=stop color language, etc. Read it
  before adding or changing any frontend UI.

## Commands

Build and run go through the root Cargo workspace plus `npm` inside
`apps/gilb-app-tauri`.

```sh
# Cargo workspace (Rust). Run from the repo root.
cargo build                                # whole workspace
cargo build -p gilb-a11y                   # one crate
cargo test                                 # all tests
cargo test -p gilb-db                      # one crate's tests
cargo test -p gilb-a11y text_buffer        # name-filtered tests
cargo clippy --workspace --all-targets     # lint
cargo fmt --all                            # format

# MCP server over the recorded DB (stdio). Spawned by Claude Code,
# but handy to run manually:
cargo run -p gilb-mcp

# Tauri (frontend + Rust shell). Run from apps/gilb-app-tauri.
cd apps/gilb-app-tauri
npm install                                # once
npm run tauri dev                          # dev shell with hot-reload
npm run tauri build                        # release .dmg/.msi signed per tauri.conf.json
```

Capture defaults are controlled by env vars consumed by
`RecordingSettings::from_env`: `CAPTURE_EVENTS`, `CAPTURE_MOUSE_MOVE`,
`CAPTURE_CLIPBOARD`, `CAPTURE_TREE_SNAPSHOTS`, `CAPTURE_SCREENSHOTS`
(opt-in, default off — heaviest PII modality; also requires the macOS
Screen Recording permission). Logging: `RUST_LOG=...`
(defaults: `info,gilb=debug` in the Tauri shell, `info` in the CLI).

The DB lives at `~/.gilb/db.sqlite` (see `gilb_config::db_path`).

## Architecture

### Crates and dependencies

```
gilb-core ──► (types: Action, ActionKind, AppInfo, ElementContext, SessionId)
gilb-config ─► (RecordingSettings, data_dir / db_path)
gilb-events ─► (EventBus: broadcast PermissionEvent + HealthEvent)

gilb-db ─────► gilb-core, gilb-config
              (SqlitePool + migrations under migrations/, sessions / actions modules)

gilb-a11y ───► gilb-core, gilb-config, gilb-events, gilb-db
              (trait CapturePlatform; cfg-gated implementations;
               text_buffer, activity_feed, budget, tree/, password_masking;
               bin gilb-a11y-cli)

gilb-shipper ─► gilb-db, gilb-events
              (background egress: cursor over unshipped actions/screenshots →
               JSONL / multipart POST to gilb-web ingest; retry/backoff with a
               Transient/Permanent/Auth error taxonomy, poison-row dead-letter,
               masking-on-egress, retention prune + local janitor)

gilb-engine ─► all crates above + gilb-shipper
              (Engine — long-lived process-wide object; owns the DB pool,
               EventBus, current CaptureSession; spawns the writer task, the
               always-on janitor, and — when ~/.gilb/credentials.json exists
               or a sign-in happens in-session — the shipper loop)

gilb-helper ─► gilb-config
              (privileged daemon over unix-socket IPC;
               currently a skeleton — Ping/Pong handshake, rmp-serde frames)

gilb-meeting ► (standalone: MeetingDetector trait + MeetingEvent enum
               + in-memory MockDetector; native detectors land later)

gilb-pipeline ► gilb-db, gilb-events, gilb-meeting, gilb-record
              (app-agnostic meeting bridge: detector → recorder → meetings
               rows, driven through the MeetingUi trait the shell implements;
               returns PipelineHandles for the stop-countdown / detection
               toggle channels)

apps/gilb-app-tauri/src-tauri ─► gilb-engine + gilb-config + gilb-events
              + gilb-pipeline
              (Tauri commands: start_capture/stop_capture/status/
               open_privacy_pane; AppState holds an Arc<Engine>;
               meeting.rs is the Tauri MeetingUi adapter over gilb-pipeline)

apps/gilb-mcp ─► gilb-core + gilb-config + gilb-db
              (read-only MCP server over ~/.gilb/db.sqlite, stdio
               transport; gilb_* tools for Claude Code.
               LLM-facing contract — apps/gilb-mcp/help.md)
```

Platform split: `crates/gilb-a11y/src/platform/{macos,windows,unsupported}`
is selected via `cfg(target_os = ...)`. `macos/` is broken into sub-modules
(`event_tap`, `ax_worker`, `focus`, `keyboard`, `pasteboard`,
`normalizer`, `permissions`, `ffi`, `platform`). `windows.rs` is a stub.

### Capture → DB data flow

1. UI (or the CLI) calls `Engine::start_capture(settings)`.
2. `Engine` inserts a row into `sessions`, opens an mpsc channel
   (`ACTION_CHANNEL_CAPACITY = 4096`), spawns a writer task, and calls
   `CapturePlatform::start(StartContext { session_id, action_tx,
   event_bus, settings })`.
3. The platform capture (on macOS — CGEventTap + AX) pushes
   `gilb_core::Action`s into `action_tx`.
4. The writer task in `gilb-engine` buffers messages and commits them in
   one transaction via `gilb_db::write_batch` once the buffer fills
   (`WRITER_BATCH_MAX`) or a flush tick elapses (`WRITER_FLUSH_INTERVAL`);
   it falls back to per-row `insert_action` if a batch transaction fails.
5. `Engine::stop_capture` stops the worker → sends shutdown to the
   writer → closes the `sessions` row with `stop_reason`.

Permission / health events flow in parallel through `EventBus`
(`tokio::sync::broadcast` channels inside `gilb-events`).

### Database

`gilb-db::open_db` opens SQLite with a fixed PRAGMA set (WAL,
`synchronous=NORMAL`, `cache_size=-65536`, `mmap_size=256MB`,
`busy_timeout=5s`, `wal_autocheckpoint=4000`) and applies migrations
from `crates/gilb-db/migrations/`. The v0 schema is `sessions`,
`actions`, `tree_snapshots`, `app_budgets`, `health_events` (see
`0001_init.sql`). Multimodal tables (`frames` / `elements` /
`ocr_text` / `audio_*`) are added by later migrations, **not**
pre-created.

**Never edit a migration that has shipped or been applied anywhere** —
not even a comment. sqlx checksums the whole file; changing it makes
every DB that already ran it refuse to start ("migration N was
previously applied but has been modified"). Any change is a **new**
migration (`000N+1`); fix stale docs in code/`help.md`, never in the
applied `.sql`.

**Second consumer of the schema — `apps/gilb-mcp`.** It reads the
same `~/.gilb/db.sqlite` and exposes `gilb_*` tools to Claude Code
with a stable user-facing contract in `apps/gilb-mcp/help.md`
(column names and semantics for `actions`, `kind` values, password
masking, `range` formats). Any migration that changes the shape of
`actions`, `sessions`, or `health_events` must also pass through
`gilb-mcp` (SQL queries + `help.md`).

## Repo layout

- `Cargo.toml` — workspace root (members =
  `apps/gilb-app-tauri/src-tauri` + `apps/gilb-mcp` + `crates/*`).
  Shared dependency versions live in `[workspace.dependencies]`;
  each crate references them via `workspace = true`.
- `apps/` — runnable binaries (Tauri shell, MCP server).
- `crates/` — library crates.
- `reference/` — third-party projects we study for ideas. **Not our
  code**, **not committed** (see `.gitignore`). Each subdir is
  typically its own git repo (an upstream clone).

## Working with `reference/`

- `reference/` is gitignored. Updating is a normal `git pull` inside
  each clone: `cd reference/<project> && git pull`.
- If you need to bring code from a reference project into Gilb, copy
  it explicitly into our sources and cite the origin in the commit
  message.

## macOS specifics

- Bundle ID: `app.farol.gilb`. Apple Developer Team ID: `83856566PM`.
  The signing identity is set in
  `apps/gilb-app-tauri/src-tauri/tauri.conf.json`.
- `Info.plist`: `LSUIElement=1` (no Dock icon),
  `NSAccessibilityUsageDescription` +
  `NSInputMonitoringUsageDescription` +
  `NSAppleEventsUsageDescription`.
- `entitlements.plist`: hardened runtime,
  `automation.apple-events`, `disable-library-validation` (required
  for the AX FFI). JIT / unsigned-exec are off — do not enable them
  without a clear reason.
- AX / Input Monitoring permissions are granted by the user in
  System Settings; the `open_privacy_pane` command in `lib.rs` opens
  the relevant pane via an `x-apple.systempreferences:` URL.
- macOS-only crates are wired through
  `[target.'cfg(target_os = "macos")'.dependencies]` in `gilb-a11y`
  (`core-graphics`, `core-foundation`, `accessibility-sys`,
  `objc2*`).
