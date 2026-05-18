# cgpt-bridge

A personal local bridge between your terminal and your already-open ChatGPT tab in Google Chrome.

**Status:** pre-MVP implementation in progress. The repository now contains the Rust workspace, Chrome extension sources, Native Messaging host, shared protocol crate, installer scripts, and Stage 1 documentation in `docs/`.

`cgpt-bridge` is **not** affiliated with, endorsed by, or approved by any vendor. It is a tool for personal, user-initiated, interactive use against your own active ChatGPT tab. For programmatic, batch, server-side, or production integrations, prefer official APIs.

---

## What it does

- Lets you send a prompt from your terminal to your existing ChatGPT tab without copy-paste, keeping the conversation visible in the browser.
- Lets ChatGPT propose **one shell command at a time** that the CLI shows to you and runs **only after you confirm** — useful for debugging sessions, repo exploration, and small diagnosis tasks.
- Keeps a local, human-readable plan (`.cgpt-bridge/plan.md`) and an append-only event log (`.cgpt-bridge/plan.jsonl`) for the task you are working on.

The core principle is:

> **ChatGPT proposes. The CLI validates and shows. The user approves. The CLI runs. The result is returned to ChatGPT. The plan is updated locally.**

---

## What it does not do

- It does **not** scrape, export, or browse your ChatGPT history.
- It does **not** run as a background monitor, daemon, scheduler, or batch job.
- It does **not** bypass login, CAPTCHA, rate limits, paywalls, or any ChatGPT protection.
- It does **not** auto-run commands proposed by ChatGPT by default. Two opt-in flags (`--auto-readonly`, `--yolo`) relax the keypress prompt for trusted contexts; denylist hits remain blocked regardless. See `docs/security.md` §3.
- It does **not** open a localhost HTTP or WebSocket port. Communication uses Chrome Native Messaging.
- It does **not** use the system clipboard.

See `docs/security.md` for the full safety model.

---

## Pieces

- **`cgpt` CLI** — Rust binary. The thing you run in your terminal.
- **Native Messaging host** — Rust binary. A thin bridge launched by Chrome on demand.
- **Chrome extension** — Manifest V3, TypeScript. Background service worker + content script.
- **Content script** — runs only on `https://chatgpt.com/*`. Inserts your prompt into the composer and reads the visible response.
- **ChatGPT DOM adapter** — `chatgptAdapter.ts`. All ChatGPT-specific selectors and behavior live here so that UI changes are localized to one file.

---

## Install

There are two install paths: **end-user** (just want to use the CLI on
another machine, using a prebuilt bundle) and **developer** (clone the
repo and build locally).

### End-user: install from a release bundle

The only third-party software you need on the target machine is **Google
Chrome** (or Chromium). All Rust / Node toolchains are baked into the
prebuilt bundle.

Optional runtime extras (no install blocker — `cgpt` falls back gracefully):

- **Clipboard reader** — only needed if you use `--buffer`. macOS ships
  `pbpaste` by default. On Linux install one of `wl-clipboard` (Wayland,
  provides `wl-paste`), `xclip`, or `xsel`.

Pretty markdown rendering of the final agent message is built in via
`termimad` — no external binary required.

Two artifact formats — pick one:

**Option A — `.dmg` (macOS, GUI-friendly)**

1. Copy `cgpt-bridge-<version>-macos-<arch>.dmg` to the target Mac, double-click to mount.
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
scp cgpt-bridge-<version>-<os>-<arch>.tar.gz othermachine:~
ssh othermachine
tar xzf cgpt-bridge-<version>-<os>-<arch>.tar.gz
cd cgpt-bridge-<version>-<os>-<arch>

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

Full per-platform notes ship inside the bundle as `INSTALL.md`.

### Developer: clone + build

Use this path if you intend to edit the source.

Prerequisites:

