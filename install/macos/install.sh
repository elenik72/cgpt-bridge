#!/usr/bin/env bash
#
# cgpt-bridge — macOS installer. Run it from the unpacked release bundle
# (cgpt-bridge-<version>-<build>-macos-<arch>.tar.gz). It
#
#   1. copies bin/, extension/ and uninstall.sh to
#      ~/Library/Application Support/cgpt-bridge/
#   2. registers the native host with Chrome
#      (~/Library/Application Support/Google/Chrome/NativeMessagingHosts/)
#   3. creates the gateway config with a fresh token
#      (~/Library/Application Support/cgpt-gateway/config.json)
#   4. adds provider "cgpt" to the global OpenCode config
#      (~/.config/opencode/opencode.json; the previous file is kept as .bak)
#      and makes it owner-only (600): it holds the gateway token
#   5. adds the OpenCode agent "cgpt" (~/.config/opencode/agents/cgpt.md): it
#      uses cgpt/chatgpt-web and asks before every command, file edit, web
#      access, MCP tool and skill, and of all subagents may start only
#      "cgpt-sub"
#      (agents/cgpt-sub.md, added too), which asks the same way. cgpt is not
#      made the default agent; an agent file the user wrote is left alone, an
#      edited copy of ours is kept as <name>.md.bak. A failure here only skips
#      that agent
#   6. installs the LaunchAgent ~/Library/LaunchAgents/com.cgpt-bridge.gateway.plist,
#      (re)starts the gateway and checks that it answers
#
# Everything is prepared first; the running gateway is stopped, replaced and
# started only at the very end, so a failed run leaves it alone. Running it
# again updates the files and changes nothing else. No sudo.
#
# Usage:
#   ./install.sh [--no-launchctl] [--no-agent]
#
#   --no-launchctl  write the LaunchAgent file but do not stop or start it
#                   (CI and tests, where there is no login session)
#   --no-agent      do not add the OpenCode agents cgpt and cgpt-sub (ones
#                   added by an earlier run stay as they are)
#
# Exit status 1 when the gateway does not answer after the start.

set -euo pipefail

NO_LAUNCHCTL=false
NO_AGENT=false
for arg in "$@"; do
  case "$arg" in
    --no-launchctl) NO_LAUNCHCTL=true ;;
    --no-agent) NO_AGENT=true ;;
    *) echo "usage: $0 [--no-launchctl] [--no-agent]" >&2; exit 2 ;;
  esac
done

# Chrome derives the extension id from the `key` in extension/manifest.json,
# so it is the same on every machine.
EXT_ID="oplkebjcjmifidmnbehpadfakodjjoge"
HOST_NAME="com.cgpt_bridge.host"
LABEL="com.cgpt-bridge.gateway"

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP="$HOME/Library/Application Support/cgpt-bridge"
NM_DIR="$HOME/Library/Application Support/Google/Chrome/NativeMessagingHosts"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
OC="${XDG_CONFIG_HOME:-$HOME/.config}/opencode/opencode.json"
AGENTS="${XDG_CONFIG_HOME:-$HOME/.config}/opencode/agents"

for f in bin/cgpt bin/cgpt-bridge-host bin/cgpt-gateway extension/manifest.json \
         uninstall.sh "$LABEL.plist" opencode-agent-cgpt.md opencode-agent-cgpt-sub.md; do
  if [[ ! -e "$SRC/$f" ]]; then
    echo "error: $SRC/$f not found — run install.sh from the unpacked release bundle" >&2
    exit 1
  fi
done

json_escape() {
  local s="${1//\\/\\\\}"
  printf '%s' "${s//\"/\\\"}"
}

# Write <text> to <path> through a temp file and a rename, so an interrupted
# run never leaves half a file. The temp file is created owner-only (umask
# 077) and gets <mode> after: the token in opencode.json is never readable by
# others, not even for a moment.
write_file() {
  local path="$1" text="$2" mode="$3"
  local tmp="$path.tmp.$$"
  (umask 077 && printf '%s\n' "$text" > "$tmp")
  chmod "$mode" "$tmp"
  mv -f "$tmp" "$path"
}

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

# Is <file> the OpenCode agent this installer writes? Both marks, so an agent
# the user named cgpt for the same model is still theirs.
is_our_agent() {
  grep -q '^model: cgpt/chatgpt-web[[:space:]]*$' "$1" &&
    grep -q '^description: ChatGPT Web (bridge)' "$1"
}

