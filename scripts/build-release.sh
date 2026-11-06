#!/usr/bin/env bash
#
# cgpt-bridge — release bundler.
#
# Builds the CLI, the host and the gateway in release mode, builds the
# extension, and packs everything into
#   dist/cgpt-bridge-<version>-<build>-<os>-<arch>.tar.gz
# (DIST_DIR=<dir> stages somewhere else than dist/.) <build> is the commit's
# short SHA (scripts/build-id.sh; CGPT_BUILD=<id> overrides it; `-dirty` is
# appended when the tree has uncommitted changes); the binaries and the
# extension carry the same id.
#
# The tarball is self-contained: drop it on another machine, untar, run
# ./install.sh (macOS), load the unpacked extension at chrome://extensions.
# Windows has its own bundler: scripts/build-release.ps1.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

VERSION="$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)"
if [[ -z "$VERSION" ]]; then
  echo "error: could not read workspace version from Cargo.toml" >&2
  exit 1
fi

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
case "$OS" in
  darwin) OS_TAG="macos" ;;
  linux)  OS_TAG="linux" ;;
  *)      echo "error: unsupported OS: $OS" >&2; exit 1 ;;
esac
ARCH="$(uname -m)"

# Read by option_env! in the protocol crate and by the extension build.
CGPT_BUILD="$("$SCRIPT_DIR/build-id.sh")"
export CGPT_BUILD

BUNDLE_NAME="cgpt-bridge-${VERSION}-${CGPT_BUILD}-${OS_TAG}-${ARCH}"
DIST_DIR="${DIST_DIR:-dist}"
STAGE_DIR="${DIST_DIR}/${BUNDLE_NAME}"
TARBALL="${DIST_DIR}/${BUNDLE_NAME}.tar.gz"

echo "==> Cleaning previous bundle"
rm -rf "$STAGE_DIR" "$TARBALL"
mkdir -p "$STAGE_DIR"

# ---------------------------------------------------------------------------
# Rust release build (cli + host + gateway).
# ---------------------------------------------------------------------------
resolve_cargo() {
  if command -v cargo >/dev/null 2>&1; then command -v cargo; return; fi
  for c in "$HOME/.cargo/bin/cargo" /opt/homebrew/bin/cargo /usr/local/bin/cargo; do
    [[ -x "$c" ]] && { echo "$c"; return; }
  done
  return 1
}
CARGO="$(resolve_cargo)" || { echo "error: cargo not found" >&2; exit 1; }
echo "==> Building Rust release binaries with $CARGO (build $CGPT_BUILD)"
"$CARGO" build --release --locked -p cgpt-bridge-cli -p cgpt-bridge-host -p cgpt-gateway

# ---------------------------------------------------------------------------
# Extension build.
# ---------------------------------------------------------------------------
echo "==> Building Chrome extension"
if command -v npm >/dev/null 2>&1; then
  (cd extension && npm ci --silent --no-audit --no-fund && npm run build)
else
  echo "error: npm not found" >&2; exit 1
fi

# ---------------------------------------------------------------------------
# Stage layout.
# ---------------------------------------------------------------------------
echo "==> Staging bundle at $STAGE_DIR"
mkdir -p "$STAGE_DIR/bin" "$STAGE_DIR/extension" "$STAGE_DIR/install/macos" "$STAGE_DIR/install/linux"

cp target/release/cgpt              "$STAGE_DIR/bin/cgpt"
cp target/release/cgpt-bridge-host  "$STAGE_DIR/bin/cgpt-bridge-host"
cp target/release/cgpt-gateway      "$STAGE_DIR/bin/cgpt-gateway"
chmod +x "$STAGE_DIR/bin/"*

# Extension dist (background.js, content.js, manifest.json).
cp -R extension/dist/. "$STAGE_DIR/extension/"

# The macOS installer of the whole set (host, gateway, OpenCode provider and
# agent, LaunchAgent) sits at the top of the bundle.
if [[ "$OS_TAG" == "macos" ]]; then
  cp install/macos/install.sh install/macos/uninstall.sh \
     install/macos/com.cgpt-bridge.gateway.plist install/opencode-agent-cgpt.md \
     install/opencode-agent-cgpt-sub.md "$STAGE_DIR/"
  chmod +x "$STAGE_DIR/install.sh" "$STAGE_DIR/uninstall.sh"
fi

# Host-only installers (Linux, and the .dmg's Install.command) — point at the
# bundled binaries instead of target/release/.
sed 's|REPO_ROOT/target/release/cgpt-bridge-host|REPO_ROOT/bin/cgpt-bridge-host|g' \
    install/macos/install-host.sh > "$STAGE_DIR/install/macos/install-host.sh"
sed 's|REPO_ROOT/target/release/cgpt-bridge-host|REPO_ROOT/bin/cgpt-bridge-host|g' \
    install/linux/install-host.sh > "$STAGE_DIR/install/linux/install-host.sh"
chmod +x "$STAGE_DIR/install/macos/install-host.sh" "$STAGE_DIR/install/linux/install-host.sh"

# Patch the installers' SKIP_BUILD default: in a prebuilt bundle there is no
# Cargo workspace, so building from source would fail. Default to --no-build.
sed -i.bak 's|SKIP_BUILD=false|SKIP_BUILD=true|g' \
  "$STAGE_DIR/install/macos/install-host.sh" \
  "$STAGE_DIR/install/linux/install-host.sh"
