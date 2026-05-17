# cgpt-bridge — Architecture (v0.1)

This document describes the v0.1 architecture of `cgpt-bridge`: components, responsibilities, message flows, and the reasons for the load-bearing design decisions.

---

## 1. High-level diagram

```
            ┌──────────────────────────────────────────────────────────────┐
            │                       Local machine                          │
            │                                                              │
 user ───►  │  Terminal / scripts                                          │
            │      │                                                       │
            │      ▼                                                       │
            │   ┌────────┐    stdin    ┌────────────────────────────────┐  │
            │   │  cgpt  │◄──────────  │ shell pipelines, e.g.          │  │
            │   │  CLI   │             │ `cargo test 2>&1 | cgpt ask …` │  │
            │   │ (Rust) │────────────►│                                │  │
            │   └────┬───┘    stdout    └────────────────────────────────┘ │
            │        │ length-prefixed JSON (stdio pipe)                   │
            │        ▼                                                     │
            │   ┌─────────────────────┐                                    │
            │   │  Native Messaging   │   stderr only for logs             │
            │   │  Host  (Rust bin)   │                                    │
            │   └────────┬────────────┘                                    │
            │            │ Chrome Native Messaging                         │
            │            │ (length-prefixed JSON over stdio,               │
            │            │  Chrome ⇄ host binary)                          │
            │            ▼                                                 │
            │   ┌─────────────────────┐                                    │
            │   │ Chrome Extension    │                                    │
            │   │ service worker (MV3)│                                    │
            │   │ TypeScript          │                                    │
            │   └────────┬────────────┘                                    │
            │            │ chrome.tabs.sendMessage                         │
            │            ▼                                                 │
            │   ┌─────────────────────┐                                    │
            │   │ Content script      │                                    │
            │   │ on chatgpt.com/*    │                                    │
            │   │ uses chatgptAdapter │                                    │
            │   └────────┬────────────┘                                    │
            │            │ DOM read/write                                  │
            │            ▼                                                 │
            │   ┌─────────────────────┐                                    │
            │   │ Active ChatGPT tab  │                                    │
            │   │ (web page DOM)      │                                    │
            │   └─────────────────────┘                                    │
            │                                                              │
            └──────────────────────────────────────────────────────────────┘
```

There is one process tree on the local side (`cgpt` spawns or talks to a native host that Chrome itself launches), and the browser side (extension + content script) lives entirely inside Chrome. Nothing listens on a network port.

---

## 2. Components

### 2.1 Rust `cgpt` CLI

Responsibilities:

- Parse CLI arguments and subcommand (`ask`, `agent`, `doctor`).
- Read stdin when piped.
- Build the request payload (prompt text, agent contract on first agent turn, command-result block on subsequent agent turns).
- Open a bidirectional connection to the native messaging host (see §2.2).
- For `agent`, drive the loop: parse `cgpt-agent-response-v1`, validate, apply plan updates, render confirmation UI, run commands, redact and truncate output, send `cgpt-command-result-v1`.
- Update `.cgpt-bridge/plan.jsonl`, regenerate `.cgpt-bridge/plan.md`, and write `runs/<session-id>/` artifacts.
- Print the assistant message to stdout. Print everything else to stderr.
- Translate every failure into a categorized error and exit code (per `requirements.md` §9).

The CLI is the only component that touches the local filesystem for plan/run/log state.

### 2.2 Rust Native Messaging Host

Responsibilities:

- Speak Chrome's Native Messaging protocol on its stdio: 4-byte little-endian length prefix + UTF-8 JSON body, in both directions.
- Bridge messages between the CLI and the Chrome extension.
- Validate inbound message sizes against Chrome's limits (1 MB request-side cap from Chrome; the host should also enforce its own response size cap).
- Log only to stderr. **Never** write anything to stdout (stdout is the wire to Chrome — any stray write corrupts the protocol).
- On JSON parse errors or oversize messages, send a structured error JSON back, never crash.
- Be a thin pipe: protocol logic (agent loop, plan, commands) lives in the CLI, not here.

The CLI talks to the host either (a) by being launched by Chrome through Native Messaging on demand for a single request, or (b) through a small local IPC (e.g. a Unix domain socket under `$XDG_RUNTIME_DIR` or `$TMPDIR`) that the CLI uses to talk to the host that Chrome launched. The exact transport between `cgpt` and the host is an internal implementation detail decided in M4 of the roadmap; what is fixed in v0.1 is that there is **no localhost TCP port**.

