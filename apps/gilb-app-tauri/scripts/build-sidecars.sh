#!/usr/bin/env bash
# Build the sidecar CLIs (gilb-mcp) and stage them under the
# names Tauri expects for `bundle.externalBin` (binaries/<base>-<target-triple>).
#
# The sidecar ships inside the .app:
#   <App>.app/Contents/MacOS/gilb-mcp — read-only MCP over the recorder DB,
#   pointed at by the operator's Claude Code --mcp-config.
#
# Target detection — TAURI_ENV_TARGET_TRIPLE is set by `tauri build`
# when invoked with `--target`. For plain host builds (no flag) we
# fall back to the host triple from rustc.
#
# For `--target universal-apple-darwin` the situation is subtle:
# Tauri runs two separate cargo builds (one per arch) — each one's
# tauri-build invocation expects `binaries/gilb-mcp-<that-arch>` to
# exist at *that* moment. Then the bundler lipos the main binary and
# expects `binaries/gilb-mcp-universal-apple-darwin` at bundle time.
# So we stage all three files: two per-arch and one fat.
#
# After `tauri build` finishes, the sidecar (universal variant) is
# copied into <App>.app/Contents/MacOS/gilb-mcp, signed with the
# same Developer ID, and included in the notarization submission.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE_ROOT="$(cd "$APP_DIR/../.." && pwd)"

TARGET="${TAURI_ENV_TARGET_TRIPLE:-$(rustc -vV | sed -n 's|host: ||p')}"
BINARIES_DIR="$APP_DIR/src-tauri/binaries"
mkdir -p "$BINARIES_DIR"

# Windows cargo artifacts — and the name Tauri expects for externalBin —
# carry a `.exe` suffix; other platforms don't.
case "$TARGET" in
  *windows*) EXE=".exe" ;;
  *) EXE="" ;;
esac

# Sidecar crates to build + stage. Each is its own workspace bin.
SIDECARS=(gilb-mcp)

build_one() {
  cd "$WORKSPACE_ROOT"
  cargo build --release --target "$1" -p gilb-mcp
}

case "$TARGET" in
  universal-apple-darwin)
    build_one aarch64-apple-darwin
    build_one x86_64-apple-darwin
    for bin in "${SIDECARS[@]}"; do
      cp "$WORKSPACE_ROOT/target/aarch64-apple-darwin/release/$bin" \
         "$BINARIES_DIR/$bin-aarch64-apple-darwin"
      cp "$WORKSPACE_ROOT/target/x86_64-apple-darwin/release/$bin" \
         "$BINARIES_DIR/$bin-x86_64-apple-darwin"
      lipo -create -output "$BINARIES_DIR/$bin-$TARGET" \
        "$WORKSPACE_ROOT/target/aarch64-apple-darwin/release/$bin" \
        "$WORKSPACE_ROOT/target/x86_64-apple-darwin/release/$bin"
    done
    ;;
  *)
    build_one "$TARGET"
    for bin in "${SIDECARS[@]}"; do
      cp "$WORKSPACE_ROOT/target/$TARGET/release/$bin$EXE" \
         "$BINARIES_DIR/$bin-$TARGET$EXE"
    done
    ;;
esac
