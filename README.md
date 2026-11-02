# cgpt-bridge

A personal local bridge between your terminal or OpenCode and ChatGPT in your Google Chrome, logged in with your own account.

**Status:** pre-MVP implementation in progress. The repository now contains the Rust workspace, Chrome extension sources, Native Messaging host, shared protocol crate, installer scripts, and Stage 1 documentation in `docs/`.

`cgpt-bridge` is **not** affiliated with, endorsed by, or approved by any vendor. It is a tool for personal, user-initiated, interactive use of your own ChatGPT account in your own browser. For programmatic, batch, server-side, or production integrations, prefer official APIs.

---

## What it does

- Lets you send a prompt from your terminal (`cgpt ask`, `cgpt agent`) to a ChatGPT tab you have open, without copy-paste, keeping the conversation visible in the browser.
- Lets OpenCode use ChatGPT as its model through the local gateway `cgpt-gateway`. Those requests run in the extension's own ChatGPT window, never in your tabs, one chat per OpenCode session.
- Lets ChatGPT propose **one shell command at a time** that the CLI shows to you and runs **only after you confirm** — useful for debugging sessions, repo exploration, and small diagnosis tasks.
- Keeps a local, human-readable plan (`.cgpt-bridge/plan.md`) and an append-only event log (`.cgpt-bridge/plan.jsonl`) for the task you are working on.

The core principle is:

> **ChatGPT proposes. The CLI validates and shows. The user approves. The CLI runs. The result is returned to ChatGPT. The plan is updated locally.**

---

## What it does not do

- It does **not** scrape, export, or browse your ChatGPT history.
- It does **not** run as a background monitor, scheduler, or batch job. The one background process is the optional OpenCode gateway (`cgpt-gateway`): it only answers requests from OpenCode on this machine.
- It does **not** bypass login, CAPTCHA, rate limits, paywalls, or any ChatGPT protection.
- The `cgpt` CLI does **not** auto-run commands proposed by ChatGPT by default. Two opt-in flags (`--auto-readonly`, `--yolo`) relax the keypress prompt for trusted contexts; denylist hits remain blocked regardless. See `docs/security.md` §3. Through the gateway it is OpenCode that runs ChatGPT's tool calls, and with its default settings it does not ask (see "Recommended OpenCode settings" below).
- The `cgpt` CLI does **not** open a localhost HTTP or WebSocket port; it talks to the native host over a local socket (named pipe on Windows). The optional OpenCode gateway listens on `127.0.0.1` only and requires a bearer token on every request.
- It does **not** use the system clipboard unless you ask for it with `--buffer` (read the prompt from it, redacted like piped text) or `--copy` (copy the answer to it, not redacted).

See `docs/security.md` for the full safety model.

---

## Pieces

- **`cgpt` CLI** — Rust binary. The thing you run in your terminal.
- **`cgpt-gateway`** — Rust binary. Local OpenAI-compatible server for OpenCode on `127.0.0.1`, token-protected; started at login by the installer.
- **`core`** — Rust library shared by the CLI and the gateway: denylist and secret redaction.
- **Native Messaging host** — Rust binary. A thin bridge launched by Chrome on demand.
- **Chrome extension** — Manifest V3, TypeScript. Background service worker + content script.
- **Content script** — runs only on `https://chatgpt.com/*`. Inserts your prompt into the composer and reads the visible response.
- **ChatGPT DOM adapter** — `chatgptAdapter.ts`. All ChatGPT-specific selectors and behavior live here so that UI changes are localized to one file.

---

## Install

There are three install paths: **OpenCode with ChatGPT** (Windows and
macOS, prebuilt bundle with installer), **CLI only** (macOS and Linux,
prebuilt bundle) and **developer** (clone the repo and build locally).

### OpenCode with ChatGPT (Windows and macOS)

`cgpt-gateway` is a local OpenAI-compatible server. OpenCode sends its
requests to it; the gateway passes them to ChatGPT in your Chrome through
the extension and the native host, each OpenCode session in its own chat.

You need **Google Chrome** with a ChatGPT login and **OpenCode** (install it
with its official installer). No admin rights, no Rust or Node.

**Get the bundle.** GitHub → Actions → workflow **build** → filter by branch
`main` → the latest **green** (successful) run → artifact
`cgpt-bridge-<version>-<build>-windows-x64` or
`cgpt-bridge-<version>-<build>-macos-<arch>`. `<build>` is the run's commit
(12-character short SHA): every part of the bundle reports it, so a mixed
install shows up (see "Which build is running" below). GitHub always
downloads an artifact as a `.zip` that wraps the real archive: unzip it
first, then unpack the `cgpt-bridge-<version>-<build>-windows-x64.zip` or
`cgpt-bridge-<version>-<build>-macos-<arch>.tar.gz` inside. The macOS bundle is
built for Apple silicon (`arm64`) only; on an Intel Mac build it from source
with `./scripts/build-release.sh` (see "Developer: clone + build"; no
Intel build is made or tested in CI). Developers
build the same with `./scripts/build-release.sh` (macOS) or
`pwsh ./scripts/build-release.ps1` (Windows).

**Verify the download.** The artifact also holds `SHA256SUMS` with the
SHA-256 of the archive (the build script writes it too, next to the archive in
`dist/`). Check it before you install, in the folder with both files:

```sh
# macOS
shasum -a 256 -c SHA256SUMS
```