### 2.3 Chrome Extension service worker (MV3, TypeScript)

Responsibilities:

- Connect to the native host via `chrome.runtime.connectNative("com.cgpt_bridge.host")`.
- Receive bridge requests of the form:

  ```
  { "type": "insert_and_get_response" | "ping" | "is_ready" | "doctor_probe",
    "request_id": "...",
    "payload": { ... } }
  ```

- For an `insert_and_get_response` request:
  - Find the **active** tab in the current focused window matching `https://chatgpt.com/*`. If multiple tabs match, prefer the active one in the focused window; otherwise fail with a clear error.
  - Send the request to that tab's content script via `chrome.tabs.sendMessage`.
  - Forward the content script's response or error back to the native host.
- Maintain at most one in-flight content-script request per tab.
- Surface clear errors when no matching tab exists, when the tab is not active, or when the content script does not respond within a timeout.

The service worker does **no DOM work** itself. It is a router.

### 2.4 Content script

Responsibilities:

- Runs on `https://chatgpt.com/*`.
- Imports and uses `chatgptAdapter` for all page-specific behavior.
- On `insert_and_get_response`:
  1. `chatgptAdapter.isSupportedPage()` — verify the page is a chat UI we recognize.
  2. `chatgptAdapter.findComposer()` — locate the input element.
  3. `chatgptAdapter.setComposerText(text)` — set the value **without using the clipboard** and dispatch the input events needed for the framework to register the change.
  4. `chatgptAdapter.submit()` — submit (Enter or send button).
  5. `chatgptAdapter.waitForNewAnswer({ timeout })` — wait for a new assistant turn to appear and stabilize.
  6. `chatgptAdapter.getLastAssistantMessage()` — read the visible text of the new turn only.
- Returns `{ text, finishedAt }` on success or a structured error.
- The content script **does not** poll the page in the background. It does work only in response to a service-worker message. When idle, it consumes no CPU.

### 2.5 ChatGPT DOM adapter (`chatgptAdapter.ts`)

This is the most fragile part of the system because ChatGPT's DOM is owned by a third party and changes without notice. The adapter exists to localize that risk.

Responsibilities and surface:

```
chatgptAdapter:
  isSupportedPage(): boolean
  findComposer(): HTMLElement | null
  setComposerText(text: string): void
  submit(): void
  getLastAssistantMessage(): { text: string, nodeId: string } | null
  waitForNewAnswer(opts: { timeout: number, baselineNodeId?: string }):
    Promise<{ text: string, nodeId: string }>
  isGenerating(): boolean
```

Design rules:

- Selectors should be **semantic and stable** wherever possible: ARIA roles, `aria-label`, semantic HTML elements (`textarea`, `form`, `button[type=submit]`), `contenteditable` containers identified by role.
- This document does **not** pin selectors to specific CSS class names. Class names are assumed unstable.
- Stabilization detection (`waitForNewAnswer`) should use a combination of: appearance of a new assistant turn node, absence of a "stop generating" indicator, and a short quiescence window where the visible text stops changing.
- All ChatGPT-specific knowledge must live in this file. No other module may import ChatGPT selectors or assumptions.
- The adapter should be straightforward to repair when ChatGPT's UI changes: a single file edit and a quick manual smoke test.

### 2.6 Local state

Directory layout (per project that uses `cgpt agent`):

```
.cgpt-bridge/
  plan.jsonl
  plan.md
  session.json
  runs/<session-id>/
    transcript.jsonl
    command-<id>.json
  logs/
```

- **`plan.jsonl`** — append-only, one `PlanEvent` per line. Source of truth.
- **`plan.md`** — regenerated from `plan.jsonl` on each plan update. Markdown, human-readable.
- **`session.json`** — current session metadata: `session_id`, `started_at`, `cwd`, `last_turn_at`, `status`, `task_summary`. No command output.
- **`runs/<session-id>/transcript.jsonl`** — full message-level transcript for this run: prompts sent, raw assistant responses, parsed blocks, command requests, redacted results.
- **`runs/<session-id>/command-<id>.json`** — per-command record (request + result, redacted).
- **`logs/`** — rotating CLI/host log files (text). Excludes command output by default.