# Put <template> at <file> (the agent path <agent> with symlinks followed)
# through a temp file and a rename; with <backup> = true the file there is
# kept as <agent>.bak first. Returns 1 on any failure (errexit is off inside
# an `if`), with the temp file removed.
write_agent() {
  local template="$1" file="$2" agent="$3" backup="$4"
  AGENT_TMP="$file.tmp.$$"
  if mkdir -p "$(dirname "$file")" &&
     cp "$template" "$AGENT_TMP" &&
     chmod 644 "$AGENT_TMP" &&
     { [[ "$backup" != true ]] || { rm -f "$agent.bak" && cp "$file" "$agent.bak"; }; } &&
     mv -f "$AGENT_TMP" "$file"; then
    AGENT_TMP=""
    return 0
  fi
  rm -f "$AGENT_TMP" 2>/dev/null
  AGENT_TMP=""
  return 1
}

# HTTP status of GET /v1/models with the token ("000" when nothing answers).
# The token goes to curl on stdin, not on its command line.
gateway_status() {
  printf 'Authorization: Bearer %s\n' "$TOKEN" |
    curl -s -o /dev/null -w '%{http_code}' -m 1 --noproxy '*' -H @- \
      "http://127.0.0.1:$PORT/v1/models" || true
}

# Load the LaunchAgent. Right after a bootout launchd may still be tearing
# the old job down and refuse with "Bootstrap failed: 5: Input/output error":
# wait (bounded) until the old job is gone, then retry a few times.
start_gateway() {
  local domain="gui/$(id -u)" n err=""
  for n in 1 2 3 4 5 6 7 8 9 10; do
    launchctl print "$domain/$LABEL" >/dev/null 2>&1 || break
    sleep 0.5
  done
  for n in 1 2 3 4 5; do
    if err="$(launchctl bootstrap "$domain" "$PLIST" 2>&1)"; then return 0; fi
    sleep 0.5
  done
  echo "$err" >&2
  return 1
}

# Staged files; whatever is still here on exit belongs to a failed run.
GW_NEW="$APP/bin/.cgpt-gateway.new"
TMP_PLIST="$PLIST.tmp.$$"
OC_NEW=""
AGENT_TMP=""
cleanup() { rm -f "$GW_NEW" "$TMP_PLIST" ${OC_NEW:+"$OC_NEW"} ${AGENT_TMP:+"$AGENT_TMP"}; }
trap cleanup EXIT

# ---------------------------------------------------------------------------
# 1. Files. Binaries are copied next to their target and renamed over it, so
#    a host Chrome still runs keeps its old file and nothing is rewritten in
#    place. The gateway binary is only staged here; it replaces the running
#    one in step 7.
# ---------------------------------------------------------------------------
echo "==> Files: $APP"
mkdir -p "$APP/bin"
for f in cgpt cgpt-bridge-host; do
  cp "$SRC/bin/$f" "$APP/bin/.$f.new"
  chmod 755 "$APP/bin/.$f.new"
  mv -f "$APP/bin/.$f.new" "$APP/bin/$f"
done
cp "$SRC/bin/cgpt-gateway" "$GW_NEW"
chmod 755 "$GW_NEW"
rm -rf "$APP/extension"
cp -R "$SRC/extension" "$APP/extension"
cp "$SRC/uninstall.sh" "$APP/uninstall.sh"
chmod 755 "$APP/uninstall.sh"
# A downloaded bundle carries the quarantine flag, and macOS kills the
# unsigned binaries on start. These are the user's own copies: drop it.
xattr -dr com.apple.quarantine "$APP/bin" "$APP/extension" 2>/dev/null || true

# ---------------------------------------------------------------------------
# 2. Native host manifest for Chrome.
# ---------------------------------------------------------------------------
echo "==> Chrome native host: $NM_DIR/$HOST_NAME.json"
mkdir -p "$NM_DIR"
write_file "$NM_DIR/$HOST_NAME.json" "{
  \"name\": \"$HOST_NAME\",
  \"description\": \"cgpt-bridge native messaging host\",
  \"path\": \"$(json_escape "$APP/bin/cgpt-bridge-host")\",
  \"type\": \"stdio\",
  \"allowed_origins\": [
    \"chrome-extension://$EXT_ID/\"
  ]
}" 644

