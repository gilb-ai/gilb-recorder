# gilb-mcp

Stdio MCP server exposing the user's recorded activity (`<Documents>/Gilb/db.sqlite`)
to Claude Code / any MCP-aware client.

This crate is read-only — the writer is `gilb-app-tauri`. SQLite WAL mode
allows both processes to coexist.

## Build

```sh
# From the workspace root
cargo build --release -p gilb-mcp
# → target/release/gilb-mcp
```

## Register with Claude Code

```sh
claude mcp add gilb $(pwd)/target/release/gilb-mcp
```

Or hand-edit `~/.claude.json` / project-level `.mcp.json`:

```json
{
  "mcpServers": {
    "gilb": {
      "command": "/path/to/gilb-recorder/target/release/gilb-mcp",
      "args": [],
      "env": {}
    }
  }
}
```

### Custom DB path

```json
{
  "mcpServers": {
    "gilb": {
      "command": "/path/to/gilb-mcp",
      "env": { "GILB_DB": "/path/to/db.sqlite" }
    }
  }
}
```

By default the binary opens `<Documents>/Gilb/db.sqlite` (same path as the Tauri
app).

## Quick smoke test

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  | ./target/debug/gilb-mcp
```

You should see a `serverInfo` response with `"name": "gilb-mcp"`.

For interactive exploration: `npx @modelcontextprotocol/inspector
/path/to/gilb-mcp`.

## Tool surface (v0)

10 tools, all read-only:

| Tool | Description |
|---|---|
| `gilb_help` | Schema + tool reference (Markdown) for the LLM |
| `gilb_list_sessions` | List recording sessions with action counts |
| `gilb_get_session` | Per-kind breakdown + top apps for one session |
| `gilb_list_apps` | Apps used in a time range, sorted by action volume |
| `gilb_recent_actions` | Timeline of last N actions (default: 10 min) |
| `gilb_search_actions` | LIKE substring search |
| `gilb_activity_summary` | Aggregated overview for a range |
| `gilb_list_tree_snapshots` | a11y tree snapshot metadata (id, app, browser_url, simhash, json_bytes) |
| `gilb_get_tree_snapshot` | Full AX tree (parsed JSON) for one snapshot id |
| `gilb_list_health_events` | Capture diagnostics (not yet persisted — always empty) |

See `help.md` for query examples and the `range` parameter format.

## Logs

Errors and `tracing` output go to **stderr** — stdout is reserved for the
JSON-RPC framing. Set `RUST_LOG=debug` to crank verbosity. The Tauri app
already writes its own log to the data folder's `logs/`; `gilb-mcp` doesn't open that
file.

## What's not in v0

- FTS5-backed search (lands when the `actions_fts` migration is applied)
- Write tools (pause capture, delete actions) — would need cooperation with
  the running gilb-app
- Sidecar bundling inside `.app` — separate concern, see project plan