---

## 3. Message flows

### 3.1 `cgpt ask`

```
user> cgpt ask "explain this error"

  1. CLI reads args + stdin.
  2. CLI -> host:        { type: "insert_and_get_response", payload: { text } }
  3. host -> extension:  same JSON, length-prefixed.
  4. extension worker: locate active https://chatgpt.com/* tab.
                       chrome.tabs.sendMessage(tabId, request).
  5. content script:   chatgptAdapter.setComposerText(text); submit();
                       waitForNewAnswer({ timeout }); getLastAssistantMessage().
  6. content script -> extension: { ok: true, text, finishedAt }.
  7. extension -> host -> CLI:    same response, length-prefixed.
  8. CLI prints `text` to stdout. Logs/errors to stderr. Exits 0.
```

Failure points along the path each map to a categorized error and an exit code (see `requirements.md` §9): no matching tab (5), content script unresponsive (6), DOM adapter cannot find composer (6), native messaging transport error (4), timeout (5 or 6 depending on phase).

### 3.2 `cgpt agent`

```
user> cgpt agent "diagnose failing tests"

  Turn 1:
    1. CLI builds prompt = AGENT_PROMPT_CONTRACT + "\n\n" + user task.
    2. CLI -> ... -> ChatGPT tab (same path as `ask`).
    3. CLI receives full assistant response text.
    4. CLI parser: extract single `cgpt-agent-response-v1` block. Validate.
       - On failure: send ONE repair prompt, re-receive, re-validate.
       - On still-failure: exit 7.
    5. CLI applies plan_update to plan.jsonl, regenerates plan.md.
    6. CLI prints user_message to stdout.
    7. If command == null:
         - status == final  -> exit 0.
         - status == blocked / needs_user_input -> print and wait.
         - status == continue -> wait (user replies in browser).
    8. If command != null:
         - Local risk classification.
         - Render confirmation panel on stderr (cwd, command, declared
           risk, local risk, expected effect, timeout, send_output, any
           warnings).
         - Prompt: [r]un / [e]dit / [s]kip / [q]uit.
         - If run/edit: execute command (see §3.3).
         - If skip: build a result block with stderr="user_rejected".
         - If quit: exit 10.

  Turn N+1:
    1. CLI builds next message: short optional comment + the
       `cgpt-command-result-v1` block.
    2. CLI -> ... -> ChatGPT tab.
    3. Repeat from Turn 1 step 3.
```

### 3.3 Command-result feedback loop

```
Command execution:
  1. Resolve cwd against project root; reject if outside.
  2. Apply local denylist (security.md §6). If blocked: synthesize a
     "policy_blocked" result instead of running.
  3. Spawn child: "$SHELL" -lc '<command>'  (fallback /bin/sh -lc).
  4. Capture stdout/stderr concurrently. Track wall-clock duration.
  5. On timeout: SIGTERM, then SIGKILL after grace period. Mark
     timed_out = true. exit_code = null.
  6. On completion: collect exit_code, durations, raw output.
  7. Run redaction pipeline over stdout and stderr.
  8. Truncate per `send_output` policy and local caps.
  9. Persist to runs/<session-id>/command-<id>.json (redacted).
  10. Build `cgpt-command-result-v1` block. Send next turn.
```

---

## 4. Chrome Native Messaging overview

Chrome Native Messaging is a stdio-based mechanism that lets an extension talk to a locally installed binary. Chrome launches the binary on demand based on a JSON manifest the user installs in a well-known OS-specific location. Messages are exchanged on the binary's stdin/stdout as **4-byte little-endian length-prefixed UTF-8 JSON**.

Key facts that affect this architecture:

- **Single allowed origin**: the manifest's `allowed_origins` field can list one or more `chrome-extension://<id>/` origins. v0.1 lists **exactly one**: the `cgpt-bridge` extension. No wildcards.
- **Message size cap**: Chrome enforces a 1 MB cap on messages from the host to the extension. Larger payloads must be truncated by the CLI before they cross this boundary.
- **No network port**: Chrome talks to the host through pipes Chrome itself owns. Nothing on the machine listens on TCP.
- **Lifecycle**: Chrome spawns the host on `connectNative` and terminates it when the port disconnects. The host should be robust to short lifetimes.