# ---------------------------------------------------------------------------
# 3. Gateway config: the gateway creates it (with a random token) on first
#    use and prints port and token. The token is not shown.
# ---------------------------------------------------------------------------
echo "==> Gateway config"
GW_CONFIG="$("$GW_NEW" --print-config)"
PORT="$(sed -n 's/.*"port":\([0-9]*\).*/\1/p' <<<"$GW_CONFIG")"
TOKEN="$(sed -n 's/.*"token":"\([^"]*\)".*/\1/p' <<<"$GW_CONFIG")"
if [[ -z "$PORT" || -z "$TOKEN" ]]; then
  echo "error: unexpected output of cgpt-gateway --print-config" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# 4. OpenCode provider. JavaScript for Automation (built into macOS) edits the
#    JSON: provider "cgpt" is set, small_model only if there is none, the rest
#    of the file is kept. It prints the new file, or nothing if it would not
#    change. The token reaches it through the environment, not argv.
# ---------------------------------------------------------------------------
echo "==> OpenCode provider cgpt: $OC"
OC_FILE="$(resolve_link "$OC")" || {
  echo "Nothing in it was changed, and the running gateway was left alone." >&2
  exit 1
}
OC_NEW="$(mktemp "${TMPDIR:-/tmp}/cgpt-bridge-opencode.XXXXXX")"
if ! CGPT_GATEWAY_TOKEN="$TOKEN" osascript -l JavaScript - "$OC_FILE" "$PORT" > "$OC_NEW" <<'JS'
ObjC.import('Foundation');
function run(argv) {
  var path = argv[0], port = argv[1];
  var token = $.NSProcessInfo.processInfo.environment.objectForKey('CGPT_GATEWAY_TOKEN').js;
  var exists = $.NSFileManager.defaultManager.fileExistsAtPath(path);
  var cfg = { '$schema': 'https://opencode.ai/config.json' };
  if (exists) {
    var text = $.NSString.stringWithContentsOfFileEncodingError(path, $.NSUTF8StringEncoding, null);
    if (text.isNil()) throw new Error('cannot read ' + path + ' as UTF-8');
    text = text.js;
    cfg = {};
    if (text.trim() !== '') {
      try { cfg = JSON.parse(text); }
      catch (e) { throw new Error(path + ' is not plain JSON (' + e.message + ')'); }
    }
  }
  var isObject = function (v) { return v !== null && typeof v === 'object' && !Array.isArray(v); };
  if (!isObject(cfg)) throw new Error(path + ': the top level is not an object');
  var before = JSON.stringify(cfg);
  if (cfg.provider === undefined) cfg.provider = {};
  if (!isObject(cfg.provider)) throw new Error(path + ': "provider" is not an object');
  cfg.provider.cgpt = {
    npm: '@ai-sdk/openai-compatible',
    name: 'ChatGPT Web (bridge)',
    options: { baseURL: 'http://127.0.0.1:' + port + '/v1', apiKey: token },
    models: {
      'chatgpt-web': { name: 'ChatGPT Web' },
      'local-title': { name: 'Local title' }
    }
  };
  if (cfg.small_model === undefined) cfg.small_model = 'cgpt/local-title';
  if (exists && JSON.stringify(cfg) === before) return '';
  return JSON.stringify(cfg, null, 2);
}
JS
then
  cat >&2 <<EOM
error: could not update $OC (see above). Nothing was changed, and the running
gateway was left alone. Add the provider by hand (the token is "token" in
$HOME/Library/Application Support/cgpt-gateway/config.json):
  "provider": { "cgpt": {
    "npm": "@ai-sdk/openai-compatible",
    "options": { "baseURL": "http://127.0.0.1:$PORT/v1", "apiKey": "<token>" },
    "models": { "chatgpt-web": {}, "local-title": {} } } },
  "small_model": "cgpt/local-title"
EOM
  exit 1
fi
NEW_OC="$(cat "$OC_NEW")"
if [[ -n "$NEW_OC" ]]; then
  mkdir -p "$(dirname "$OC_FILE")"
  if [[ -f "$OC_FILE" ]]; then
    # A fresh owner-only copy: `cp -p`, or copying over an older .bak, would
    # keep a mode that lets others read it until the chmod below.
    (umask 077 && rm -f "$OC.bak" && cp "$OC_FILE" "$OC.bak")
    echo "    previous file kept as $OC.bak"
  fi
  write_file "$OC_FILE" "$NEW_OC" 600
else
  echo "    already up to date"
fi
# The file holds the gateway token: owner-only, like the gateway config. A
# .bak of an earlier run holds it too.
if [[ "$(stat -f '%Lp' "$OC_FILE")" != 600 || -n "$NEW_OC" ]]; then
  chmod 600 "$OC_FILE"
  echo "    $OC_FILE holds the gateway token: mode set to 600 (only you can read it)"
fi
[[ -f "$OC.bak" ]] && chmod 600 "$OC.bak"

# ---------------------------------------------------------------------------
# 5. OpenCode agents cgpt and cgpt-sub: the bundled files, as they are. cgpt
#    may start no subagent but cgpt-sub (OpenCode's own subagents, such as
#    general, run every command without asking). An agents/<name>.md is ours
#    when it names the model cgpt/chatgpt-web and its description starts with
#    "ChatGPT Web (bridge)" (uninstall.sh uses the same marks); one the user
#    wrote is not replaced. The install folder keeps a copy of the template
#    each agent was written from: a file equal to it is unedited and is
#    replaced as is; any other file of ours is an edit and is kept as
#    <name>.md.bak first (also when there is no such copy). So an unedited
#    older template never overwrites a saved edit. Nothing else in OpenCode
#    is switched: default_agent stays as it is, the user picks the agent with
#    Tab. The agents are optional: when one cannot be written the install
#    goes on.
# ---------------------------------------------------------------------------
# install_agent <name>: sets AGENT_OK=true when agents/<name>.md is ours after
# the step (written now, unchanged, or left from an earlier run).
install_agent() {
  local name="$1"
  local agent="$AGENTS/$name.md"
  local template="$SRC/opencode-agent-$name.md"
  local installed="$APP/opencode-agent-$name.md"
  local file result backup
  AGENT_OK=false
  echo "==> OpenCode agent $name: $agent"
  if [[ "$NO_AGENT" == true ]]; then
    echo "    not added (--no-agent)"
    # An agent of an earlier run stays, and stays usable.
    if [[ -f "$agent" ]] && is_our_agent "$agent"; then AGENT_OK=true; fi
    return 0
  fi
  if ! file="$(resolve_link "$agent" 2>/dev/null)"; then
    echo "    not added: $agent is a symlink loop"
  elif [[ -d "$file" ]]; then
    echo "    not added: $agent is a folder; left as it is"
  elif [[ -f "$file" ]] && ! is_our_agent "$file"; then
    echo "    not added: $agent is not ours (another agent named $name); left as it is"
  elif cmp -s "$template" "$file"; then
    echo "    agent $name: unchanged"
    AGENT_OK=true
  else
    backup=false
    if [[ ! -f "$file" ]]; then
      result="added"
    elif cmp -s "$installed" "$file"; then
      result="updated"
    else
      result="updated (previous saved as $name.md.bak)"
      backup=true
    fi
    if write_agent "$template" "$file" "$agent" "$backup"; then
      echo "    agent $name: $result"
      AGENT_OK=true
    else
      echo "warning: could not write $agent (see above): agent $name not added; the rest of the install goes on" >&2
    fi
  fi
  # The template the agent file now comes from, for the next run and for
  # uninstall.sh. Left as it is when the agent was not (re)written.
  if [[ "$AGENT_OK" == true ]]; then
    cp "$template" "$installed" 2>/dev/null || true
  fi
}
install_agent cgpt-sub
# A cgpt-sub the user wrote has its own permissions, and cgpt may start it.
if [[ "$AGENT_OK" != true && "$NO_AGENT" == false && -e "$AGENTS/cgpt-sub.md" ]]; then
  echo "warning: agent cgpt may start cgpt-sub, which is not ours: it runs with its own permissions ($AGENTS/cgpt-sub.md)" >&2
fi
install_agent cgpt
AGENT_READY="$AGENT_OK"

# ---------------------------------------------------------------------------
# 6. LaunchAgent file, prepared next to its target.
# ---------------------------------------------------------------------------
echo "==> LaunchAgent: $PLIST"
mkdir -p "$(dirname "$PLIST")"
cp "$SRC/$LABEL.plist" "$TMP_PLIST"
plutil -replace ProgramArguments -json "[\"$(json_escape "$APP/bin/cgpt-gateway")\"]" "$TMP_PLIST"
plutil -replace EnvironmentVariables.TMPDIR -string "$(getconf DARWIN_USER_TEMP_DIR)" "$TMP_PLIST"
plutil -replace StandardOutPath -string "$APP/gateway.log" "$TMP_PLIST"
plutil -replace StandardErrorPath -string "$APP/gateway.log" "$TMP_PLIST"
plutil -lint -s "$TMP_PLIST"
chmod 644 "$TMP_PLIST"

# ---------------------------------------------------------------------------
# 7. Swap: stop the running gateway, put the new binary and LaunchAgent in
#    place, start it and check that it answers with the token.
# ---------------------------------------------------------------------------
STATUS=0
if [[ "$NO_LAUNCHCTL" == false ]]; then
  launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
fi
mv -f "$GW_NEW" "$APP/bin/cgpt-gateway"
mv -f "$TMP_PLIST" "$PLIST"

if [[ "$NO_LAUNCHCTL" == false ]] && ! start_gateway; then
  echo "error: launchd did not load the LaunchAgent: the gateway is not running." >&2
  echo "       Try: launchctl bootstrap gui/$(id -u) \"$PLIST\"" >&2
  STATUS=1
elif [[ "$NO_LAUNCHCTL" == false ]]; then
  CODE=000
  END=$((SECONDS + 6))
  while (( SECONDS < END )); do
    CODE="$(gateway_status)"
    [[ "$CODE" == 200 ]] && break
    sleep 0.5
  done
  case "$CODE" in
    200)
      echo "    gateway answers on http://127.0.0.1:$PORT (log: $APP/gateway.log)" ;;
    401)
      echo "error: port $PORT answers but rejects the gateway token: another program uses port $PORT." >&2
      echo "       Free the port or set another \"port\" in $HOME/Library/Application Support/cgpt-gateway/config.json, then run install.sh again." >&2
      STATUS=1 ;;
    *)
      echo "error: the gateway does not answer on http://127.0.0.1:$PORT/v1/models." >&2
      echo "       Is port $PORT taken by another program? Check: lsof -nP -iTCP:$PORT -sTCP:LISTEN" >&2
      echo "       Gateway log: $APP/gateway.log" >&2
      STATUS=1 ;;
  esac