```powershell
# Windows: prints OK or MISMATCH
if ((Get-FileHash .\cgpt-bridge-<version>-<build>-windows-x64.zip -Algorithm SHA256).Hash -ieq ((Get-Content .\SHA256SUMS) -split '\s+')[0]) { 'OK' } else { 'MISMATCH' }
```

The checksum travels in the same artifact as the archive, so it catches a
damaged or mixed-up download, not a tampered artifact.

**macOS:**

```sh
tar xzf cgpt-bridge-<version>-<build>-macos-arm64.tar.gz
cd cgpt-bridge-<version>-<build>-macos-arm64
./install.sh
```

**Windows** (unpack the `.zip`, then in PowerShell inside that folder):

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

The installer:

| | macOS | Windows |
|---|---|---|
| Files (binaries, extension, uninstaller) | `~/Library/Application Support/cgpt-bridge/` | `%LOCALAPPDATA%\cgpt-bridge\` |
| Native host registered with Chrome | manifest in `~/Library/Application Support/Google/Chrome/NativeMessagingHosts/` | manifest + key `HKCU\Software\Google\Chrome\NativeMessagingHosts\com.cgpt_bridge.host` |
| Gateway config with a random token | `~/Library/Application Support/cgpt-gateway/config.json` | `%APPDATA%\cgpt-gateway\config.json` |
| OpenCode provider `cgpt` (the file then holds the gateway token) | `~/.config/opencode/opencode.json`, made owner-only (mode 600) | `%USERPROFILE%\.config\opencode\opencode.json`, ACL of your profile kept (private to you by default) |
| OpenCode agent `cgpt` (asks before commands, file edits and web fetches) | `~/.config/opencode/agents/cgpt.md` | `%USERPROFILE%\.config\opencode\agents\cgpt.md` |
| Gateway autostart, started right away and checked (`GET /v1/models` with the token) | LaunchAgent `~/Library/LaunchAgents/com.cgpt-bridge.gateway.plist` | registry value `cgpt-gateway` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, which runs `start-gateway.ps1` from the install folder (a `cgpt-gateway.lnk` of an earlier version is removed from the Startup folder) |
| Gateway log | `…/cgpt-bridge/gateway.log` | `…\cgpt-bridge\gateway.log` |

In `opencode.json` it adds the provider `cgpt` (package
`@ai-sdk/openai-compatible`, `baseURL` `http://127.0.0.1:8765/v1`, the
gateway token as `apiKey`, models `chatgpt-web` and `local-title`) and sets
`small_model` to `cgpt/local-title` only if you have no `small_model` yet,
so session titles are made locally and never cost a ChatGPT message.
Everything else in the file stays as it was; the previous file is kept next
to it as `opencode.json.bak`. OpenCode also merges
`~/.config/opencode/opencode.jsonc` if you have one: check that it does not
override `provider.cgpt` or `small_model`. A symlinked `opencode.json` (dotfiles) stays a
link: the file it points to is edited. Running the installer again changes
nothing that is already in place. If the gateway does not answer after the
start (for example another program holds port 8765), the installer says so
and exits with status 1. The check only proves that something on that port
answers `/v1/models` with this token; whether the extension and ChatGPT are
reachable shows on the first real request.

**The agent `cgpt`.** The installer also adds an OpenCode agent `cgpt` as a
file of its own, `agents/cgpt.md` next to `opencode.json` (it holds no
token). The agent uses the model `cgpt/chatgpt-web` and asks before every
shell command, file edit and web fetch (`bash`, `edit` and `webfetch` set to
`ask`). It cannot start subagents (`task` set to `deny`): a subagent has
your usual permissions and would run commands without asking.

- **You switch to it yourself.** The installer does not make `cgpt` the
  default agent and does not change `default_agent`: OpenCode starts with
  your usual agent and model as before. Press **Tab** in OpenCode to switch
  to the agent `cgpt`, or start it with `opencode --agent cgpt`.
- **The questions belong to the agent, not to the model.** If you pick the
  model `cgpt/chatgpt-web` in another agent (for example `build` through
  `/models`), commands run without asking.
- Its `ask` rules come after your own `permission` in `opencode.json` and
  win: in this agent even a command you `deny` there is asked about, not
  refused. Tools of your MCP servers, `websearch` and file reads (except
  `.env` files) do not ask.
- In a non-interactive `opencode run --agent cgpt` there is nobody to answer,
  so OpenCode refuses each of these actions.
- An `agents/cgpt.md` you wrote yourself (one without both of the
  installer's marks, the line `model: cgpt/chatgpt-web` and a description
  that starts with `ChatGPT Web (bridge)`) is not replaced. If you edited the
  installer's file, the next run (or the uninstaller) keeps your copy as
  `agents/cgpt.md.bak` before it replaces (or removes) the file; OpenCode does
  not load that copy. An unedited file of an older version is replaced
  without a `.bak`, so a saved edit is not overwritten by an update. The installer prints `agent cgpt: added`, `updated` or
  `unchanged`. `--no-agent` (macOS) or `-NoAgent` (Windows) skips the agent;
  an agent added by an earlier run then stays as it is. If the agent cannot
  be written, the installer warns and finishes without it.
- If `agents/cgpt.md` is a symlink (dotfiles), the installer writes the file
  it points to, and the uninstaller removes that file and leaves the link,
  now pointing at nothing, for you to delete.

Then:

1. Chrome → `chrome://extensions` → **Developer mode** → **Load unpacked** →
   the `extension` folder inside the install folder above (on macOS press
   Cmd+Shift+G in the file dialog and paste the path).