rm -f "$STAGE_DIR/install/macos/install-host.sh.bak" "$STAGE_DIR/install/linux/install-host.sh.bak"

# Bundle-specific install README.
cat > "$STAGE_DIR/INSTALL.md" <<'INSTALL'
# cgpt-bridge — install on a new machine

This bundle contains:

- `bin/cgpt-gateway` — local OpenAI-compatible server for OpenCode. The
  gateway is for macOS and Windows only (Windows has its own bundle). On
  Linux it is built but not supported and has no installer: Linux gets the
  host and the CLI.
- `bin/cgpt-bridge-host` — the Chrome Native Messaging host.
- `bin/cgpt` — the CLI (`cgpt ask`, `cgpt agent`).
- `extension/` — the unpacked Chrome extension.
- macOS: `install.sh`, `uninstall.sh`, the gateway LaunchAgent template,
  `opencode-agent-cgpt.md` (the OpenCode agent `cgpt`) and
  `opencode-agent-cgpt-sub.md` (`cgpt-sub`, the only subagent `cgpt` may
  start).
- `install/linux/install-host.sh` — host-only installer for Linux.

## macOS

1. Run `./install.sh` (no sudo). It copies everything to
   `~/Library/Application Support/cgpt-bridge/`, registers the native host
   with Chrome, creates the gateway config with its token, adds provider
   `cgpt` to `~/.config/opencode/opencode.json` (the previous file is kept as
   `opencode.json.bak`), adds the OpenCode agents `cgpt` and `cgpt-sub`
   (`~/.config/opencode/agents/`; `--no-agent` skips them) and starts
   the gateway at login via
   `~/Library/LaunchAgents/com.cgpt-bridge.gateway.plist`.
2. Chrome → `chrome://extensions` → *Developer mode* → *Load unpacked* →
   `~/Library/Application Support/cgpt-bridge/extension` (Cmd+Shift+G in the
   file dialog). The extension id is pinned by the manifest `key`.
   After an update press ↻ on the extension card: Chrome keeps running the
   old native host until then. Then close the old cgpt-bridge window.
3. Open https://chatgpt.com and log in. Keep chat history on and do not use
   a temporary chat.
4. In OpenCode press Tab to switch to the agent `cgpt` (or run
   `opencode --agent cgpt`). It uses the model `cgpt/chatgpt-web` and asks
   before every command, file edit, web access, MCP tool and skill. It is
   not the default
   agent: pick it with Tab each time. The model `cgpt/chatgpt-web` chosen in
   another agent works too, but nothing asks there.
5. On the first request the extension opens its own ChatGPT window without
   focus; OpenCode never uses your own ChatGPT tabs. Leave that window open,
   do not type in it, and click once inside its page (again after it was
   fully reloaded) so it keeps streaming in the background. Each OpenCode
   session becomes a chat in your ChatGPT history.

Uninstall: `~/Library/Application Support/cgpt-bridge/uninstall.sh`, then
remove the extension at `chrome://extensions`.

See README.md (section "Install") for OpenCode permissions, skills and
troubleshooting. This bundle is for Apple silicon (arm64); on an Intel Mac
build from source.

## Linux (host and CLI only)

1. `./install/linux/install-host.sh` (`--chromium` for Chromium).
2. Load `extension/` at `chrome://extensions` as above.
3. `cp bin/cgpt ~/.local/bin/cgpt`, then `cgpt ask "hello"`.
INSTALL

# Copy top-level project docs that are useful on the install machine.
cp README.md  "$STAGE_DIR/README.md"  2>/dev/null || true
cp -R docs    "$STAGE_DIR/docs"       2>/dev/null || true

# A small version file lets `cgpt doctor` (future) and humans see the bundle.
echo "$VERSION"                 > "$STAGE_DIR/VERSION"
echo "$OS_TAG-$ARCH"             > "$STAGE_DIR/PLATFORM"

# ---------------------------------------------------------------------------
# Tar it up.
# ---------------------------------------------------------------------------
echo "==> Creating tarball $TARBALL"
tar -C "$DIST_DIR" -czf "$TARBALL" "$BUNDLE_NAME"

# ---------------------------------------------------------------------------
# Checksum: SHA256SUMS next to the tarball, in `sha256sum` format
# (lowercase hex, two spaces, file name); verify with `shasum -a 256 -c`.
# ---------------------------------------------------------------------------
echo "==> Writing ${DIST_DIR}/SHA256SUMS"
(
  cd "$DIST_DIR"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${BUNDLE_NAME}.tar.gz" > SHA256SUMS
  else
    shasum -a 256 "${BUNDLE_NAME}.tar.gz" > SHA256SUMS
  fi
)

# ---------------------------------------------------------------------------
# Summary.
# ---------------------------------------------------------------------------
echo ""
echo "Built bundle: $TARBALL"
echo "Checksum:     ${DIST_DIR}/SHA256SUMS"
echo "Staged tree:  $STAGE_DIR"
echo ""
echo "Smoke check on this machine:"
echo "  $STAGE_DIR/bin/cgpt --version"
echo "Copy to the other machine and follow $STAGE_DIR/INSTALL.md."
