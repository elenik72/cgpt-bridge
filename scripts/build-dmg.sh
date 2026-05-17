#!/usr/bin/env bash
#
# cgpt-bridge — macOS .dmg builder.
#
# Wraps the regular release bundle (scripts/build-release.sh) in a .dmg so
# end-users can drag-install instead of untarring. Chrome still has to load
# the unpacked extension manually; everything else is handled by the
# included Install.command.
#
# Output:
#   dist/cgpt-bridge-<version>-macos-<arch>.dmg

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: build-dmg.sh only runs on macOS (hdiutil required)" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Reuse the regular bundle. Idempotent: if you ran build-release.sh already
# the bundle is reused as-is.
# ---------------------------------------------------------------------------
./scripts/build-release.sh

VERSION="$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)"
ARCH="$(uname -m)"
BUNDLE_NAME="cgpt-bridge-${VERSION}-macos-${ARCH}"
BUNDLE_DIR="dist/${BUNDLE_NAME}"
DMG_NAME="${BUNDLE_NAME}.dmg"
DMG_PATH="dist/${DMG_NAME}"

if [[ ! -d "$BUNDLE_DIR" ]]; then
  echo "error: expected bundle dir $BUNDLE_DIR not found" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Stage a DMG-friendly layout. We want:
#   /Volumes/cgpt-bridge/
#     cgpt-bridge/                 ← the bundle folder (drag this somewhere)
#     Install.command              ← double-click to install NM manifest + /usr/local/bin/cgpt
#     README — Load extension first.txt
# ---------------------------------------------------------------------------
STAGE="dist/.dmg-stage-${BUNDLE_NAME}"
rm -rf "$STAGE"
mkdir -p "$STAGE"

cp -R "$BUNDLE_DIR" "$STAGE/cgpt-bridge"

# Install.command — bash with .command extension so Finder runs it in Terminal
# when the user double-clicks. Lives at the DMG root next to the bundle.
cat > "$STAGE/Install.command" <<'INSTALL_CMD'
#!/usr/bin/env bash
#
# cgpt-bridge installer (macOS).
# Double-click in Finder, or run from Terminal.

set -euo pipefail

cd "$(dirname "$0")"

# Resolve the staged bundle next to this script.
BUNDLE="$PWD/cgpt-bridge"
if [[ ! -d "$BUNDLE" ]]; then
  echo "error: 'cgpt-bridge' folder not found next to Install.command" >&2
  echo "       Did you copy both items out of the DMG together?" >&2
  read -n 1 -s -r -p "Press any key to close..."
  exit 1
fi

cat <<BANNER

╭───────────────────────────────────────────────────────────╮
│  cgpt-bridge — macOS installer                            │
╰───────────────────────────────────────────────────────────╯

This will:
  1) Copy bin/cgpt to /usr/local/bin/cgpt (sudo may prompt for password).
  2) Write the Chrome Native Messaging manifest pinning the host to the
     bundled extension id.

You still need to load the unpacked extension yourself in Chrome:
   chrome://extensions  →  Developer mode ON  →  Load unpacked
   then pick:
   $BUNDLE/extension

BANNER

read -p "Continue? [y/N] " ok
case "${ok:-}" in y|Y|yes|YES) ;; *) echo "Aborted."; exit 0 ;; esac

# 1) cgpt into PATH.
DEST="/usr/local/bin/cgpt"
mkdir -p "$(dirname "$DEST")" 2>/dev/null || true
if ! cp -f "$BUNDLE/bin/cgpt" "$DEST" 2>/dev/null; then
  echo "Need sudo to write $DEST"
  sudo cp -f "$BUNDLE/bin/cgpt" "$DEST"
fi
chmod +x "$DEST" 2>/dev/null || sudo chmod +x "$DEST"
echo "Installed CLI: $DEST"

# 2) Native messaging manifest (uses the bundle's own install-host.sh,
#    --no-build because the host binary is already in bin/).
"$BUNDLE/install/macos/install-host.sh" --no-build

cat <<DONE

✓ Installer finished.

Next:
  - Open Chrome → chrome://extensions → toggle Developer mode → Load unpacked
    → select:  $BUNDLE/extension
  - Reload the extension card once after install (↻).
  - In a terminal run:  cgpt ask "hello"

If anything looks off, see INSTALL.md inside the cgpt-bridge folder.

DONE
read -n 1 -s -r -p "Press any key to close..."
INSTALL_CMD
chmod +x "$STAGE/Install.command"

# Plain-text README at the DMG root.
cat > "$STAGE/README — Load extension first.txt" <<README
cgpt-bridge

1. Copy the entire "cgpt-bridge" folder somewhere (e.g. ~/Applications/).
2. Double-click Install.command.
3. Open Chrome → chrome://extensions → Developer mode → Load unpacked
   → pick the "extension" folder inside cgpt-bridge.
4. In Terminal:  cgpt ask "hello"

Full details: cgpt-bridge/INSTALL.md
README

# ---------------------------------------------------------------------------
# hdiutil — create the .dmg.
# ---------------------------------------------------------------------------
rm -f "$DMG_PATH"
hdiutil create \
  -volname "cgpt-bridge" \
  -srcfolder "$STAGE" \
  -ov \
  -fs HFS+ \
  -format UDZO \
  "$DMG_PATH" >/dev/null

# Tidy up the staging dir; keep the bundle (someone may still want it).
rm -rf "$STAGE"

echo ""
echo "Built DMG: $DMG_PATH"
echo "          ($(du -h "$DMG_PATH" | cut -f1))"
echo ""
echo "Mount it locally to smoke-test:"
echo "  open $DMG_PATH"
