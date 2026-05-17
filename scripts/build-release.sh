#!/usr/bin/env bash
#
# cgpt-bridge — release bundler.
#
# Builds the CLI and host in release mode, builds the extension, and packs
# everything into  dist/cgpt-bridge-<version>-<os>-<arch>.tar.gz
#
# The tarball is self-contained: drop it on another machine, untar, run the
# matching install script, load the unpacked extension at chrome://extensions.

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

BUNDLE_NAME="cgpt-bridge-${VERSION}-${OS_TAG}-${ARCH}"
STAGE_DIR="dist/${BUNDLE_NAME}"
TARBALL="dist/${BUNDLE_NAME}.tar.gz"

echo "==> Cleaning previous bundle"
rm -rf "$STAGE_DIR" "$TARBALL"
mkdir -p "$STAGE_DIR"

# ---------------------------------------------------------------------------
# Rust release build (cli + host).
# ---------------------------------------------------------------------------
resolve_cargo() {
  if command -v cargo >/dev/null 2>&1; then command -v cargo; return; fi
  for c in "$HOME/.cargo/bin/cargo" /opt/homebrew/bin/cargo /usr/local/bin/cargo; do
    [[ -x "$c" ]] && { echo "$c"; return; }
  done
  return 1
}
CARGO="$(resolve_cargo)" || { echo "error: cargo not found" >&2; exit 1; }
echo "==> Building Rust release binaries with $CARGO"
"$CARGO" build --release -p cgpt-bridge-cli -p cgpt-bridge-host

# ---------------------------------------------------------------------------
# Extension build.
# ---------------------------------------------------------------------------
echo "==> Building Chrome extension"
if command -v npm >/dev/null 2>&1; then
  (cd extension && npm install --silent --no-audit --no-fund && npm run build)
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
chmod +x "$STAGE_DIR/bin/"*

# Extension dist (background.js, content.js, manifest.json).
cp -R extension/dist/. "$STAGE_DIR/extension/"

# Installers — point at the bundled binaries instead of target/release/.
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

This tarball contains:

- `bin/cgpt` — the CLI.
- `bin/cgpt-bridge-host` — the Chrome Native Messaging host.
- `extension/` — the unpacked Chrome extension (load this folder at chrome://extensions).
- `install/macos/install-host.sh` and `install/linux/install-host.sh` — Native Messaging manifest installers.

## Steps

1. **Load the extension.**
   - Open `chrome://extensions`.
   - Enable *Developer mode* (top right).
   - Click *Load unpacked*, choose the `extension/` folder from this bundle.
   - The extension id is fixed via the `key` field in the manifest, so it is
     the same on every machine. You do not need to copy it — the installer
     below already knows it.

2. **Install the Native Messaging manifest.**
   - macOS:  `./install/macos/install-host.sh`
   - Linux:  `./install/linux/install-host.sh`
     Use `--chromium` if you loaded the extension in Chromium instead of Chrome.

   The script writes a manifest pinning the host to the pinned extension id
   and pointing it at `bin/cgpt-bridge-host` in this folder. Do not move the
   folder after running the installer — the manifest stores an absolute path.

3. **Reload the extension.** Click the refresh icon on the extension card.

4. **Verify.** Open the extension's service-worker DevTools (link on the
   extension card), and in its Console run:
   ```
   pingNative()
   ```
   You should see `{ type: "pong", ... }`.

5. **Drop `bin/cgpt` somewhere on your `PATH`** (or call it via its absolute
   path):
   ```
   cp bin/cgpt /usr/local/bin/cgpt          # macOS
   cp bin/cgpt ~/.local/bin/cgpt            # Linux (typical user-local)
   ```

6. **Smoke test.** Open https://chatgpt.com in any window, click anywhere
   inside that tab once (required for the background-tab keep-alive). Then:
   ```
   cgpt ask "hello"
   cgpt agent --yolo "list files"
   ```

## Hidden-tab caveat

The bridge can drive ChatGPT while the tab is in the background. To prevent
Chrome from throttling timers on the hidden tab, the extension plays a
silent audio stream when sending a prompt. Chrome's autoplay policy requires
the page to have received at least one user gesture (a click, keypress,
etc.) since it loaded. If you reopen the ChatGPT tab fresh and immediately
hide it without clicking, the audio context cannot resume and you'll see
slow responses; a single click anywhere in the tab unblocks it for the
session.

## Uninstall

- Delete the Native Messaging manifest:
  - macOS:  `rm "$HOME/Library/Application Support/Google/Chrome/NativeMessagingHosts/com.cgpt_bridge.host.json"`
  - Linux:  `rm "$HOME/.config/google-chrome/NativeMessagingHosts/com.cgpt_bridge.host.json"` (or `chromium`)
- Remove the extension at `chrome://extensions`.
- Delete this folder.
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
tar -C dist -czf "$TARBALL" "$BUNDLE_NAME"

# ---------------------------------------------------------------------------
# Summary.
# ---------------------------------------------------------------------------
echo ""
echo "Built bundle: $TARBALL"
echo "Staged tree:  $STAGE_DIR"
echo ""
echo "Smoke check on this machine:"
echo "  $STAGE_DIR/bin/cgpt --version"
echo "Copy to the other machine and follow $STAGE_DIR/INSTALL.md."
