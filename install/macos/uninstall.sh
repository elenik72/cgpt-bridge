#!/usr/bin/env bash
#
# cgpt-bridge — macOS uninstaller. install.sh copies it to
# ~/Library/Application Support/cgpt-bridge/uninstall.sh.
#
# Removes what install.sh created and nothing else:
#   - the gateway LaunchAgent (stopped first)
#   - the Chrome native host manifest, if it still points at the installed host
#   - bin/, extension/, uninstall.sh, gateway.log and the agent templates in
#     the install folder
#     (the folder itself only when nothing else is left in it)
#   - the gateway config, sessions, rate-limit state and records of unread answers
#   - provider "cgpt" from the OpenCode config, small_model if it is
#     cgpt/local-title, model if it names a cgpt/ model and default_agent if
#     it is cgpt and no agent cgpt is left (the previous file is kept as .bak)
#   - the OpenCode agents cgpt and cgpt-sub (~/.config/opencode/agents/cgpt.md
#     and cgpt-sub.md), each if it is the one install.sh wrote (a copy the
#     user edited is kept as <name>.md.bak), and the agents folder if that
#     leaves it empty
#
# Usage:
#   uninstall.sh [--no-launchctl]
#
#   --no-launchctl  do not stop the LaunchAgent (CI and tests)

set -euo pipefail

NO_LAUNCHCTL=false
for arg in "$@"; do
  case "$arg" in
    --no-launchctl) NO_LAUNCHCTL=true ;;
    *) echo "usage: $0 [--no-launchctl]" >&2; exit 2 ;;
  esac
done

HOST_NAME="com.cgpt_bridge.host"
LABEL="com.cgpt-bridge.gateway"
APP="$HOME/Library/Application Support/cgpt-bridge"
GW="$HOME/Library/Application Support/cgpt-gateway"
NM="$HOME/Library/Application Support/Google/Chrome/NativeMessagingHosts/$HOST_NAME.json"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
OC="${XDG_CONFIG_HOME:-$HOME/.config}/opencode/opencode.json"
AGENTS="${XDG_CONFIG_HOME:-$HOME/.config}/opencode/agents"
STATUS=0