2. Open `https://chatgpt.com` and log in. Chat history must be on, and do
   not use a temporary chat for this (see below).
3. In OpenCode press **Tab** to switch to the agent **cgpt** (or start
   `opencode --agent cgpt`): it uses ChatGPT and asks before each command,
   edit and web fetch. Or pick the model **cgpt/chatgpt-web** in any agent:
   `/models` in the TUI, or `"model": "cgpt/chatgpt-web"` in
   `opencode.json`, or `opencode run -m cgpt/chatgpt-web "<task>"`; then
   nothing asks unless you set the permissions below. The installer never
   changes your `model` or `default_agent`.

**The first OpenCode request.** The extension opens its **own ChatGPT
window**, without focus, and works only there; your own ChatGPT tabs are
never used for OpenCode.

- Leave that window open and do not type in it. If you close it, the next
  request opens a new one.
- Click once inside the page of that window — the gateway's tab, not your
  own ChatGPT tab. Chrome lets the tab keep streaming in the background only
  after a click in it. Click again after that tab was fully reloaded.
- Each OpenCode session becomes a chat in your ChatGPT history; the gateway
  finds it again by its `chatgpt.com/c/<id>` address. Temporary chats and
  chat history turned off give a chat no address, and the request fails
  (`chat_url_missing`).
- That chat goes into the ChatGPT project named after OpenCode's workspace
  root (see "ChatGPT projects" below).
- After you reload the extension (↻ at `chrome://extensions`, for example
  after an update), the extension knows that window again by a mark its
  page keeps, and goes on using it. After a Chrome restart that restores
  the window it does so only on the first request, and only while the
  window holds just that tab and no other tab alone in its window carries
  the mark (a duplicated or reopened copy of the tab does). Otherwise the
  window becomes an ordinary tab and the next request opens a new one;
  close the old one then. A window opened by a version before this one
  has no mark: close it after the first update.
- Do not keep a copy of that tab (Duplicate, or a reopened closed tab):
  if the original was closed and Chrome restarts before the next request,
  the copy is taken for the gateway's window.

**Recommended OpenCode settings.**

- *Permissions.* By default OpenCode runs shell commands and edits files
  without asking; only the agent `cgpt` (above) asks. To confirm each one in
  every agent, add to `opencode.json`:

  ```json
  "permission": {
    "bash": "ask",
    "edit": "ask"
  }
  ```

  Without `ask`, the gateway's denylist is the only protection, and it is
  a heuristic, not a sandbox. It checks only the `command` of each tool call:
  file reads, writes and edits and web fetches are not checked, and it cannot
  see what a file written by one call does when another call runs it. Secret
  redaction only covers what is sent to ChatGPT. See `docs/security.md` §15.
- *Skills.* OpenCode also loads skills from `~/.claude/skills` and sends their
  list to ChatGPT with every new chat. To turn that off set
  `OPENCODE_DISABLE_CLAUDE_CODE_SKILLS=1` (macOS: `export` it in your shell
  profile; Windows: `setx OPENCODE_DISABLE_CLAUDE_CODE_SKILLS 1`, then open a
  new terminal).
- *Scripted runs.* `opencode run --standalone "<task>"` was more reliable in
  testing than going through the background service (`opencode serve
  --service` once hung at the start of a turn).

**Gateway config.** `config.json` holds `port` (8765), `token`,
`turn_timeout_ms` (900000) and `allow_tools`. ChatGPT does not get OpenCode's
`execute` tool unless you add `"execute"` to `allow_tools`. After editing the
file restart the gateway (log out and in, or run the installer again). A
key the gateway does not know (a typo such as `allowed_tools`) is ignored; the
log names it in a line `unknown key(s) ignored`, and the gateway still starts.

**Updating.** Unpack the new bundle and run the installer again; Chrome may
stay open. Everything is prepared first, and the running gateway is stopped,
replaced and started only at the end, so a failed run (for example an
`opencode.json` with comments, which the installer refuses to edit) leaves
it running. Then press ↻ on the cgpt-bridge card at `chrome://extensions`:
Chrome keeps the old native host process running until the extension is
reloaded (on Windows the installer renames that host's file aside and
deletes it on a later run). The gateway's window stays in use after the
reload (see "The first OpenCode request" above); after the first update to
this version close the old window once.

**Troubleshooting.**

- *Windows: do not run Chrome as administrator.* The bridge's named pipe is
  then created by the elevated account, and the gateway (not elevated)
  cannot reach it.
- *macOS 13 and later: the gateway does not start.* macOS shows "Background
  item added" after the install; if the `cgpt-gateway` entry is switched off
  in System Settings → General → Login Items (Allow in the Background),
  launchd does not start the LaunchAgent. Switch it on and run `install.sh`
  again.
- *Which build is running.* Every line of the gateway log (`gateway.log`)
  starts with its UTC time, `2026-10-27T12:34:56Z`. The log starts with
  `cgpt-gateway v<version> (build <build>) starting`, and the first request
  adds `cgpt-gateway: builds: gateway …, host …, extension …`. The line says
  `warning: builds differ` when the host or the extension is from another
  build (`(none)`: older than build ids); it is written again whenever they
  change. Usual cause: the extension was not reloaded after an update. The
  extension card at `chrome://extensions` shows the build next to the
  version, and the native host names it in its first stderr line. An older
  native host drops the extension's build on the way, so the line then says
  `extension (none)` for a current extension too (with `host (none)`). The
  other way round, a gateway or a `cgpt` older than the error codes
  `cancelled` and `tab_busy` (a stale copy on `PATH`, or a gateway left
  running by an install with `--no-launchctl`) cannot read them from a
  current host: a stopped ask or a busy tab fails there as an unreadable
  answer (`cgpt` exits with `7`). The installer replaces all parts together;
  run it again.
