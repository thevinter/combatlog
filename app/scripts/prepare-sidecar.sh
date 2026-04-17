#!/usr/bin/env bash
# This downloads the Node.js binary for the current host target and copies 
# it into app/src-tauri/binaries/ with the target-triple suffix Tauri expects.
# Also copies the root parser-harness.js into src-tauri/resources/.

# This is necessary because the whole thing ig silly and we're running
# an obfuscated js, and the rust node runtimes can't JIT compile it properly 
# so this is the only way to avoid 10  min uploads at the cost of 800MB of sidecar.

set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(cd .. && pwd)"
BINARIES_DIR="src-tauri/binaries"
RESOURCES_DIR="src-tauri/resources"
NODE_VERSION="${NODE_VERSION:-v20.18.1}"

TARGET="${1:-}"
if [[ -z "$TARGET" ]]; then
  TARGET="$(rustc -vV | awk -F': ' '/^host/ {print $2}')"
fi

case "$TARGET" in
  x86_64-unknown-linux-gnu)
    NODE_ARCHIVE="node-${NODE_VERSION}-linux-x64.tar.xz"
    NODE_BIN_PATH="node-${NODE_VERSION}-linux-x64/bin/node"
    EXT=""
    ;;
  x86_64-pc-windows-msvc|x86_64-pc-windows-gnu)
    NODE_ARCHIVE="node-${NODE_VERSION}-win-x64.zip"
    NODE_BIN_PATH="node-${NODE_VERSION}-win-x64/node.exe"
    EXT=".exe"
    ;;
  aarch64-unknown-linux-gnu)
    NODE_ARCHIVE="node-${NODE_VERSION}-linux-arm64.tar.xz"
    NODE_BIN_PATH="node-${NODE_VERSION}-linux-arm64/bin/node"
    EXT=""
    ;;
  *)
    echo "unsupported target: $TARGET" >&2
    exit 2
    ;;
esac

mkdir -p "$BINARIES_DIR" "$RESOURCES_DIR"
cp "$ROOT/parser-harness.js" "$RESOURCES_DIR/parser-harness.js"

DEST="$BINARIES_DIR/node-${TARGET}${EXT}"
if [[ -f "$DEST" ]]; then
  echo "sidecar already present: $DEST"
  exit 0
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
url="https://nodejs.org/dist/${NODE_VERSION}/${NODE_ARCHIVE}"
echo "downloading $url"
curl -fsSL -o "$tmp/$NODE_ARCHIVE" "$url"
if [[ "$NODE_ARCHIVE" == *.zip ]]; then
  unzip -q "$tmp/$NODE_ARCHIVE" -d "$tmp"
else
  tar -xJf "$tmp/$NODE_ARCHIVE" -C "$tmp"
fi
install -m 0755 "$tmp/$NODE_BIN_PATH" "$DEST"
echo "staged sidecar: $DEST"