# Follow symlinks (dotfiles setups link opencode.json): the edit goes to the
# real file and the link stays a link.
resolve_link() {
  local path="$1" target hops=0
  while [[ -L "$path" ]]; do
    if (( ++hops > 40 )); then
      echo "error: $1: more than 40 symbolic links in a row (a cycle?)" >&2
      return 1
    fi
    target="$(readlink "$path")"
    if [[ "$target" == /* ]]; then path="$target"; else path="$(dirname "$path")/$target"; fi
  done
  printf '%s' "$path"
}

echo "==> LaunchAgent"
if [[ "$NO_LAUNCHCTL" == false ]]; then
  launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
fi
rm -f "$PLIST"

echo "==> Chrome native host"
if [[ -f "$NM" ]]; then
  if [[ "$(plutil -extract path raw -o - "$NM" 2>/dev/null || true)" == "$APP/bin/cgpt-bridge-host" ]]; then
    rm -f "$NM"
  else
    echo "    $NM points at another host binary; left in place"
  fi
fi

# remove_agent <name>: agents/<name>.md, if ours by the marks install.sh
# checks too; an agent the user wrote stays. A copy of ours the user edited is
# kept as <name>.md.bak.
remove_agent() {
  local name="$1"
  local agent="$AGENTS/$name.md"
  local installed="$APP/opencode-agent-$name.md"
  local file keep
  echo "==> OpenCode agent $name: $agent"
  if ! file="$(resolve_link "$agent" 2>/dev/null)" || [[ ! -f "$file" ]]; then
    return 0
  fi
  if ! { grep -q '^model: cgpt/chatgpt-web[[:space:]]*$' "$file" &&
         grep -q '^description: ChatGPT Web (bridge)' "$file"; }; then
    echo "    $agent is not ours; left in place"
    return 0
  fi
  keep=""
  if [[ ! -f "$installed" ]]; then
    keep="no installed template to compare with: kept as $agent.bak to be safe"
  elif ! cmp -s "$installed" "$file"; then
    keep="the file differs from the installed one: kept as $agent.bak"
  fi
  if [[ -n "$keep" ]] && ! { rm -f "$agent.bak" && cp "$file" "$agent.bak"; }; then
    echo "warning: could not save $agent as $agent.bak: agent $name left in place; remove it by hand" >&2
    STATUS=1
    return 0
  fi
  [[ -n "$keep" ]] && echo "    $keep"
  if ! rm -f "$file"; then
    echo "warning: could not remove $agent: agent $name left in place; remove it by hand" >&2
    STATUS=1
    return 0
  fi
  AGENT_REMOVED=true
  if [[ -L "$agent" ]]; then
    echo "    removed $file; the link $agent to it is left in place"
  fi
}
AGENT_REMOVED=false
remove_agent cgpt
remove_agent cgpt-sub
# The agents folder, when removing ours left it empty.
if [[ "$AGENT_REMOVED" == true ]]; then rmdir "$AGENTS" 2>/dev/null || true; fi
# default_agent "cgpt" is dropped from opencode.json below once no agent file
# cgpt is left (it would name a missing agent), like a model of provider cgpt.
AGENT_GONE=0
[[ -e "$AGENTS/cgpt.md" ]] || AGENT_GONE=1

echo "==> OpenCode provider cgpt: $OC"
if ! OC_FILE="$(resolve_link "$OC")"; then
  echo "warning: $OC left unchanged; remove provider \"cgpt\" from it by hand" >&2
  STATUS=1
  OC_FILE=""
fi
if [[ -n "$OC_FILE" && -f "$OC_FILE" ]]; then
  OC_NEW="$(mktemp "${TMPDIR:-/tmp}/cgpt-bridge-opencode.XXXXXX")"
  trap 'rm -f "$OC_NEW"' EXIT
  if osascript -l JavaScript - "$OC_FILE" "$AGENT_GONE" > "$OC_NEW" <<'JS'
ObjC.import('Foundation');
function run(argv) {
  var path = argv[0], agentGone = argv[1] === '1';
  var text = $.NSString.stringWithContentsOfFileEncodingError(path, $.NSUTF8StringEncoding, null);
  if (text.isNil()) throw new Error('cannot read ' + path + ' as UTF-8');
  text = text.js;
  if (text.trim() === '') return '';
  var cfg;
  try { cfg = JSON.parse(text); }
  catch (e) { throw new Error(path + ' is not plain JSON (' + e.message + ')'); }
  var isObject = function (v) { return v !== null && typeof v === 'object' && !Array.isArray(v); };
  if (!isObject(cfg)) return '';
  var before = JSON.stringify(cfg);
  if (isObject(cfg.provider)) {
    delete cfg.provider.cgpt;
    if (Object.keys(cfg.provider).length === 0) delete cfg.provider;
  }
  if (cfg.small_model === 'cgpt/local-title') delete cfg.small_model;
  if (typeof cfg.model === 'string' && cfg.model.indexOf('cgpt/') === 0) delete cfg.model;
  // An agent cgpt defined in opencode.json itself is the user's own.
  if (cfg.default_agent === 'cgpt' && agentGone && !(isObject(cfg.agent) && cfg.agent.cgpt !== undefined)) {
    delete cfg.default_agent;
  }
  if (JSON.stringify(cfg) === before) return '';
  return JSON.stringify(cfg, null, 2);
}
JS
  then
    NEW_OC="$(cat "$OC_NEW")"
    if [[ -n "$NEW_OC" ]]; then
      cp -p "$OC_FILE" "$OC.bak"
      TMP="$OC_FILE.tmp.$$"
      printf '%s\n' "$NEW_OC" > "$TMP"
      chmod "$(stat -f '%Lp' "$OC_FILE")" "$TMP"
      mv -f "$TMP" "$OC_FILE"
      echo "    removed; previous file kept as $OC.bak"
    fi
  else
    echo "warning: could not update $OC; remove provider \"cgpt\" from it by hand" >&2
    STATUS=1
  fi
fi

echo "==> Files: $APP"
rm -rf "$APP/bin" "$APP/extension"
rm -f "$APP/uninstall.sh" "$APP/gateway.log" "$APP/opencode-agent-cgpt.md" \
      "$APP/opencode-agent-cgpt-sub.md"
rmdir "$APP" 2>/dev/null || true

echo "==> Gateway config: $GW"
rm -rf "$GW/sessions" "$GW/captures"
rm -f "$GW/config.json" "$GW/cooldown.json" "$GW/unparsed.jsonl" "$GW/unparsed.jsonl.1"
rmdir "$GW" 2>/dev/null || true

for dir in "$APP" "$GW"; do
  if [[ -e "$dir" ]]; then
    echo "    $dir has files that are not ours; left in place"
  fi
done

echo
echo "Done. Remove the cgpt-bridge extension at chrome://extensions yourself."
exit "$STATUS"
