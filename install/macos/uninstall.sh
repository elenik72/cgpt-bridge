#!/usr/bin/env bash
#
# cgpt-bridge — macOS uninstaller. install.sh copies it to
# ~/Library/Application Support/cgpt-bridge/uninstall.sh.
#
# Removes what install.sh created and nothing else:
#   - the gateway LaunchAgent (stopped first)
#   - the Chrome native host manifest, if it still points at the installed host
#   - bin/, extension/, uninstall.sh, gateway.log and the agent template in the
#     install folder
#     (the folder itself only when nothing else is left in it)
#   - the gateway config, sessions and rate-limit state
#   - provider "cgpt" from the OpenCode config, small_model if it is
#     cgpt/local-title, model if it names a cgpt/ model and default_agent if
#     it is cgpt and no agent cgpt is left (the previous file is kept as .bak)
#   - the OpenCode agent cgpt (~/.config/opencode/agents/cgpt.md) if it is the
#     one install.sh wrote (a copy the user edited is kept as cgpt.md.bak),
#     and the agents folder if that leaves it empty
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
AGENT="${XDG_CONFIG_HOME:-$HOME/.config}/opencode/agents/cgpt.md"
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

echo "==> OpenCode agent cgpt: $AGENT"
# Ours by the marks install.sh checks too; an agent the user wrote stays. A
# copy of ours the user edited is kept as cgpt.md.bak.
if AGENT_FILE="$(resolve_link "$AGENT" 2>/dev/null)" && [[ -f "$AGENT_FILE" ]]; then
  if grep -q '^model: cgpt/chatgpt-web[[:space:]]*$' "$AGENT_FILE" &&
     grep -q '^description: ChatGPT Web (bridge)' "$AGENT_FILE"; then
    KEEP=""
    if [[ ! -f "$APP/opencode-agent-cgpt.md" ]]; then
      KEEP="no installed template to compare with: kept as $AGENT.bak to be safe"
    elif ! cmp -s "$APP/opencode-agent-cgpt.md" "$AGENT_FILE"; then
      KEEP="the file differs from the installed one: kept as $AGENT.bak"
    fi
    if [[ -n "$KEEP" ]] && ! { rm -f "$AGENT.bak" && cp "$AGENT_FILE" "$AGENT.bak"; }; then
      echo "warning: could not save $AGENT as $AGENT.bak: agent cgpt left in place; remove it by hand" >&2
      STATUS=1
    else
      [[ -n "$KEEP" ]] && echo "    $KEEP"
      rm -f "$AGENT_FILE"
      if [[ -L "$AGENT" ]]; then
        echo "    removed $AGENT_FILE; the link $AGENT to it is left in place"
      fi
      rmdir "$(dirname "$AGENT")" 2>/dev/null || true
    fi
  else
    echo "    $AGENT is not ours; left in place"
  fi
fi
# default_agent "cgpt" is dropped from opencode.json below once no agent file
# cgpt is left (it would name a missing agent), like a model of provider cgpt.
AGENT_GONE=0
[[ -e "$AGENT" ]] || AGENT_GONE=1

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
rm -f "$APP/uninstall.sh" "$APP/gateway.log" "$APP/opencode-agent-cgpt.md"
rmdir "$APP" 2>/dev/null || true

echo "==> Gateway config: $GW"
rm -rf "$GW/sessions"
rm -f "$GW/config.json" "$GW/cooldown.json"
rmdir "$GW" 2>/dev/null || true

for dir in "$APP" "$GW"; do
  if [[ -e "$dir" ]]; then
    echo "    $dir has files that are not ours; left in place"
  fi
done

echo
echo "Done. Remove the cgpt-bridge extension at chrome://extensions yourself."
exit "$STATUS"