### 4.1 Why Native Messaging over a localhost HTTP/WebSocket server

- **No open port.** A localhost server is reachable by anything on the loopback interface, including other browser tabs and any local process. Native Messaging is pipe-based; only Chrome's spawned child has the handle.
- **Origin pinning.** `allowed_origins` restricts the host to one specific extension id. A rogue local extension cannot connect to our host by mistake.
- **No CORS surface.** No browser-vs-server origin policy to misconfigure.
- **No need to manage a daemon.** Chrome owns the host's lifecycle.

A localhost socket may be added later for development-time tooling, but is not part of v0.1 production mode.

---

## 5. Why no clipboard

The system clipboard is global and observable. Using it to insert prompts would (a) wipe whatever the user had copied, (b) be visible to any other app that watches the clipboard, and (c) be a side channel where prompt text and secrets can leak.

The content script sets the composer text directly with DOM property writes and dispatches the input events the page framework needs to register the change (e.g. `input`, `change`). The adapter is responsible for getting this right per ChatGPT's current implementation.

---

## 6. Why the DOM adapter must be isolated

ChatGPT's DOM is owned by a third party. Class names, ARIA labels, and element shapes can change at any time. Concentrating that risk in `chatgptAdapter.ts` means:

- A UI break is one file to fix.
- The rest of the codebase (service worker, native host, CLI, protocol) is unaffected by UI changes.
- Repairs can be reviewed against a small diff with a focused smoke test.

Rules of thumb for the adapter:

- Prefer semantic selectors (role, aria-label, element type) to class names.
- Encapsulate stabilization logic; do not export raw DOM references.
- Provide one well-named function per page interaction. Avoid catch-all helpers.

---

## 7. Error boundaries

Where errors are caught and translated:

- **Content script** translates DOM errors into structured `{ ok: false, code, message }` responses. It never throws across the messaging boundary.
- **Service worker** translates tab-discovery and messaging errors into structured responses to the native host. It does not retry silently.
- **Native host** translates Chrome Native Messaging transport errors and oversize payloads into structured error JSON to the CLI. It never crashes; the worst outcome is a clean error response.
- **CLI** owns user-facing error rendering, exit codes, and the decision to repair (protocol) or abort.

---

## 8. Logging boundaries

- Stdout of `cgpt ask` is reserved for the assistant's visible message.
- Stdout of `cgpt agent` is reserved for `user_message` text plus final messages.
- All progress, warnings, and errors go to stderr.
- The native host logs only to its own stderr. Chrome captures host stderr to its native messaging error log when in developer mode.
- The extension logs go to the Chrome extension console (background and content script). The extension does not send logs to any network endpoint.
- File logs live under `.cgpt-bridge/logs/`. They are rotated by size, default cap 5 MB per file, 3 files retained.

---

## 9. Concurrency model

- One active `cgpt agent` session at a time per project directory. A lockfile at `.cgpt-bridge/.lock` prevents concurrent agent loops in the same project.
- Multiple `cgpt ask` invocations may run, but they all target the same active ChatGPT tab and will be serialized at the extension layer (one in-flight content-script request per tab).
- Commands run sequentially. Never in parallel.

---

## 10. Configuration

v0.1 configuration is intentionally minimal and lives at `~/.config/cgpt-bridge/config.toml` (XDG on Linux, `~/Library/Application Support/cgpt-bridge/config.toml` on macOS). Allowed keys:

- `default_timeout_ms` (integer, default 60000)
- `command_timeout_cap_ms` (integer, default 600000)
- `redaction_extra_patterns` (array of strings, regex)
- `denylist_extra` (array of strings, regex on the trimmed command)
- `allow_sudo` (boolean, default false)
- `log_level` (string)

Per-project overrides may live at `.cgpt-bridge/config.toml` (project takes precedence for non-security knobs; security knobs may only be loosened in user config, never in project config, to avoid a repo lowering its own bar).

---

## 11. What this architecture deliberately excludes

- No localhost HTTP or WebSocket server.
- No remote control of `cgpt`.
- No multi-tab orchestration.
- No background workers polling the ChatGPT DOM.
- No persistent connection from the CLI to ChatGPT — each turn is a discrete request.
- No automatic command execution path.