else
  echo "    not started (--no-launchctl)"
fi

cat <<EOM

Done. Next steps:
  1. Chrome → chrome://extensions → Developer mode → Load unpacked → choose
       $APP/extension
     (in the file dialog press Cmd+Shift+G and paste the path).
     Already loaded from this folder? Press ↻ on the cgpt-bridge card: only
     then does Chrome start the new native host. Loaded from anywhere else
     (a source checkout, an older bundle)? Remove that card first.
  2. Open https://chatgpt.com and log in (chat history on). On the first
     OpenCode request the extension opens its own ChatGPT window: leave it
     open, do not type in it, and click once inside its page.
EOM
if [[ "$AGENT_READY" == true ]]; then
  cat <<EOM
  3. In OpenCode press Tab to switch to the agent cgpt, or start it with
       opencode --agent cgpt
     It uses cgpt/chatgpt-web and asks before every command, file edit, web
     access, MCP tool and skill; of the subagents it may start only cgpt-sub,
     which asks the
     same way. It is not made the default agent: OpenCode starts with your
     usual agent and model, and you pick cgpt with Tab. If you choose the
     model cgpt/chatgpt-web in another agent (/models), nothing asks.
EOM
else
  cat <<EOM
  3. In OpenCode choose the model cgpt/chatgpt-web (/models), or set
       "model": "cgpt/chatgpt-web"  in $OC
EOM
fi
cat <<EOM
  4. Optional, the cgpt CLI on PATH:
       ln -sf "$APP/bin/cgpt" /usr/local/bin/cgpt
Warning: when OpenCode starts, it sends the unfinished turns of old sessions
(for example of a killed opencode run) to ChatGPT again. Delete the sessions
you do not want to go on (/sessions in the TUI); see the README.
Uninstall: "$APP/uninstall.sh"
EOM
exit "$STATUS"
