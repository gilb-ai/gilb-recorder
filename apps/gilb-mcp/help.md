# gilb MCP — quick reference for LLMs

You are talking to `gilb-mcp`, a read-only MCP server over the user's local
activity log. Each captured action is one row.

## What the data looks like

The main table is `actions`. One row per atomic user action:

- `id` — primary key (i64). Stable across reads.
- `session_id` — FK to `sessions.id`. Group by this to scope queries
  to one Start → Stop recording.
- `captured_at` — ISO 8601 UTC timestamp (e.g. `2026-05-22T22:04:35.802740Z`)
- `kind` — one of:
  - `click` — mouse button down. Has `element_role`, `element_name`,
    `element_value`, `element_id` when the AX worker enriched the
    click with element context. `extra.button` / `extra.x` / `extra.y` are
    the button + screen point; `extra.click_count` is the burst count
    (1=single, 2=double, 3=triple, …); `extra.modifiers` is a held-modifier
    bitfield: SHIFT=1, CTRL=2, OPT=4, CMD=8, CAPS=16, FN=32 (OR-combined;
    0 = none).
  - `text` — text the user typed, already after a 300 ms debounce (one row =
    one "burst" of typing). `text_content` is the typed string.
  - `key` — non-printable navigation/editing key (Enter, Tab, Backspace,
    arrows, etc.). `extra.key` names which one; `extra.modifiers` is the same
    held-modifier bitfield as `click` (SHIFT=1, CTRL=2, OPT=4, CMD=8, CAPS=16,
    FN=32).
  - `scroll` — wheel event. `extra.delta_x`, `extra.delta_y`.
  - `clipboard` — `text_content` is the clipboard string (PII-redacted,
    capped at 64 KiB with a `…[truncated N bytes]` marker for huge copies).
    `clipboard_op` is the op ("copy"; cut/paste detection deferred). A
    `content_hash` of the raw text exists in the DB for server-side
    copy↔paste linking but is NOT returned by these tools (hashing
    pre-redaction text would make short secrets guessable).
  - `focus_change` — frontmost app changed; useful as an activity boundary.
  - `system` — lifecycle marker. `extra.system` is `idle_start` / `idle_end` /
    `alive` (lock/unlock/recording arrive later). Segment cases/sessions on these.
- `app_name`, `app_bundle_id`, `window_title` — foreground app context.
- `browser_url` — URL of the focused tab when the foreground app is a
  known browser (Chrome / Safari / Firefox / Edge / Brave / Arc /
  Chromium / Vivaldi / Opera / Zen / Comet). `NULL` for non-browser
  apps. Sourced from `AXDocument` first, then a shallow walk for an
  address-bar-shaped `AXTextField` / `AXComboBox`. Useful for
  disambiguating browser activity (`crunchbase.com/organization/<slug>`
  vs `crunchbase.com/home`) without OCR.
- `text_content`, `element_value` — **already replaced with `'[masked]'`**
  when `password_flag = true`. Do not try to recover masked content.

Other tables:

- `sessions` — Start → Stop boundaries.
- `health_events` — capture diagnostics (dropped events, sleep/wake, AX
  timeouts).
- `tree_snapshots` — one row per focused-window change that the
  snapshotter decided was substantively different (SimHash-deduped) from
  the previous one. Captures the AX tree of the focused window as a JSON
  blob (`role`/`name`/`value`/`depth` per node, depth- and node-bounded).
  Correlate to `actions` by `(session_id, captured_at)` within a short
  window. Useful when you need to know **what was on screen** at a
  specific moment, not just what the user clicked. List metadata via
  `gilb_list_tree_snapshots`, fetch one tree by id via
  `gilb_get_tree_snapshot`.
- `meetings` — one row per recorded call/meeting (screen video + audio).
  Independent of `sessions`. Has `app`, `started_at`/`ended_at`, `status`.
- `meeting_transcripts` — on-device Whisper transcript, 1:1 with a meeting.
  The audio is captured as two channels (the local mic and the remote call
  audio) and transcribed separately, so every segment is tagged with a
  **speaker**: `"Me"` (local participant) or `"Others"` (remote). List
  meetings via `gilb_list_meetings`, read one transcript via
  `gilb_get_transcript`.

## How to query

Always start broad, then narrow:

1. **What was I doing recently?** → `gilb_recent_actions` with
   `range = {"last": "10m"}`.
2. **What apps did I use today?** → `gilb_list_apps` with
   `range = {"last": "today"}`.
3. **Did I work on X?** → `gilb_search_actions` with `q: "X"`,
   optionally `app: "Slack"` / `kind: "text"`.
4. **Summary of my day** → `gilb_activity_summary` with
   `range = {"last": "today"}`.
5. **Why is the log empty around 14:00?** → `gilb_list_health_events`
   (likely sleep/wake or AX timeout).
6. **What was said in my last meeting?** → `gilb_list_meetings` to find
   the id, then `gilb_get_transcript` with that `meeting_id`.

## `range` parameter

Accepted forms for the optional `range` argument:

| JSON | Meaning |
|---|---|
| `{"last": "10m"}` | last 10 minutes |
| `{"last": "2h"}` | last 2 hours |
| `{"last": "1d"}` | last day |
| `{"last": "today"}` | since local midnight |
| `{"last": "yesterday"}` | since midnight yesterday |
| `{"from": "2026-05-22T20:00:00Z", "to": "2026-05-22T21:00:00Z"}` | explicit |

Units: `s`/`m`/`h`/`d`/`w`. If unset, each tool picks a sensible default
(`gilb_recent_actions` defaults to 10 minutes; aggregate tools default to
24 hours).

## Available tools

| Tool | Purpose |
|---|---|
| `gilb_help` | this document |
| `gilb_list_sessions` | recording sessions (Start → Stop) with action_count |
| `gilb_get_session` | one session: per-kind breakdown + top apps |
| `gilb_list_apps` | apps used in a range, sorted by action volume |
| `gilb_recent_actions` | timeline of last N actions |
| `gilb_search_actions` | LIKE substring search over text/element/window |
| `gilb_activity_summary` | per-range aggregate: totals, per-kind, top apps, top text snippets |
| `gilb_list_tree_snapshots` | a11y tree snapshots metadata (id, app, window, browser_url, simhash, json_bytes) |
| `gilb_get_tree_snapshot` | full AX tree (parsed JSON) for one snapshot id |
| `gilb_list_health_events` | diagnostic events (drops, sleep/wake) |
| `gilb_list_meetings` | recorded meetings, newest first, with has_transcript flag |
| `gilb_get_transcript` | one meeting's transcript: text + speaker-tagged (Me/Others) segments |

## Output format

Each tool returns one `text` content with a JSON-pretty-printed body.
Parse it on your side; we keep field names stable across versions.

## Privacy

- Rows with `password_flag = true` have `text_content` and `element_value`
  pre-replaced with `'[masked]'` at the SQL layer. `gilb_search_actions`
  additionally **excludes** masked rows entirely from search results.
- A fixed block-list of password managers (1Password, Bitwarden,
  KeePassXC, macOS Keychain Access) is dropped at capture time and
  won't appear in the log at all.

## Style hints for summarizing

- Group actions inside the same app / window into one activity ("around
  22:05, edited a note in Notes about X"), don't enumerate every scroll.
- Use `focus_change` events as boundaries between activities.
- Quote short `text_content` fragments (≤80 chars) only when telling.
- Never reveal raw `[masked]` content; instead state "user entered a
  password" if relevant.
- Lead with a one-sentence summary, follow with time-anchored bullets.
