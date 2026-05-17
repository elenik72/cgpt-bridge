#!/usr/bin/env bash
#
# cgpt-bridge — macOS Native Messaging host installer.
#
# Writes the Chrome Native Messaging manifest to:
#   ~/Library/Application Support/Google/Chrome/NativeMessagingHosts/com.cgpt_bridge.host.json
#
# The extension id is derived from the `key` field in extension/manifest.json
# and is therefore the same on every machine that loads this unpacked
# extension. You only need to pass it explicitly if you replace the key.
#
# Usage:
#   ./install/macos/install-host.sh [<chrome-extension-id>] [--no-build]

set -euo pipefail

# Pinned extension id derived from the `key` in extension/manifest.json.
# Override by passing a different id as the first positional argument (only
# needed if you regenerate the keypair).
DEFAULT_EXT_ID="oplkebjcjmifidmnbehpadfakodjjoge"

EXT_ID=""
SKIP_BUILD=false
for arg in "$@"; do
  case "$arg" in
    --no-build) SKIP_BUILD=true ;;
    --*)        echo "unknown flag: $arg" >&2; exit 2 ;;
    *)          if [[ -z "$EXT_ID" ]]; then EXT_ID="$arg"; fi ;;
  esac
done
if [[ -z "$EXT_ID" ]]; then
  EXT_ID="$DEFAULT_EXT_ID"
  echo "Using pinned extension id: $EXT_ID"
fi

# Sanity-check the extension id shape (Chrome uses 32 lowercase letters a-p).
if [[ ! "$EXT_ID" =~ ^[a-p]{32}$ ]]; then
  echo "warning: '$EXT_ID' does not look like a Chrome extension id (32 chars, a-p)." >&2
  echo "         Proceeding anyway, but double-check chrome://extensions." >&2
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HOST_BIN="$REPO_ROOT/target/release/cgpt-bridge-host"

resolve_cargo() {
  if command -v cargo >/dev/null 2>&1; then
    command -v cargo
    return 0
  fi
  for candidate in \
    "$HOME/.cargo/bin/cargo" \
    "/opt/homebrew/bin/cargo" \
    "/usr/local/bin/cargo"; do
    if [[ -x "$candidate" ]]; then
      echo "$candidate"
      return 0
    fi
  done
  return 1
}

if [[ "$SKIP_BUILD" == false ]]; then
  if ! CARGO_BIN="$(resolve_cargo)"; then
    cat >&2 <<EOM
error: 'cargo' not found in PATH or common install locations.

Tried: PATH, ~/.cargo/bin/cargo, /opt/homebrew/bin/cargo, /usr/local/bin/cargo

Install Rust (https://rustup.rs) or run with --no-build after building manually:
  cargo build --release -p cgpt-bridge-host
EOM
    exit 4
  fi
  echo "Building release host binary with: $CARGO_BIN"
  (cd "$REPO_ROOT" && "$CARGO_BIN" build --release -p cgpt-bridge-host)
fi

if [[ ! -x "$HOST_BIN" ]]; then
  echo "error: host binary not found or not executable: $HOST_BIN" >&2
  echo "       run without --no-build to build it, or run 'cargo build --release -p cgpt-bridge-host'." >&2
  exit 3
fi

MANIFEST_NAME="com.cgpt_bridge.host"
MANIFEST_DIR="$HOME/Library/Application Support/Google/Chrome/NativeMessagingHosts"
MANIFEST_PATH="$MANIFEST_DIR/$MANIFEST_NAME.json"

mkdir -p "$MANIFEST_DIR"

# Atomic write via a temp file so an interrupted install never leaves a
# half-written manifest.
TMP_MANIFEST="$(mktemp "${TMPDIR:-/tmp}/cgpt-bridge-manifest.XXXXXX")"
trap 'rm -f "$TMP_MANIFEST"' EXIT

cat > "$TMP_MANIFEST" <<JSON
{
  "name": "$MANIFEST_NAME",
  "description": "cgpt-bridge native messaging host",
  "path": "$HOST_BIN",
  "type": "stdio",
  "allowed_origins": [
    "chrome-extension://$EXT_ID/"
  ]
}
JSON

mv "$TMP_MANIFEST" "$MANIFEST_PATH"
chmod 644 "$MANIFEST_PATH"

echo "Installed manifest:"
echo "  $MANIFEST_PATH"
echo "Host binary:"
echo "  $HOST_BIN"
echo ""
echo "Next steps:"
echo "  1. Reload the extension at chrome://extensions (↻ on cgpt-bridge)."
echo "  2. Open the service worker DevTools (link on the extension card)."
echo "  3. In its Console, run:  pingNative()"
echo ""
echo "If pingNative() fails, verify the extension id matches the one in"
echo "allowed_origins inside $MANIFEST_PATH."