- **Rust** (stable). Install via [rustup](https://rustup.rs/).
- **Node.js** 20+ and **npm**.
- **Git**.
- **Google Chrome** (or Chromium).
- **openssl** — only if you regenerate the extension keypair. Ships with macOS by default; on Linux it is usually preinstalled or one `apt install openssl` away.

Build the everything bundle in one command:

```sh
git clone https://github.com/elenik72/cgpt-bridge.git
cd cgpt-bridge
./scripts/build-release.sh
```

That produces `dist/cgpt-bridge-<version>-<os>-<arch>/` plus a matching
`.tar.gz`. On macOS, `./scripts/build-dmg.sh` additionally wraps the
bundle in a `.dmg`.

For day-to-day development you usually want to skip the bundler and
symlink the dev binary so each `cargo build --release` is picked up
automatically:

```sh
cargo build --release -p cgpt-bridge-cli -p cgpt-bridge-host
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

`--buffer` also works for `cgpt ask`. When the final agent message arrives
and stdout is a TTY, it is rendered as pretty markdown via the built-in
`termimad` skin (headers, lists, tables, code blocks). When stdout is piped
or redirected, raw markdown is emitted automatically. Use `--no-pretty` to
force the raw path even on a TTY.

Check that your install is healthy:

```sh
cgpt doctor
```

---

## Safety model in one paragraph

`cgpt agent` only executes commands that arrive inside a single, validated `cgpt-agent-response-v1` JSON block. Prose, markdown, and inline code are never executed. Every proposed command is shown to you with the resolved `cwd`, the exact command string, ChatGPT's declared risk, the CLI's locally classified risk, and any warnings — and you must press `r` (run), `e` (edit), `s` (skip), or `q` (quit) on your local terminal. Dangerous patterns (e.g. `sudo`, `rm -rf /`, `curl … | sh`, credential extraction) are hard-blocked locally regardless of what ChatGPT claims. Command output is run through a secret-redaction pipeline before being sent back to ChatGPT. See `docs/security.md` for the full list.

---

## Local plan files

When you run `cgpt agent`, the CLI maintains a local project folder:

- `.cgpt-bridge/plan.jsonl` — append-only event log. The source of truth. One JSON event per line.
- `.cgpt-bridge/plan.md` — human-readable plan, regenerated from `plan.jsonl`.
- `.cgpt-bridge/session.json` — current session metadata.
- `.cgpt-bridge/runs/<session-id>/` — per-run transcript and command records (redacted).
- `.cgpt-bridge/logs/` — rotating CLI/host logs.

Hand edits to `plan.md` are not preserved (it is regenerated). Edit `plan.jsonl` if you need to fix history.

---

## Repository structure

```
cgpt-bridge/
  Cargo.toml            # Rust workspace: cli, host, protocol
  Cargo.lock
  README.md
  cli/                  # `cgpt` terminal CLI: ask, agent, plan, runner, transport
  host/                 # Native Messaging host / local bridge process
  protocol/             # shared message types, framing, agent protocol parser
  extension/            # Chrome MV3 extension: background, content script, DOM adapter
  install/              # platform install scripts for the native host
  docs/
    requirements.md     # what v0.1 must do
    protocol.md         # the cgpt-agent-response-v1 / cgpt-command-result-v1 wire formats
    architecture.md     # components, message flows, why Native Messaging
    security.md         # threat model, confirmation flow, denylist, redaction
    roadmap.md          # staged delivery from M1 through M11
```

---

## Supported platforms (v0.1)

- macOS and Linux for the local CLI and Native Messaging host.
- Google Chrome (stable channel) on those platforms.
- Single host URL: `https://chatgpt.com/*`.

Windows, other Chromium browsers, and Firefox are out of scope for v0.1 (see `docs/roadmap.md`).

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
- [ ] `cgpt doctor` (M10) — see `plans/roadmap-remaining.md`
- [ ] Full end-to-end v0.1 hardening across CLI, host, extension, and active ChatGPT tab

---

## License

To be decided before any code is published. Until a `LICENSE` file is added, no license is granted.