- *Requests fail with «Расширение cgpt-bridge устарело».* This also happens
  when Chrome has an old build of the extension loaded: load the `extension`
  folder of the install folder (see step 1 above) and reload it.
- *Requests fail with `chat_url_missing`.* Turn chat history on and do not
  use a temporary chat in the gateway's window.
- *Requests fail with `tab_busy`.* An earlier answer was still generating in
  the gateway's window after Stop, or another ask was running there. The
  request was refused before anything was typed, so OpenCode sends it again by
  itself; you see the error only when that keeps failing. Look at the
  gateway's window. A request that had already sent a part (the start of a
  long message, or the question a repair follows) fails as a DOM failure
  instead and is not sent again.
- *Error codes after the reply has started.* Once the gateway has started
  streaming a reply, it names two errors in the stream differently from its
  HTTP answers: `bridge_offline` for `extension_unavailable` (no extension,
  tab, sign-in or host) and `bridge_internal` for `internal_error`. Before the
  stream they stay HTTP 503 `extension_unavailable` and 500 `internal_error`.
  The new names keep OpenCode from sending such a request to ChatGPT again: it
  retries a code that contains `unavailable`, and `internal_error`, whatever
  else the error says. Scripts that look for the old names in the stream need
  an update.
- *Requests fail with `answer_not_detected` or `target_mismatch`.* Both reach
  OpenCode as a DOM failure. `answer_not_detected`: the page showed no reply,
  either none 30 seconds after the prompt was sent, or the answer stream ended
  without text and the page showed none either (an error in the stream,
  moderation, an answer that is not text, or ChatGPT's markup changed).
  `target_mismatch`: the gateway's window no longer showed the chat of the
  request, before the prompt was sent or when the answer was read (you
  switched that window to another chat). Do not use the gateway's window for
  your own chats. Nothing is stopped after a `target_mismatch`: what
  generates there is your other chat. Details: `extension/README.md`.

**Uninstall.** `~/Library/Application Support/cgpt-bridge/uninstall.sh`
(macOS) or
`powershell -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\cgpt-bridge\uninstall.ps1"`
(Windows). It removes only what the installer created: the files, the
gateway config and sessions, the autostart entry, the native host
registration (if it still points at the installed host), provider `cgpt`
in `opencode.json` (plus `small_model` if it is `cgpt/local-title` and
`model` if it is a `cgpt/` model, `default_agent` if it is `cgpt` and no
agent `cgpt` is left) and the agent file `agents/cgpt.md` if it is the
installer's (the `agents` folder too when nothing else is left in it).
Remove the extension at `chrome://extensions` yourself.

### OpenCode in WSL

Not supported out of the box (WSL1 is untested: it shares the Windows
loopback, so it may work, but nothing here checks it). Run OpenCode in Windows (PowerShell or Windows
Terminal), where the installer sets everything up. Three reasons:

- The gateway listens on `127.0.0.1` of Windows. In WSL2's default (NAT)
  networking, `127.0.0.1` inside WSL is the Linux side, not Windows, so
  OpenCode in WSL gets "connection refused".
- The installer writes the provider into the Windows `opencode.json`
  (`%USERPROFILE%\.config\opencode\`), not into the one in your WSL home.
- The `cgpt` CLI is not supported on Windows, and a `cgpt` inside WSL looks
  for a Unix socket while the Windows host listens on a named pipe, so it
  cannot reach the bridge.

For WSL2, **mirrored networking** is the way to use OpenCode from WSL:
in `%USERPROFILE%\.wslconfig` set `networkingMode=mirrored` under `[wsl2]`
(Windows 11 22H2 or later), run `wsl --shutdown`, and `127.0.0.1` in WSL then
reaches the Windows gateway. Add the provider `cgpt` by hand to
`~/.config/opencode/opencode.json` in WSL: copy the `cgpt` block from the
Windows file (same `baseURL` and `apiKey`; the token is also in
`%APPDATA%\cgpt-gateway\config.json`) and, unless you already have a
`small_model`, add `"small_model": "cgpt/local-title"`, as the installer does,
so that session titles are made locally and never cost a ChatGPT message
(check that no `opencode.jsonc` overrides it). The file now holds the gateway
token: run `chmod 600 ~/.config/opencode/opencode.json`. Chrome, the
extension and the gateway stay on Windows. The installer and CI do not check this setup.

### CLI only: install from a release bundle (macOS, Linux)

The only third-party software you need on the target machine is **Google
Chrome** (or Chromium). All Rust / Node toolchains are baked into the
prebuilt bundle.

Optional runtime extras (no install blocker — `cgpt` falls back gracefully):

- **Clipboard reader** — only needed if you use `--buffer`. macOS ships
  `pbpaste` by default. On Linux install one of `wl-clipboard` (Wayland,
  provides `wl-paste`), `xclip`, or `xsel`.

Pretty markdown rendering of the final agent message is built in via
`termimad` + `syntect` — no external binary required. Code blocks inside
the final message are syntax-highlighted from the fence info string
(```` ```rust ````, ```` ```sh ````, etc.).

Two artifact formats — pick one:

**Option A — `.dmg` (macOS, GUI-friendly)**

1. Copy `cgpt-bridge-<version>-<build>-macos-<arch>.dmg` to the target Mac, double-click to mount.
2. Drag the `cgpt-bridge` folder out of the DMG into a stable location (e.g. `~/Applications/cgpt-bridge/`). Do **not** keep it inside the mounted DMG — the Native Messaging manifest stores an absolute path.
3. Double-click `Install.command` from the DMG. It:
   - Copies `bin/cgpt` to `/usr/local/bin/cgpt` (may prompt for `sudo`).
   - Writes the Native Messaging manifest to the user's Chrome profile.
4. Open Chrome → `chrome://extensions` → enable **Developer mode** → **Load unpacked** → pick the `extension/` folder inside the `cgpt-bridge` folder you placed in step 2. The extension id is pinned via the manifest `key` field, so it always resolves to `oplkebjcjmifidmnbehpadfakodjjoge` — no need to copy anything.
5. Reload the extension card once (`↻`) so it picks up the manifest you just wrote.
6. Open `https://chatgpt.com` and click anywhere inside the tab once. This satisfies Chrome's autoplay policy so the extension can keep the tab responsive when it is hidden.
7. Smoke test from any terminal:
   ```sh
   cgpt --version
   cgpt ask "hello"
   ```

**Option B — `.tar.gz` (macOS or Linux, CLI-friendly)**

```sh
scp cgpt-bridge-<version>-<build>-<os>-<arch>.tar.gz othermachine:~
ssh othermachine
tar xzf cgpt-bridge-<version>-<build>-<os>-<arch>.tar.gz
cd cgpt-bridge-<version>-<build>-<os>-<arch>

# Install the Native Messaging manifest. No extension id argument needed —
# it is pinned in the bundled manifest.
./install/macos/install-host.sh         # macOS
./install/linux/install-host.sh         # Linux (add --chromium for Chromium)

# Put cgpt on PATH.
sudo cp bin/cgpt /usr/local/bin/cgpt    # macOS / Linux
# or, no-sudo variant:
mkdir -p ~/.local/bin && cp bin/cgpt ~/.local/bin/cgpt
```

Then Chrome → `chrome://extensions` → Load unpacked → pick the `extension/`
folder from the unpacked bundle, and click once inside `chatgpt.com` as in
step 6 above.

A `.tar.gz` from CI (artifact `cgpt-bridge-<version>-<build>-macos-arm64` or
`cgpt-bridge-<version>-<build>-linux-x86_64`, from the latest green run on
`main`) comes with `SHA256SUMS`; check it before
unpacking, in the folder with both files: `shasum -a 256 -c SHA256SUMS`
(macOS) or `sha256sum -c SHA256SUMS` (Linux). As above, this catches a damaged
or mixed-up download, not a tampered artifact. A `.dmg` built with
`./scripts/build-dmg.sh` is listed in the same `SHA256SUMS`.

Full per-platform notes ship inside the bundle as `INSTALL.md`.

### Developer: clone + build

Use this path if you intend to edit the source.

Prerequisites:

- **Rust** (stable). Install via [rustup](https://rustup.rs/).
- **Node.js** 20.19+ or 22.12+ (vite 8) and **npm**.
- **Git**.
- **Google Chrome** (or Chromium).
- **openssl** — only if you regenerate the extension keypair. Ships with macOS by default; on Linux it is usually preinstalled or one `apt install openssl` away.

Build the everything bundle in one command:

```sh
git clone https://github.com/elenik72/cgpt-bridge.git
cd cgpt-bridge
./scripts/build-release.sh
```

That produces `dist/cgpt-bridge-<version>-<build>-<os>-<arch>/` plus a matching
`.tar.gz` and `dist/SHA256SUMS`. On macOS, `./scripts/build-dmg.sh`
additionally wraps the bundle in a `.dmg` and adds it to `dist/SHA256SUMS`.

For day-to-day development you usually want to skip the bundler and
symlink the dev binary so each `cargo build --release` is picked up
automatically:

```sh
cargo build --release -p cgpt-bridge-cli -p cgpt-bridge-host -p cgpt-gateway
sudo ln -sf "$PWD/target/release/cgpt" /usr/local/bin/cgpt

# extension
cd extension && npm install && npm run build && cd ..
# Chrome → chrome://extensions → Load unpacked → ./extension/dist

# Native Messaging manifest pointing at target/release/cgpt-bridge-host.
./install/macos/install-host.sh         # or install/linux/...
```

Then `cgpt ask "hello"` from any terminal.

### Dev workflow cheat sheet

| You changed | Rebuild | Reload |
|---|---|---|
| `cli/src/*.rs` | `cargo build --release -p cgpt-bridge-cli` | nothing (cgpt symlinked) |
| `host/src/*.rs` | `cargo build --release -p cgpt-bridge-host` | reload extension |
| `extension/src/*.ts` | `cd extension && npm run build` | reload extension card in `chrome://extensions` |
| `extension/manifest.json` (key changed) | `cd extension && npm run build` | reload extension **and** re-run `install-host.sh` |
| Anything for a fresh artifact | `./scripts/build-release.sh` (and `./scripts/build-dmg.sh` for DMG) | — |

---

## Basic usage examples

> The examples below describe the intended v0.1 interface. Some commands may depend on the current milestone and local installation state.

Send a one-shot prompt from your terminal:

```sh
cgpt ask "explain this error"
```

Pipe a failing test run into a prompt:

```sh
cargo test 2>&1 | cgpt ask "why is this failing?"
```

What you pass in (words, piped text, `--buffer`) is redacted with the same
secret rules as command output before it is sent, and one line on stderr names
the rules that fired. A secret in a pasted `.env` becomes `«redacted:…»`. To
send the text as it is, add `--no-redact`; to read what will go out first, add
`--editor`. This is a heuristic: a password written in a sentence is missed, a
log line with more than 6 prefix fields is not looked through, and ordinary
code is sometimes changed (see "Known limits" in `docs/security.md` §7). Do not
paste what ChatGPT must not see.

Run an interactive agent loop where ChatGPT proposes commands you approve one at a time:

```sh
cgpt agent "diagnose failing tests"
```

Pass the task via the OS clipboard (no typing, no piping):

```sh
# copy a long task description to the clipboard, then:
cgpt agent --buffer
# ...or prepend a short lead-in:
cgpt agent --buffer "fix this:"
```

Compose a longer prompt in your editor (uses `$EDITOR`, falls back to `vi`
then `nano`):

```sh
cgpt ask --editor
cgpt agent --editor
```

Continue the last agent session in this project (skips re-sending the
prompt contract — ChatGPT already has the context in its tab):

```sh
cgpt agent -c "follow-up question"
cgpt agent --resume s19e3c30c179_151c2 "another follow-up"
```

Browse and replay past sessions offline (no ChatGPT round-trip):

```sh
cgpt history               # list sessions, newest first
cgpt last                  # re-render the latest final message
cgpt last --copy           # ...and copy it to the clipboard
cgpt replay <session-id>
```

Copy the final assistant message to the clipboard as part of any run:

```sh
cgpt ask "summarise X" --copy
cgpt agent "produce a report" --copy
```

### ChatGPT projects

A new chat the bridge opens goes into the ChatGPT project named after the
project folder you work in. `cgpt ask` and `cgpt agent` take the repository
root above the folder they run in (the nearest folder with `.git`, looked for
below your home folder), else the folder itself. A git worktree has its own
`.git` file, so it is named after its own folder, not the main repository's;
a folder reached through a symlink is named after its real folder. The gateway takes OpenCode's
workspace root (the `Workspace root folder:` line of OpenCode's system
prompt), else its working directory (`Working directory:`). When there is no
project of that name, the bridge creates it (memory setting left as ChatGPT
proposes it); when there is one, the chat goes into it. In your home folder
and at a disk root there is no project and the chat is a plain new chat, as
before. A folder name that cannot be a project name (more than 100
characters, control characters, or text the redaction rules change) also
gives a plain new chat, with the warning below.

- Only a **new** chat is affected. A `cgpt ask` into a tab that shows a chat
  continues that chat where it is; so do the later turns of `cgpt agent` and
  of an OpenCode session. For the CLI a new chat is a tab on ChatGPT's start
  page or on a project's page with no message yet.
- The project is found by its name in the sidebar (compared in Unicode NFC,
  so a macOS folder name finds it; the "Projects" section is opened when it
  is collapsed, and the project rows get 5 s to load before a project is
  created). The extension then remembers the project's id for the name
  (`chrome.storage.local`): the gateway opens that project's page directly
  next time, even when the sidebar does not show it, and the CLI looks for
  its row first. The CLI cannot load your tab, so when neither that row nor
  one with the name is in view, it creates the project again (a duplicate). A remembered id is forgotten when its page does not open
  (another account, a deleted project), when its row shows another name, or
  when a chat in it gets no address; the sidebar is then searched again.
- If the project can be neither found nor created (the sidebar is hidden,
  the page markup changed), the chat goes to a plain new chat and the bridge
  warns: a `cgpt: warning: …` line on stderr, a `warning` line in the gateway
  log. A collapsed "Projects" list that does not open is such a failure: no
  project is created while the list may be hiding it.
- **Same name, same project.** Two folders with the same name (`~/a/app` and
  `~/b/app`) share one project, with its memory and files.
- **Renamed project.** After you rename a project in ChatGPT, new chats keep
  going into it while its row is not in view; once the sidebar shows it
  under the new name, a new project with the folder's name is created.
- **Duplicates.** With more than five projects ChatGPT lists the five newest
  and a "show more" control. When the project's row is not in view the
  extension presses that control once and looks again before it creates a
  project. A project still not found then (say, a list that is shortened in
  another way) is created a second time with the same name. A live check with
  many projects is a release gate for this feature.
- The project name travels in the `ask` request (`project`). A native host
  older than this field drops it, and an extension older than it ignores it:
  either way the chat is a plain new chat with no warning. The host, the
  extension and the clients ship in one bundle, so update them together.

`--buffer`/`--editor`/`--copy`/`--no-redact` work for both `ask` and `agent`.
`--buffer` text is redacted like piped text. `--copy` is not redacted: it copies
ChatGPT's answer as received. When the
final agent message arrives and stdout is a TTY, it is rendered as pretty
markdown via the built-in `termimad` + `syntect` pipeline. When stdout is
piped or redirected, there is no terminal rendering. Use `--no-pretty` to
get the same plain output on a TTY. Plain is not the text byte for byte: the
CLI turns a message whose newlines arrived as literal `\n` back into lines and
shows control characters (terminal escape codes) escaped. Without `--no-pretty`
(on a TTY, and also when piped) it also wraps a bare block of code, one ChatGPT
forgot to fence, in a fence, so a piped result can hold a fence ChatGPT did not
write; `--no-pretty` does not do that. This applies to the final agent message
and to `cgpt replay` and `cgpt last`; `cgpt ask` prints the answer text as it
came.

`cgpt doctor` (an install health check) is planned for M10 and is **not
implemented**: the command does not exist yet.

### Exit codes

`cgpt ask` and `cgpt agent` share one table (details: `docs/requirements.md` §9):
`0` ok, `1` generic failure, `2` usage error (no prompt or task, unknown option,
host `bad_request`, a prompt too large for the native host's 1 MiB request
limit, refused before anything is sent), `3` setup, `4` transport, `5` tab unavailable (also a
signed-out ChatGPT window, a timeout, a changed chat), `6` DOM failure, `7`
protocol violation, `8` busy tab (see below), `10` user cancelled (also an ask
stopped on purpose: Stop pressed in the ChatGPT page, or a `stop` request),
`11` internal error.
`9` is reserved and never returned. Ctrl+C has no code: `cgpt` ends as killed
by it (shells show `130`, so a loop around `cgpt` stops too; Windows:
`STATUS_CONTROL_C_EXIT`).

**Ctrl+C while `cgpt ask` waits for the answer** stops that answer in ChatGPT:
the CLI asks the native host to press Stop for this ask only (in your tab, only
while this ask's answer is the one being written, and only while the tab still
shows the ask's chat: the chat it was sent from, or the new chat it started),
waits up to 3 seconds, prints one line on stderr and then ends as killed by
Ctrl+C. The line is "stopped: the answer in ChatGPT was interrupted" only when
the extension confirmed it, and "stopped: the prompt was not sent to ChatGPT"
when the stop came before the prompt was sent (the extension was still choosing
the tab, the ask waited behind another one in the same tab, or the page had not
sent it yet): the ask then ends and nothing of it reaches the chat. Otherwise it
says the answer was not stopped (nothing of this ask was being written, the tab
shows another chat, or the native host is too old and needs an update) or that
the stop was not confirmed and the answer may still be generating (an extension
that was not reloaded after an update ignores the stop). A second Ctrl+C ends `cgpt` at once. Ctrl+C
before the prompt has been handed to the native host ends `cgpt` as it always
did. The same holds for the ask of the current turn of `cgpt agent`, which then
marks the session cancelled; Ctrl+C while a command runs is unchanged.

**`cgpt ask` while ChatGPT is still answering.** `cgpt ask` types into your own
ChatGPT tab and presses Stop there only on Ctrl+C, for its own answer. If an
answer is still generating in that tab (your previous `cgpt ask` timed out, or
you started one yourself in the page), the ask is refused before anything is
typed, because the reply would be that old answer, not yours. A tab whose
answer is just ending is given up to 15 seconds (never past the time the ask needs
to send) before it counts as busy. The
refusal exits with `8` (host error `tab_busy`) and says "The previous answer is
still generating in this tab". Wait for the answer to end (or press Stop in the
tab) and run the ask again. A second `cgpt ask` while another one still runs
in the same tab is not refused: it waits for the first one to end and is then
typed and sent into the same chat (within its own `--timeout-ms`; if that runs
out first, it exits with `5`, "timed out waiting for the previous request in
the same ChatGPT tab; nothing was sent"). Only when the extension lost track
of the running ask (its service worker restarted mid-ask) is the second one
refused, with exit `8` and the message "another ask is running in this tab".
An extension built before the `tab_busy` code, or a current one
behind an older native host, reports a busy tab as a broken ChatGPT page: exit
`6`, with the same message. Before this change `cgpt ask` could type into a
running answer and print that answer's text as its own result.

Changed codes (scripts that test them need an update):

- `cgpt agent` with no task: `1` → `2`.
- `cgpt agent`, host `bad_request`: `1` → `2`.
- `cgpt agent`, host `internal` error: `1` → `11`.
- `cgpt ask --buffer`, clipboard read failure: `2` → `1`.
- `cgpt ask` / `cgpt agent`, an ask stopped on purpose (Stop pressed in the
  ChatGPT page, or ended by a `stop` request): `11` → `10` (host
  `cancelled`). An extension older than that code, or a new extension behind
  an older native host, still reports it as `internal`: `11`.
- `cgpt ask` / `cgpt agent`, a busy ChatGPT tab: `6` → `8` (host
  `tab_busy`). An extension older than that code, or a new extension behind
  an older native host, still reports it as `dom_failure`: `6`.

---

## Safety model in one paragraph

`cgpt agent` only executes commands that arrive inside a single, validated `cgpt-agent-response-v1` JSON block. Prose, markdown, and inline code are never executed. Every proposed command is shown to you with the resolved `cwd`, the exact command string, ChatGPT's declared risk, the CLI's locally classified risk, and any warnings — and you must press `r` (run), `e` (edit), `s` (skip), or `q` (quit) on your local terminal (unless you pass `--auto-readonly` or `--yolo`, `docs/security.md` §3). `--auto-readonly` runs without a keypress only commands the classifier calls read-only that also pass a narrow list: `ls`, `cat`, `head`, `tail`, `wc`, `stat`, `file`, `grep`, `rg`, `find`, `tree`, `sort`, `echo`, `pwd` and `which`, each with a whitelist of flags, run from a program outside the project, with every named path inside it and no secrets file named. Every `git` command, version queries, `ps`, `more`, PowerShell names and flags that follow links, read paths from a file, write, run programs or open hidden files for `rg` always ask. Use it only on repositories you trust. Dangerous patterns (e.g. `sudo`, `rm -rf /`, `curl … | sh`, credential extraction) are hard-blocked locally regardless of what ChatGPT claims. Command output is run through a pattern-based secret-redaction pipeline before being sent back to ChatGPT. Both the denylist and the redaction are heuristics, not a sandbox. Through the OpenCode gateway there is no keypress: OpenCode runs the tool calls, by default without asking, and the gateway applies the same denylist to their `command` strings and redacts everything it sends to ChatGPT. See `docs/security.md` for the full list and the known gaps.

---

## Local plan files

When you run `cgpt agent`, the CLI maintains a local project folder:

- `.cgpt-bridge/plan.jsonl` — append-only event log. The source of truth. One JSON event per line. The task text in it (and in `plan.md`, `session.json` and `runs/<id>/`) is the redacted text unless you passed `--no-redact`.
- `.cgpt-bridge/plan.md` — human-readable plan, regenerated from `plan.jsonl`.
- `.cgpt-bridge/session.json` — metadata of the latest session.
- `.cgpt-bridge/runs/<session-id>/` — per-run transcript and command records (redacted), and `session.json` with that session's own status and chat address (what `--resume <id>` reads).
- `.cgpt-bridge/logs/` — an empty folder, reserved for CLI/host logs. **Not implemented:** nothing writes log files there and nothing is rotated. The gateway writes `gateway.log` in its install folder (see "Install"); the host logs to its own stderr.
- `.cgpt-bridge/.gitignore` — contains `*`, so git ignores the whole folder. It is written whenever it is missing (when a session starts or is resumed) and never overwritten if you already have one. Files git already tracks stay tracked: run `git rm --cached -r .cgpt-bridge` to untrack them. The `command` text in `plan.jsonl`, `plan.md` and the command records goes through the same secret redaction as command output; the command that runs is not changed.

Hand edits to `plan.md` are not preserved (it is regenerated). Edit `plan.jsonl` if you need to fix history.

---

## Repository structure

```
cgpt-bridge/
  Cargo.toml            # Rust workspace: cli, host, protocol, core, gateway
  Cargo.lock
  README.md
  cli/                  # `cgpt` terminal CLI: ask, agent, plan, runner, transport
  host/                 # Native Messaging host / local bridge process
  protocol/             # shared message types, framing, local socket, agent protocol parser
  core/                 # denylist and secret redaction, shared by the CLI and the gateway
  gateway/              # `cgpt-gateway`: OpenAI-compatible server for OpenCode
  extension/            # Chrome MV3 extension: background, content script, DOM adapter
  install/              # installers: macOS and Windows (whole set), Linux (host only)
  scripts/              # release bundles and installer checks
  docs/
    requirements.md     # what v0.1 must do
    protocol.md         # the cgpt-agent-response-v1 / cgpt-command-result-v1 wire formats
    architecture.md     # components, message flows, why Native Messaging
    security.md         # threat model, confirmation flow, denylist, redaction
    roadmap.md          # staged delivery from M1 through M11
```

---

## Supported platforms (v0.1)

- OpenCode gateway: Windows and macOS (built and installer-checked in CI on `windows-latest` and `macos-latest`). WSL: see "OpenCode in WSL".
- Native host and extension: Windows, macOS and Linux.
- `cgpt` CLI: macOS and Linux (the Linux bundle holds the CLI, the native host and the extension, built, tested and host-installer-checked in CI on `ubuntu-latest`; only the gateway is limited to Windows and macOS, with no Linux installer: the Linux bundle also holds a built `bin/cgpt-gateway` that is unsupported there). Prebuilt macOS bundles are Apple silicon (`arm64`); on an Intel Mac build from source, no Intel build is checked in CI.
- Google Chrome (stable channel) on those platforms.
- Single host URL: `https://chatgpt.com/*`.

Other Chromium browsers and Firefox are out of scope for v0.1 (see `docs/roadmap.md`).

---

## Current implementation status

- [x] Stage 1 documentation in `docs/`
- [x] Rust workspace skeleton and shared protocol crate
- [x] `cgpt ask` CLI path with stdin handling and categorized errors
- [x] `cgpt agent` protocol parser, plan updates, command proposal loop, runner integration
- [x] Chrome extension source layout and content-script request handling
- [x] macOS and Linux native-host install scripts
- [x] Release bundler (`scripts/build-release.sh`) and macOS `.dmg` builder (`scripts/build-dmg.sh`)
- [x] Pinned extension id via the manifest `key` field — installs no longer require copying the id from `chrome://extensions`
- [x] In-process markdown rendering of final agent message (`termimad` + `syntect` syntax-highlighted fenced blocks)
- [x] Convenience flags: `--buffer`, `--editor`, `--copy`, `--no-pretty`
- [x] Session continuity: `--continue`/`-c`, `--resume <id>`
- [x] Offline subcommands: `cgpt history`, `cgpt replay`, `cgpt last`
- [ ] `cgpt doctor` (M10) — see `plans/roadmap-remaining.md`
- [x] OpenCode gateway (`cgpt-gateway`) with installers for Windows and macOS
- [ ] Full end-to-end v0.1 hardening across CLI, gateway, host, extension and ChatGPT

---

## License

[The Unlicense](LICENSE) — public domain dedication. Do whatever you want
with this code: copy, modify, sell, sublicense, vendor it, fork it, ship it.
No attribution required, no warranty, no strings. SPDX identifier:
`Unlicense`.
