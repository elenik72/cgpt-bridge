#!/usr/bin/env bash
#
# cgpt-bridge — checks for the macOS installer (run by CI on macos-latest and
# locally). Everything happens under a temporary HOME: the real ~/Library,
# Chrome profile, LaunchAgents and opencode.json are never touched, and the
# real launchctl is never called: install.sh / uninstall.sh run either with
# --no-launchctl or with a stub `launchctl` first on PATH that starts and
# stops the gateway from the plist the way launchd would (case E).
#
# Usage:
#   ./scripts/check-install-macos.sh <unpacked-bundle-dir>
#
# The bundle is what scripts/build-release.sh stages under dist/.

set -euo pipefail

if [[ $# -ne 1 || ! -d "$1" ]]; then
  echo "usage: $0 <unpacked-bundle-dir>" >&2
  exit 2
fi
BUNDLE="$(cd "$1" && pwd)"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/cgpt-install-check.XXXXXX")"
cleanup() {
  [[ -f "$WORK/launchd.pid" ]] && kill "$(cat "$WORK/launchd.pid")" 2>/dev/null
  [[ -n "${NC_PID:-}" ]] && kill "$NC_PID" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT
export HOME="$WORK/home"
unset XDG_CONFIG_HOME

APP="$HOME/Library/Application Support/cgpt-bridge"
GW="$HOME/Library/Application Support/cgpt-gateway"
NM_DIR="$HOME/Library/Application Support/Google/Chrome/NativeMessagingHosts"
NM="$NM_DIR/com.cgpt_bridge.host.json"
AGENTS="$HOME/Library/LaunchAgents"
PLIST="$AGENTS/com.cgpt-bridge.gateway.plist"
OC_DIR="$HOME/.config/opencode"
OC="$OC_DIR/opencode.json"
OC_AGENTS="$OC_DIR/agents"
OC_AGENT="$OC_AGENTS/cgpt.md"

FAILS=0
ok()   { echo "ok   - $*"; }
fail() { echo "FAIL - $*" >&2; FAILS=$((FAILS + 1)); }
check() { # check <description> <command...>
  local what="$1"; shift
  if "$@" >/dev/null 2>&1; then ok "$what"; else fail "$what"; fi
}
# Value at a key path of a JSON or plist file ("" when missing).
get() { plutil -extract "$2" raw -o - "$1" 2>/dev/null || true; }
# Sub-tree at a key path as compact JSON ("" when missing).
get_json() { plutil -extract "$2" json -o - "$1" 2>/dev/null || true; }
has() { plutil -extract "$2" raw -o - "$1" >/dev/null 2>&1 || plutil -extract "$2" json -o - "$1" >/dev/null 2>&1; }
eq() { # eq <description> <actual> <expected>
  if [[ "$2" == "$3" ]]; then ok "$1"; else fail "$1: got '$2', want '$3'"; fi
}
# Checksums of every file under HOME: an idempotent re-run leaves it unchanged.
snapshot() { (cd "$HOME" && find . -type f -print0 | sort -z | xargs -0 shasum -a 256); }

# The pinned id must be the one Chrome derives from the manifest `key`.
KEY="$(sed -n 's/.*"key": *"\([^"]*\)".*/\1/p' "$BUNDLE/extension/manifest.json")"
EXT_ID="$(printf '%s' "$KEY" | base64 -D | shasum -a 256 | cut -c1-32 | tr '0-9a-f' 'a-p')"

install() { "$BUNDLE/install.sh" --no-launchctl; }
uninstall() { "$APP/uninstall.sh" --no-launchctl; }

if [[ ! -x "$BUNDLE/install.sh" ]]; then
  echo "FAIL - $BUNDLE/install.sh is missing or not executable" >&2
  exit 1
fi

# Work on a copy marked the way a browser download is marked.
cp -R "$BUNDLE" "$WORK/bundle"
BUNDLE="$WORK/bundle"
xattr -w com.apple.quarantine "0081;00000000;Chrome;" "$BUNDLE/bin/cgpt-gateway"

# ---------------------------------------------------------------------------
echo "# A: existing opencode.json with another provider"
# ---------------------------------------------------------------------------
mkdir -p "$HOME" "$OC_DIR" "$NM_DIR" "$AGENTS"
cat > "$OC" <<'JSON'
{
  "$schema": "https://opencode.ai/config.json",
  "theme": "dark",
  "model": "anthropic/claude-sonnet-4-5",
  "provider": {
    "anthropic": { "options": { "apiKey": "sk-ant-foreign" } }
  },
  "instructions": ["Отвечай по-русски 🙂"]
}
JSON
cp "$OC" "$WORK/opencode.original.json"
echo '{"name":"com.other.host"}' > "$NM_DIR/com.other.host.json"
cp "$BUNDLE/com.cgpt-bridge.gateway.plist" "$AGENTS/com.other.agent.plist"

OUT="$(install 2>&1)" || { echo "$OUT" >&2; fail "install.sh exits 0"; exit 1; }
ok "install.sh exits 0"

for f in cgpt cgpt-bridge-host cgpt-gateway; do
  check "bin/$f installed and executable" test -x "$APP/bin/$f"
done
check "extension installed" test -f "$APP/extension/manifest.json"
check "installed binaries carry no quarantine flag" \
  test -z "$(xattr -p com.apple.quarantine "$APP/bin/cgpt-gateway" 2>/dev/null)"
check "uninstall.sh installed" test -x "$APP/uninstall.sh"

eq "host manifest name" "$(get "$NM" name)" "com.cgpt_bridge.host"
eq "host manifest path" "$(get "$NM" path)" "$APP/bin/cgpt-bridge-host"
eq "host manifest type" "$(get "$NM" type)" "stdio"
eq "host manifest allowed origin" "$(get "$NM" allowed_origins.0)" "chrome-extension://$EXT_ID/"
eq "host manifest has one origin" "$(get "$NM" allowed_origins.1)" ""

TOKEN="$(get "$GW/config.json" token)"
check "gateway token is 64 lowercase hex" grep -Eq '^[0-9a-f]{64}$' <<<"$TOKEN"
eq "gateway port" "$(get "$GW/config.json" port)" "8765"
eq "gateway config is owner-only" "$(stat -f '%Lp' "$GW/config.json")" "600"
if grep -q "$TOKEN" <<<"$OUT"; then fail "installer output does not show the token"; else ok "installer output does not show the token"; fi

check "LaunchAgent plist passes plutil -lint" plutil -lint "$PLIST"
eq "plist Label" "$(get "$PLIST" Label)" "com.cgpt-bridge.gateway"
eq "plist program" "$(get "$PLIST" ProgramArguments.0)" "$APP/bin/cgpt-gateway"
eq "plist RunAtLoad" "$(get "$PLIST" RunAtLoad)" "true"
eq "plist ThrottleInterval" "$(get "$PLIST" ThrottleInterval)" "60"
check "plist sets TMPDIR (socket path of the host)" test -n "$(get "$PLIST" EnvironmentVariables.TMPDIR)"
eq "plist log" "$(get "$PLIST" StandardErrorPath)" "$APP/gateway.log"

eq "cgpt npm" "$(get "$OC" provider.cgpt.npm)" "@ai-sdk/openai-compatible"
eq "cgpt baseURL" "$(get "$OC" provider.cgpt.options.baseURL)" "http://127.0.0.1:8765/v1"
eq "cgpt apiKey is the gateway token" "$(get "$OC" provider.cgpt.options.apiKey)" "$TOKEN"
check "model chatgpt-web" has "$OC" provider.cgpt.models.chatgpt-web
check "model local-title" has "$OC" provider.cgpt.models.local-title
eq "small_model set" "$(get "$OC" small_model)" "cgpt/local-title"
eq "model untouched" "$(get "$OC" model)" "anthropic/claude-sonnet-4-5"
eq "other provider untouched" "$(get_json "$OC" provider.anthropic)" \
  "$(get_json "$WORK/opencode.original.json" provider.anthropic)"
eq "other keys untouched" "$(get "$OC" theme)" "dark"
eq "UTF-8 kept" "$(get "$OC" instructions.0)" "Отвечай по-русски 🙂"
check "opencode.json.bak is the original" cmp -s "$OC.bak" "$WORK/opencode.original.json"
eq "opencode.json (holds the token) is owner-only" "$(stat -f '%Lp' "$OC")" "600"
eq "opencode.json.bak is owner-only" "$(stat -f '%Lp' "$OC.bak")" "600"
check "installer says opencode.json was made owner-only" grep -q "holds the gateway token: mode set to 600" <<<"$OUT"

check "agent cgpt created" test -f "$OC_AGENT"
check "agent file is the bundled template" cmp -s "$OC_AGENT" "$BUNDLE/opencode-agent-cgpt.md"
for line in "model: cgpt/chatgpt-web" "mode: primary" "  bash: ask" "  edit: ask" "  webfetch: ask" \
            "  task: deny" "  question: allow"; do
  check "agent file has '$line'" grep -qxF "$line" "$OC_AGENT"
done
check "the agent description is plain YAML (no ': ' in the value)" \
  grep -Eq '^description: [^:]+$' "$OC_AGENT"
check "agent file has LF line ends only" test -z "$(tr -cd '\r' < "$OC_AGENT")"
check "agent file has no BOM" test "$(head -c 3 "$OC_AGENT" | od -An -tx1 | tr -d ' ')" != "efbbbf"
eq "agent file has ordinary permissions" "$(stat -f '%Lp' "$OC_AGENT")" "644"
check "opencode.json gets no agent key" test -z "$(get_json "$OC" agent)"
if has "$OC" default_agent; then fail "opencode.json gets no default_agent"; else ok "opencode.json gets no default_agent"; fi
check "installer tells to pick the agent with Tab" grep -q "Tab to switch to the agent cgpt" <<<"$OUT"
check "installer says the agent was added" grep -q "agent cgpt: added" <<<"$OUT"
check "installer says the agent is not the default" grep -q "not made the default agent" <<<"$OUT"

BEFORE="$(snapshot)"
install >/dev/null 2>&1 || fail "second install.sh exits 0"
eq "second install changes nothing" "$(snapshot)" "$BEFORE"
eq "one cgpt provider" "$(grep -c '"cgpt"' "$OC")" "1"
check "opencode.json.bak is still the original" cmp -s "$OC.bak" "$WORK/opencode.original.json"

# An up-to-date opencode.json that became readable by others is tightened.
chmod 644 "$OC"
cp "$OC" "$WORK/opencode.installed.json"
OUT="$(install 2>&1)" || fail "install.sh over a 644 opencode.json exits 0"
eq "a re-run tightens opencode.json to 600" "$(stat -f '%Lp' "$OC")" "600"
check "a re-run leaves the content alone" cmp -s "$OC" "$WORK/opencode.installed.json"
check "a re-run says so" grep -q "mode set to 600" <<<"$OUT"

echo "a note of the user" > "$APP/notes.txt"
OUT="$(uninstall 2>&1)" || { echo "$OUT" >&2; fail "uninstall.sh exits 0"; }
for f in bin extension uninstall.sh gateway.log opencode-agent-cgpt.md; do
  check "uninstall removes $f" test ! -e "$APP/$f"
done
check "uninstall keeps a foreign file in the install dir" test -f "$APP/notes.txt"
check "uninstall removes the gateway config" test ! -e "$GW"
check "uninstall removes the host manifest" test ! -e "$NM"
check "uninstall keeps a foreign host manifest" test -f "$NM_DIR/com.other.host.json"
check "uninstall removes the LaunchAgent" test ! -e "$PLIST"
check "uninstall keeps a foreign LaunchAgent" test -f "$AGENTS/com.other.agent.plist"
check "uninstall removes provider cgpt" test -z "$(get_json "$OC" provider.cgpt)"
eq "uninstall removes small_model it set" "$(get "$OC" small_model)" ""
eq "uninstall keeps model" "$(get "$OC" model)" "anthropic/claude-sonnet-4-5"
eq "uninstall keeps the other provider" "$(get_json "$OC" provider.anthropic)" \
  "$(get_json "$WORK/opencode.original.json" provider.anthropic)"
eq "uninstall keeps UTF-8" "$(get "$OC" instructions.0)" "Отвечай по-русски 🙂"
check "uninstall removes agent cgpt" test ! -e "$OC_AGENT"
check "uninstall removes the agents folder it left empty" test ! -e "$OC_AGENTS"
rm -f "$APP/notes.txt"; rmdir "$APP"

# ---------------------------------------------------------------------------
echo "# B: no opencode.json yet; host manifest replaced by a dev install"
# ---------------------------------------------------------------------------
rm -rf "$OC_DIR"
install >/dev/null 2>&1 || fail "install.sh without opencode.json exits 0"
eq "opencode.json created with schema" "$(get "$OC" '$schema')" "https://opencode.ai/config.json"
eq "created config has cgpt" "$(get "$OC" provider.cgpt.npm)" "@ai-sdk/openai-compatible"
eq "created config has small_model" "$(get "$OC" small_model)" "cgpt/local-title"
check "no .bak for a new opencode.json" test ! -e "$OC.bak"

# A later dev install (install/macos/install-host.sh) points the host elsewhere.
sed -i '' "s|$APP/bin/cgpt-bridge-host|/elsewhere/cgpt-bridge-host|" "$NM"
uninstall >/dev/null 2>&1 || fail "uninstall.sh exits 0"
eq "uninstall keeps a host manifest it did not write" "$(get "$NM" path)" "/elsewhere/cgpt-bridge-host"
check "uninstall leaves no cgpt provider" test -z "$(get_json "$OC" provider)"
check "uninstall removes the empty install dir" test ! -e "$APP"

# ---------------------------------------------------------------------------
echo "# C: user already has small_model and picked cgpt as model"
# ---------------------------------------------------------------------------
cat > "$OC" <<'JSON'
{ "model": "cgpt/chatgpt-web", "small_model": "anthropic/claude-haiku-4-5", "default_agent": "plan" }
JSON
install >/dev/null 2>&1 || fail "install.sh exits 0"
eq "existing small_model kept" "$(get "$OC" small_model)" "anthropic/claude-haiku-4-5"
eq "model kept" "$(get "$OC" model)" "cgpt/chatgpt-web"
eq "default_agent kept" "$(get "$OC" default_agent)" "plan"
uninstall >/dev/null 2>&1 || fail "uninstall.sh exits 0"
eq "uninstall keeps a foreign small_model" "$(get "$OC" small_model)" "anthropic/claude-haiku-4-5"
eq "uninstall drops a model of the removed provider" "$(get "$OC" model)" ""
eq "uninstall keeps default_agent" "$(get "$OC" default_agent)" "plan"

# ---------------------------------------------------------------------------
echo "# D: opencode.json is a symlink (dotfiles)"
# ---------------------------------------------------------------------------
mkdir -p "$WORK/dotfiles"
printf '{ "theme": "light" }\n' > "$WORK/dotfiles/opencode.json"
chmod 640 "$WORK/dotfiles/opencode.json"
cp "$WORK/dotfiles/opencode.json" "$WORK/dotfiles.original.json"
rm -f "$OC" "$OC.bak"
ln -s ../../dotfiles-link/opencode.json "$OC"          # relative link ...
ln -s "$WORK/dotfiles" "$HOME/dotfiles-link"           # ... through a linked dir
install >/dev/null 2>&1 || fail "install.sh with a linked opencode.json exits 0"
check "opencode.json is still a symlink" test -L "$OC"
eq "the link target got provider cgpt" "$(get "$WORK/dotfiles/opencode.json" provider.cgpt.npm)" "@ai-sdk/openai-compatible"
eq "the link target is made owner-only" "$(stat -f '%Lp' "$WORK/dotfiles/opencode.json")" "600"
check "opencode.json.bak is the original target" cmp -s "$OC.bak" "$WORK/dotfiles.original.json"
uninstall >/dev/null 2>&1 || fail "uninstall.sh exits 0"
check "opencode.json is still a symlink after uninstall" test -L "$OC"
check "uninstall removes cgpt from the link target" test -z "$(get_json "$WORK/dotfiles/opencode.json" provider)"
eq "the link target keeps its mode on uninstall" "$(stat -f '%Lp' "$WORK/dotfiles/opencode.json")" "600"
rm -f "$OC" "$OC.bak" "$HOME/dotfiles-link"

ln -s cycle-b.json "$OC"
ln -s opencode.json "$OC_DIR/cycle-b.json"
RC=0; OUT="$(install 2>&1)" || RC=$?
check "install.sh refuses a symlink cycle" test "$RC" -ne 0
check "the error names the link problem" grep -q "symbolic links" <<<"$OUT"
RC=0; OUT="$(uninstall 2>&1)" || RC=$?
check "uninstall.sh reports a symlink cycle" test "$RC" -ne 0
check "uninstall.sh names the link problem" grep -q "symbolic links" <<<"$OUT"
rm -f "$OC" "$OC_DIR/cycle-b.json"

# ---------------------------------------------------------------------------
echo "# G: OpenCode agent cgpt next to the user's own agents"
# ---------------------------------------------------------------------------
agent_line() { grep -A1 '==> OpenCode agent cgpt' <<<"$1" | tail -n 1; }
rm -rf "$OC_DIR"
mkdir -p "$OC_AGENTS"
printf -- '---\ndescription: my own agent\nmode: primary\n---\nBe brief.\n' > "$OC_AGENTS/mine.md"
cp "$OC_AGENTS/mine.md" "$WORK/mine.original.md"
OUT="$(install 2>&1)" || fail "install.sh next to a foreign agent exits 0"
check "agent cgpt added next to a foreign agent" cmp -s "$OC_AGENT" "$BUNDLE/opencode-agent-cgpt.md"
check "install leaves a foreign agent alone" cmp -s "$OC_AGENTS/mine.md" "$WORK/mine.original.md"

HASH="$(shasum -a 256 < "$OC_AGENT")"
RC=0; OUT="$(install 2>&1)" || RC=$?
eq "a re-run exits 0" "$RC" "0"
eq "a re-run leaves agent cgpt as it was" "$(shasum -a 256 < "$OC_AGENT")" "$HASH"
check "a re-run says the agent is unchanged" grep -q "agent cgpt: unchanged" <<<"$(agent_line "$OUT")"
check "a re-run leaves no cgpt.md.bak" test ! -e "$OC_AGENT.bak"

# An edited (or older) agent file of ours is brought up to date; the edited
# copy is kept as cgpt.md.bak.
sed -i '' 's/webfetch: ask/webfetch: allow/' "$OC_AGENT"
cp "$OC_AGENT" "$WORK/agent.edited.md"
OUT="$(install 2>&1)" || fail "install.sh over an edited agent cgpt exits 0"
check "an edited agent cgpt of ours is replaced" cmp -s "$OC_AGENT" "$BUNDLE/opencode-agent-cgpt.md"
check "the edited copy is kept as cgpt.md.bak" cmp -s "$OC_AGENT.bak" "$WORK/agent.edited.md"
check "install says the agent was updated and where the copy is" \
  grep -q "agent cgpt: updated (previous saved as cgpt.md.bak)" <<<"$(agent_line "$OUT")"

# Edit, update, then a newer bundle with a changed template, twice: the edit
# stays in cgpt.md.bak. The unedited older template is replaced without a
# .bak, so it never overwrites the saved edit.
cp -R "$BUNDLE" "$WORK/bundle-v2"
sed -i '' 's/edits and web fetches$/edits and web fetches (v2)/' "$WORK/bundle-v2/opencode-agent-cgpt.md"
if cmp -s "$WORK/bundle-v2/opencode-agent-cgpt.md" "$BUNDLE/opencode-agent-cgpt.md"; then
  fail "the v2 template differs from v1"; else ok "the v2 template differs from v1"; fi
RC=0; OUT="$("$WORK/bundle-v2/install.sh" --no-launchctl 2>&1)" || RC=$?
eq "an update to a changed template exits 0" "$RC" "0"
check "the unedited older template is replaced by the new one" \
  cmp -s "$OC_AGENT" "$WORK/bundle-v2/opencode-agent-cgpt.md"
check "an unedited older template is reported as updated, without a .bak" \
  grep -qx "    agent cgpt: updated" <<<"$(agent_line "$OUT")"
check "the saved edit survives a template change" cmp -s "$OC_AGENT.bak" "$WORK/agent.edited.md"
OUT="$("$WORK/bundle-v2/install.sh" --no-launchctl 2>&1)" || fail "a second v2 install exits 0"
check "a second v2 install says unchanged" grep -q "agent cgpt: unchanged" <<<"$(agent_line "$OUT")"
check "the saved edit survives a second v2 install" cmp -s "$OC_AGENT.bak" "$WORK/agent.edited.md"
OUT="$(install 2>&1)" || fail "going back to v1 exits 0"
check "going back to v1 replaces the unedited v2 file" cmp -s "$OC_AGENT" "$BUNDLE/opencode-agent-cgpt.md"
check "the saved edit survives going back" cmp -s "$OC_AGENT.bak" "$WORK/agent.edited.md"
rm -rf "$WORK/bundle-v2"
rm -f "$OC_AGENT.bak"

# --no-agent creates nothing and leaves an agent of an earlier run alone.
sed -i '' 's/webfetch: ask/webfetch: allow/' "$OC_AGENT"
cp "$OC_AGENT" "$WORK/agent.older.md"
OUT="$("$BUNDLE/install.sh" --no-launchctl --no-agent 2>&1)" || fail "install.sh --no-agent exits 0"
check "--no-agent leaves an agent cgpt of an earlier run in place" cmp -s "$OC_AGENT" "$WORK/agent.older.md"
OUT="$(uninstall 2>&1)" || fail "uninstall.sh exits 0"
check "uninstall removes agent cgpt" test ! -e "$OC_AGENT"
check "uninstall keeps an edited agent cgpt as cgpt.md.bak" cmp -s "$OC_AGENT.bak" "$WORK/agent.older.md"
check "uninstall keeps a foreign agent" cmp -s "$OC_AGENTS/mine.md" "$WORK/mine.original.md"
check "uninstall keeps an agents folder that is not empty" test -d "$OC_AGENTS"
check "uninstall says why it kept the copy" grep -q "differs from the installed one" <<<"$OUT"
rm -f "$OC_AGENT.bak"
OUT="$("$BUNDLE/install.sh" --no-launchctl --no-agent 2>&1)" || fail "install.sh --no-agent exits 0"
check "--no-agent does not create agent cgpt" test ! -e "$OC_AGENT"
check "--no-agent says so" grep -q -- "--no-agent" <<<"$(agent_line "$OUT")"
check "without the agent the installer does not point at Tab" test -z "$(grep 'Tab to switch' <<<"$OUT")"
uninstall >/dev/null 2>&1 || fail "uninstall.sh exits 0"

# Uninstall cannot write the .bak of an edited copy (read-only agents
# folder): it warns, keeps the file and finishes the rest.
install >/dev/null 2>&1 || fail "install.sh exits 0"
sed -i '' 's/webfetch: ask/webfetch: allow/' "$OC_AGENT"
cp "$OC_AGENT" "$WORK/agent.ro.md"
chmod 555 "$OC_AGENTS"
RC=0; OUT="$(uninstall 2>&1)" || RC=$?
chmod 755 "$OC_AGENTS"
check "uninstall reports the failed .bak with a non-zero status" test "$RC" -ne 0
check "uninstall warns that the agent was left" grep -q "agent cgpt left in place" <<<"$OUT"
check "uninstall keeps the edited agent it could not back up" cmp -s "$OC_AGENT" "$WORK/agent.ro.md"
check "uninstall goes on after the failed .bak" test ! -e "$APP/bin"
rm -f "$OC_AGENT"
# Without the installed template copy (an install from before it existed)
# the file is kept as .bak to be safe, and the message says so.
install >/dev/null 2>&1 || fail "install.sh exits 0"
rm -f "$APP/opencode-agent-cgpt.md"
OUT="$(uninstall 2>&1)" || fail "uninstall.sh exits 0"
check "without the installed copy uninstall keeps a .bak" test -f "$OC_AGENT.bak"
check "without the installed copy the message does not claim a difference" \
  test -z "$(grep 'differs from the installed one' <<<"$OUT")"
check "without the installed copy the message says why" grep -q "to be safe" <<<"$OUT"
rm -f "$OC_AGENT" "$OC_AGENT.bak"

# An agent cgpt the user wrote is not ours: neither install nor uninstall
# touches it. Both marks are needed, the model alone is not enough.
for foreign in 'description: my own cgpt\nmodel: openai/gpt-5' \
               'description: my own cgpt\nmodel: cgpt/chatgpt-web'; do
  printf -- "---\n$foreign\n---\nBe brief.\n" > "$OC_AGENT"
  cp "$OC_AGENT" "$WORK/foreign-cgpt.md"
  echo '{ "default_agent": "cgpt" }' > "$OC"
  RC=0; OUT="$(install 2>&1)" || RC=$?
  eq "install.sh next to a foreign agent cgpt exits 0" "$RC" "0"
  check "install does not overwrite a foreign agent cgpt" cmp -s "$OC_AGENT" "$WORK/foreign-cgpt.md"
  check "install says the agent was not added" grep -q "not added" <<<"$(agent_line "$OUT")"
  check "without the agent the installer does not point at Tab" test -z "$(grep 'Tab to switch' <<<"$OUT")"
  uninstall >/dev/null 2>&1 || fail "uninstall.sh exits 0"
  check "uninstall keeps a foreign agent cgpt" cmp -s "$OC_AGENT" "$WORK/foreign-cgpt.md"
  eq "uninstall keeps default_agent cgpt while a foreign agent cgpt stays" "$(get "$OC" default_agent)" "cgpt"
done
rm -f "$OC_AGENT"

# default_agent cgpt (the user made our agent the default) would point at
# nothing once the agent is gone: uninstall drops it, like a cgpt/ model.
echo '{ "default_agent": "cgpt", "theme": "dark" }' > "$OC"
install >/dev/null 2>&1 || fail "install.sh with default_agent cgpt exits 0"
eq "install leaves default_agent alone" "$(get "$OC" default_agent)" "cgpt"
uninstall >/dev/null 2>&1 || fail "uninstall.sh exits 0"
check "uninstall removes agent cgpt" test ! -e "$OC_AGENT"
if has "$OC" default_agent; then fail "uninstall drops default_agent cgpt"; else ok "uninstall drops default_agent cgpt"; fi
eq "uninstall keeps the rest of opencode.json" "$(get "$OC" theme)" "dark"
# ... but not one that names an agent defined in opencode.json itself.
echo '{ "default_agent": "cgpt", "agent": { "cgpt": { "model": "openai/gpt-5" } } }' > "$OC"
install >/dev/null 2>&1 || fail "install.sh exits 0"
uninstall >/dev/null 2>&1 || fail "uninstall.sh exits 0"
eq "uninstall keeps default_agent cgpt of an agent in opencode.json" "$(get "$OC" default_agent)" "cgpt"
rm -rf "$OC_DIR"

# agents/cgpt.md is a folder: not an agent of ours, nothing is said about Tab.
mkdir -p "$OC_AGENT"
RC=0; OUT="$(install 2>&1)" || RC=$?
eq "install.sh with a folder named cgpt.md exits 0" "$RC" "0"
check "install says the agent was not added (folder)" grep -q "not added" <<<"$(agent_line "$OUT")"
check "the folder named cgpt.md is left empty" test -z "$(ls -A "$OC_AGENT")"
check "with a folder in the way the installer does not point at Tab" test -z "$(grep 'Tab to switch' <<<"$OUT")"
uninstall >/dev/null 2>&1 || fail "uninstall.sh exits 0"
check "uninstall leaves the folder named cgpt.md" test -d "$OC_AGENT"
rm -rf "$OC_DIR"

# The optional agent step cannot fail the install: agents is a file here.
mkdir -p "$OC_DIR"
echo 'not a folder' > "$OC_AGENTS"
RC=0; OUT="$(install 2>&1)" || RC=$?
eq "install.sh exits 0 when the agent cannot be written" "$RC" "0"
check "it warns that the agent was not added" grep -q "agent cgpt not added" <<<"$OUT"
check "the install goes on after it (LaunchAgent written)" test -f "$PLIST"
check "a failed agent step leaves no temp file" test -z "$(find "$OC_DIR" -name '*.tmp.*')"
check "after a failed agent step the installer does not point at Tab" test -z "$(grep 'Tab to switch' <<<"$OUT")"
uninstall >/dev/null 2>&1 || fail "uninstall.sh exits 0"
rm -rf "$OC_DIR"

# The agents folder is a symlink (dotfiles): the file goes into its target,
# uninstall removes the file and keeps the link.
mkdir -p "$OC_DIR" "$WORK/dot-agents"
ln -s "$WORK/dot-agents" "$OC_AGENTS"
install >/dev/null 2>&1 || fail "install.sh with a linked agents folder exits 0"
check "agent cgpt is written into the linked agents folder" \
  cmp -s "$WORK/dot-agents/cgpt.md" "$BUNDLE/opencode-agent-cgpt.md"
uninstall >/dev/null 2>&1 || fail "uninstall.sh exits 0"
check "uninstall removes agent cgpt from the linked folder" test ! -e "$WORK/dot-agents/cgpt.md"
check "uninstall keeps the agents link" test -L "$OC_AGENTS"
rm -rf "$OC_DIR"

# The whole opencode folder is a symlink (dotfiles).
mkdir -p "$WORK/dot-opencode" "$HOME/.config"
ln -s "$WORK/dot-opencode" "$OC_DIR"
install >/dev/null 2>&1 || fail "install.sh with a linked opencode folder exits 0"
check "the opencode folder is still a symlink" test -L "$OC_DIR"
check "agent cgpt is written into the link target" \
  cmp -s "$WORK/dot-opencode/agents/cgpt.md" "$BUNDLE/opencode-agent-cgpt.md"
uninstall >/dev/null 2>&1 || fail "uninstall.sh exits 0"
check "the opencode folder is still a symlink after uninstall" test -L "$OC_DIR"
check "uninstall removes agent cgpt and its folder from the link target" test ! -e "$WORK/dot-opencode/agents"
rm -f "$OC_DIR"

# A refused opencode.json (comments) means nothing in OpenCode was changed.
mkdir -p "$OC_DIR"
printf '// comments are not plain JSON\n{}\n' > "$OC"
RC=0; install >/dev/null 2>&1 || RC=$?
check "install.sh refuses a JSONC opencode.json" test "$RC" -ne 0
check "a refused install adds no agent" test ! -e "$OC_AGENTS"
echo '{}' > "$OC"
uninstall >/dev/null 2>&1 || fail "uninstall.sh exits 0"
rm -rf "$OC_DIR"
mkdir -p "$OC_DIR"

# ---------------------------------------------------------------------------
echo "# F: files that hold the token are owner-only from the moment they exist"
# ---------------------------------------------------------------------------
# With chmod a no-op every file keeps the mode it was created with, so a file
# created readable by others and tightened only afterwards shows up here.
mkdir -p "$WORK/nochmod"
printf '#!/bin/sh\nexit 0\n' > "$WORK/nochmod/chmod"
/bin/chmod +x "$WORK/nochmod/chmod"
printf '{ "theme": "dark" }\n' > "$OC"
/bin/chmod 644 "$OC"
printf 'stale\n' > "$OC.bak"
/bin/chmod 644 "$OC.bak"
(umask 022; PATH="$WORK/nochmod:$PATH" "$BUNDLE/install.sh" --no-launchctl) >/dev/null 2>&1 \
  || fail "install.sh with chmod disabled exits 0"
eq "opencode.json is written owner-only" "$(stat -f '%Lp' "$OC")" "600"
eq "opencode.json.bak is written owner-only" "$(stat -f '%Lp' "$OC.bak")" "600"
uninstall >/dev/null 2>&1 || fail "uninstall.sh exits 0"
rm -f "$OC" "$OC.bak"

# ---------------------------------------------------------------------------
echo "# E: start, health check and update order (stub launchctl)"
# ---------------------------------------------------------------------------
mkdir -p "$WORK/fakebin"
cat > "$WORK/fakebin/launchctl" <<'STUB'
#!/bin/bash
# Stands in for launchd: bootstrap runs the plist's program in the
# background, bootout stops it, print succeeds while it is loaded. Every call
# but print is logged. $WORK/bootstrap.fails = how many bootstraps fail first.
[[ "$1" == print ]] || echo "$1" >> "$WORK/launchd.log"
case "$1" in
  print)
    [[ -f "$WORK/launchd.pid" ]] && exit 0
    exit 113 ;;
  bootout)
    if [[ -f "$WORK/launchd.pid" ]]; then
      kill "$(cat "$WORK/launchd.pid")" 2>/dev/null
      rm -f "$WORK/launchd.pid"
      exit 0
    fi
    exit 3 ;;                                   # like launchd: not loaded
  bootstrap)
    fails="$(cat "$WORK/bootstrap.fails" 2>/dev/null || echo 0)"
    if (( fails > 0 )); then
      echo $((fails - 1)) > "$WORK/bootstrap.fails"
      echo "Bootstrap failed: 5: Input/output error" >&2
      exit 5
    fi
    prog="$(plutil -extract ProgramArguments.0 raw -o - "$3")"
    log="$(plutil -extract StandardErrorPath raw -o - "$3")"
    "$prog" >> "$log" 2>&1 &
    echo $! > "$WORK/launchd.pid" ;;
esac
STUB
chmod +x "$WORK/fakebin/launchctl"
export WORK
launchd_install() { PATH="$WORK/fakebin:$PATH" "$BUNDLE/install.sh"; }
launchd_uninstall() { PATH="$WORK/fakebin:$PATH" "$APP/uninstall.sh"; }
set_port() { # set_port <port>: the gateway config keeps its token
  sed -i '' "s/\"port\": *[0-9]*/\"port\": $1/" "$GW/config.json"
}

echo '{}' > "$OC"
install >/dev/null 2>&1 || fail "install.sh exits 0"
set_port 18765
RC=0; OUT="$(launchd_install 2>&1)" || RC=$?
eq "install.sh with launchd exits 0" "$RC" "0"
check "health check reports the gateway on the new port" grep -q "gateway answers on http://127.0.0.1:18765" <<<"$OUT"
eq "launchctl: stop, then start" "$(tr '\n' ' ' < "$WORK/launchd.log")" "bootout bootstrap "
eq "opencode.json follows the new port" "$(get "$OC" provider.cgpt.options.baseURL)" "http://127.0.0.1:18765/v1"
TOKEN="$(get "$GW/config.json" token)"
eq "the started gateway serves /v1/models" \
  "$(printf 'Authorization: Bearer %s\n' "$TOKEN" | curl -s -o /dev/null -w '%{http_code}' -m 2 --noproxy '*' -H @- http://127.0.0.1:18765/v1/models)" "200"
check "token is not on the osascript command line" \
  test -z "$(grep -n 'osascript.*TOKEN' "$BUNDLE/install.sh" "$APP/uninstall.sh")"

GW_PID="$(cat "$WORK/launchd.pid")"
: > "$WORK/launchd.log"
printf '// comments are not plain JSON\n{}\n' > "$OC"
RC=0; OUT="$(launchd_install 2>&1)" || RC=$?
check "install.sh refuses a JSONC opencode.json" test "$RC" -ne 0
eq "a refused install does not touch launchd" "$(cat "$WORK/launchd.log")" ""
check "a refused install leaves the gateway running" kill -0 "$GW_PID"
check "a refused install leaves no staged binary" test -z "$(find "$APP/bin" -name '.*')"
echo '{}' > "$OC"

set_port 18766
nc -lk 127.0.0.1 18766 >/dev/null 2>&1 &
NC_PID=$!
sleep 0.3
RC=0; OUT="$(launchd_install 2>&1)" || RC=$?
check "install.sh fails when the port is taken" test "$RC" -ne 0
check "the error names the port" grep -q "port 18766" <<<"$OUT"
check "a busy port is not reported as started" test -z "$(grep 'gateway answers' <<<"$OUT")"
kill "$NC_PID" 2>/dev/null; NC_PID=""

set_port 18765
: > "$WORK/launchd.log"
echo 2 > "$WORK/bootstrap.fails"
RC=0; OUT="$(launchd_install 2>&1)" || RC=$?
eq "install.sh retries a bootstrap that fails while the old job goes away" "$RC" "0"
eq "launchctl: stop, then three starts" "$(tr '\n' ' ' < "$WORK/launchd.log")" "bootout bootstrap bootstrap bootstrap "
check "the retried gateway answers" grep -q "gateway answers on http://127.0.0.1:18765" <<<"$OUT"

echo 99 > "$WORK/bootstrap.fails"
RC=0; OUT="$(launchd_install 2>&1)" || RC=$?
check "install.sh fails when launchd never starts the gateway" test "$RC" -ne 0
check "the error says the gateway is not running" grep -q "the gateway is not running" <<<"$OUT"
rm -f "$WORK/bootstrap.fails"

launchd_uninstall >/dev/null 2>&1 || fail "uninstall.sh with launchd exits 0"
check "uninstall stops the gateway" test ! -e "$WORK/launchd.pid"

echo
if [[ $FAILS -gt 0 ]]; then
  echo "$FAILS check(s) failed" >&2
  exit 1
fi
echo "all macOS install checks passed"
