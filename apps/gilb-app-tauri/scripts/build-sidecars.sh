#!/usr/bin/env bash
# Build the gilb-mcp sidecar and stage it under the name Tauri expects
# for `bundle.externalBin` (binaries/<base>-<host-triple>). The host triple
# is detected from rustc so the same script works on Apple Silicon
# (aarch64-apple-darwin) and Intel (x86_64-apple-darwin) without touching
# tauri.conf.json.
#
# After `tauri build` finishes, the sidecar is copied to
# gilb.app/Contents/MacOS/gilb-mcp, signed with the same Developer ID,
# and included in the notarization submission alongside the main binary.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE_ROOT="$(cd "$APP_DIR/../.." && pwd)"
TRIPLE="$(rustc -vV | sed -n 's|host: ||p')"

cd "$WORKSPACE_ROOT"
cargo build --release -p gilb-mcp

mkdir -p "$APP_DIR/src-tauri/binaries"
cp "$WORKSPACE_ROOT/target/release/gilb-mcp" \
   "$APP_DIR/src-tauri/binaries/gilb-mcp-$TRIPLE"
