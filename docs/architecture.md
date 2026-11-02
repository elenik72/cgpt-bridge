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

There is one process tree on the local side (`cgpt` spawns or talks to a native host that Chrome itself launches), and the browser side (extension + content script) lives entirely inside Chrome. The CLI and the native host listen on no network port.

A second client sits next to the CLI: **`cgpt-gateway`**, an OpenAI-compatible server for OpenCode. It reaches the native host over the same local socket as the CLI (a named pipe on Windows), applies the same denylist and redaction from `core/`, and is the only component that listens on a network port: `127.0.0.1` only, with a bearer token on every request (see `security.md` §15). Its asks run in the extension's own ChatGPT tab, in a separate window opened without focus, never in the user's tabs. Design: `docs/superpowers/specs/2026-10-26-opencode-gateway-design.md`.

---

## 2. Components

### 2.1 Rust `cgpt` CLI

Responsibilities:

- Parse CLI arguments and subcommand (`ask`, `agent`, `history`, `replay`, `last`; `doctor` is planned, **not implemented**).
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

The CLI does not launch the host: Chrome starts it when the extension connects, and the CLI (and the gateway) reach that running host through a small local IPC: a Unix domain socket under `$XDG_RUNTIME_DIR` or `$TMPDIR` on macOS and Linux, a named pipe (`\\.\pipe\cgpt-bridge-<user>`) on Windows, where only the gateway uses it (`protocol/src/local_socket.rs`). If the host is not running, the CLI exits with the setup error (3). There is **no localhost TCP port** for this link.

### 2.3 Chrome Extension service worker (MV3, TypeScript)

Responsibilities:

- Connect to the native host via `chrome.runtime.connectNative("com.cgpt_bridge.host")`.
- Receive bridge requests from the host (`protocol/src/message.rs`, `extension/src/bridge.ts`). Each is one JSON object with a `type` and an `id` that the answer echoes:

  ```
  { "type": "ask",    "id": "...", "text": "...", "timeout_ms": 120000,
    "target": { "kind": "conversation", "url": "..." } | { "kind": "new_chat" },   // target: gateway asks only
    "project": "..." }   // optional: the ChatGPT project a new chat goes into
  { "type": "stop",   "id": "..." }   // press Stop in the gateway's tab
  { "type": "stop_ask", "id": "...", "ask_id": "..." }   // cgpt ask on Ctrl+C: stop that one ask only, in its tab, while the tab shows its chat
  { "type": "status", "id": "..." }   // tab open? signed in?
  { "type": "ping",   "id": "...", "payload": ... }
  ```

  Answers are `ask_result`, `stopped` (to `stop_ask` with `other_chat: true` when the tab showed another chat and nothing was pressed, or `not_sent: true` when the ask was cancelled before its prompt was sent — while its tab was still being chosen, while it waited behind another ask, or before the page sent it), `status_result`, `pong` or `error` (with a machine-readable `code` and a `message`). `pong` carries the host's `build` and `status_result` the extension's (the commit's short SHA a release is built from, `dev` for a local build); the field is optional both ways, and the gateway logs the three builds at the first request, with a warning when they differ. The shared frames are pinned in `protocol/tests/fixtures/` (frames of older parts in `legacy/`).
  A turn stopped on purpose (Stop in the ChatGPT page, or a `stop` request) is the error code `cancelled`; the gateway answers it with HTTP 422 `cancelled_by_user`. To a host that sent no `build` in its `pong` (older than that code, it would reject the frame) the extension sends it the old way, `internal` with an exact message from `protocol/tests/fixtures/stop_messages.txt`, which the gateway still recognizes; that fallback goes once those parts are gone.
  `project` (optional) names the ChatGPT project a new chat goes into: the CLI sends the name of the repository root above the folder it runs in (else that folder), the gateway that of OpenCode's workspace root (else its working directory), none for the home folder or a disk root, and a warning instead for a name that cannot be one (`core/src/project.rs`). The extension compares it with the sidebar's labels in NFC and remembers the project's id per name in `chrome.storage.local`. It is not empty, at most 100 characters, without control characters or whitespace at either end; the host refuses another one as `bad_request`. Only a new chat is affected: the content script opens the project's page (`/g/<id>/project`) from the sidebar, creating the project when there is none by that name, and the chat's address becomes `/g/<id>-<slug>/c/<chat id>`, which the target checks accept for that project only. When the project can be neither found nor created, the chat is a plain new chat and `ask_result` carries a `warning` (the CLI prints it on stderr, the gateway logs it). A host older than the field drops it (it forwards only the fields it declares), and so the `warning`; an extension older than it ignores it: accepted, the parts ship in one bundle. A `#[serde(flatten)]` pass-through of unknown fields is not used.
  A tab refused as busy (an earlier answer still generating, or, after a service-worker restart lost the queue, another ask running in it; nothing was typed) is the error code `tab_busy`; the CLI exits with 8, and the gateway answers 503 `tab_busy`, which a client may resend unless an earlier part of the same request was sent (then it is a DOM failure, 502 `dom_failure`). To a host that sent no `build` the extension sends it the old way, `dom_failure` with the same message.

- For an `ask` request:
  - Without a `target` (the CLI), the first `https://chatgpt.com/*` tab of: the tab that answered the previous CLI ask, while it still shows the same chat; the active one in the last-focused window; the active one in any window; the only one, when exactly one is open. Several inactive ones and none active fail with a clear error, as does none. The own tab is never chosen, nor a recorded one whose mark is still in doubt. (CLI asks only. Gateway asks name a chat and go to the extension's own tab; see §1.)
  - Send the request to that tab's content script via `chrome.tabs.sendMessage`. A tab where no content script listens (open since before the extension was installed, updated or reloaded: Chrome injects the manifest's scripts only into pages loaded later) gets the page scripts injected with `chrome.scripting.executeScript` (waited for at most 5 s: a page still loading takes them only once loaded) and the same request once more. Not a `stop` (a script injected then knows of no ask to stop), not a tab that has left chatgpt.com, and not an ask that a `stop` cancelled meanwhile: that ask ends at once as `cancelled`, nothing sent. Only that failure is re-sent: nobody took the request. A page that took it and went away (`page_reloaded`) is never asked again, the prompt may already be in the chat. On install and update the service worker injects them into every open ChatGPT tab as well.
  - Forward the content script's response or error back to the native host.
- Maintain at most one in-flight content-script request per tab.
- Register each ask before its tab is chosen (finding or opening the tab can take seconds). A `stop` (for the own tab's asks) or a `stop_ask` naming the ask that comes while its tab is still being chosen, or while it waits behind another ask in the same tab, cancels it: it ends `cancelled` and nothing is sent to the tab (`stop_ask` is answered with `not_sent: true`).
- Surface clear errors when no matching tab exists, when the tab is not active, or when the content script does not respond within a timeout.

The service worker does **no DOM work** itself. It is a router.

### 2.4 Content script

Responsibilities:

- Runs on `https://chatgpt.com/*`.
- Imports and uses `chatgptAdapter` for all page-specific behavior.
- On an `ask` request:
  1. `chatgptAdapter.isSupportedPage()` — verify the page is a chat UI we recognize.
  2. `chatgptAdapter.findComposer()` — locate the input element.
  3. `chatgptAdapter.setComposerText(text)` — set the value **without using the clipboard** and dispatch the input events needed for the framework to register the change.
  4. `chatgptAdapter.submit()` — submit (Enter or send button).
  5. `chatgptAdapter.waitForNewAnswer({ timeout })` — wait for a new assistant turn to appear and stabilize.
  6. `chatgptAdapter.getLastAssistantMessage()` — read the visible text of the new turn only.
- Returns the answer text (and, for a gateway ask, the address of the chat it came from) on success, or a structured error with a code (`composer_not_found`, `answer_not_detected`, `target_mismatch`, `previous_answer_running`, and others; the service worker maps them to the host's `dom_failure`, `tab_unavailable`, `timeout`, `rate_limited`, `cancelled`, `tab_busy` or `bad_request`).
- Right before typing and right before every Send click it checks that the extension context is still alive (`chrome.runtime.id`). After an extension reload or update the old content script is cut off but keeps running next to a newly injected one; an ask it was driving then ends without typing or sending anything more, so two scripts never write into one composer.
- The content script **does not** poll the page in the background. It does work only in response to a service-worker message. When idle, it consumes no CPU.

### 2.5 ChatGPT DOM adapter (`chatgptAdapter.ts`)

This is the most fragile part of the system because ChatGPT's DOM is owned by a third party and changes without notice. The adapter exists to localize that risk.

Responsibilities and surface (the shape of the API, not an exact list: the real module has more functions, for example for chat addressing and Stop):

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
- All ChatGPT-specific DOM knowledge must live in this file. No other module may import ChatGPT selectors or assumptions. The exception is ChatGPT's answer stream (endpoint and event format), which `pageShim.ts` and `sseParser.ts` know.
- The adapter should be straightforward to repair when ChatGPT's UI changes: a single file edit and a quick manual smoke test.

### 2.6 Local state

Directory layout (per project that uses `cgpt agent`):

```
.cgpt-bridge/
  plan.jsonl
  plan.md
  session.json
  runs/<session-id>/
    session.json        # this session's own metadata (status, chat address)
    transcript.jsonl
    command-<id>.json
  logs/                 # created empty, nothing writes there (not implemented)
```

- **`plan.jsonl`** — append-only, one `PlanEvent` per line. Source of truth.
- **`plan.md`** — regenerated from `plan.jsonl` on each plan update. Markdown, human-readable.
- **`session.json`** — metadata of the latest session: `session_id`, `started_at`, `cwd`, `last_turn_at`, `status`, `task_summary`, optional `conversation_url`. No command output. Each session also keeps its own copy in `runs/<session-id>/session.json`; stopping or resuming an older session updates only its own copy, and the root file only while it still belongs to that session.
- **`runs/<session-id>/transcript.jsonl`** — one line per assistant reply of the run: `{turn, kind: "assistant_message", text_len, at_unix_ms}`. Lengths and times only: no prompt, reply or command text is kept there (commands and their redacted results are in `plan.jsonl` and `command-<id>.json`).
- **`runs/<session-id>/command-<id>.json`** — per-command record (request + result, redacted).
- **`logs/`** — created empty by the CLI. **Not implemented:** it is reserved for CLI/host text logs; nothing writes to it and nothing is rotated.

---

## 3. Message flows

### 3.1 `cgpt ask`

```
user> cgpt ask "explain this error"

  1. CLI reads args + stdin.
  2. CLI -> host:        { type: "ask", id, text, timeout_ms }   (local socket)
  3. host -> extension:  same JSON, length-prefixed (Native Messaging).
  4. extension worker: locate active https://chatgpt.com/* tab.
                       chrome.tabs.sendMessage(tabId, request).
  5. content script:   refuse if an answer is still generating in the tab;
                       chatgptAdapter.setComposerText(text); submit();
                       waitForNewAnswer({ timeout }); read the new turn.
  6. content script -> extension: { ok: true, text }.
  7. extension -> host -> CLI:    { type: "ask_result", id, text }.
  8. CLI prints `text` to stdout. Logs/errors to stderr. Exits 0.
```

Failure points along the path each map to a categorized error and an exit code (see `requirements.md` §9): no matching tab (5), content script unresponsive (5), DOM adapter cannot find composer (6), an answer still generating in the tab (8, `tab_busy`; 6 from an extension or native host built before that code, with the same message), native messaging transport error (4), host `timeout` (5).

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
         - status == blocked -> print a line, exit 1.
         - status == needs_user_input -> print a line, exit 0 (answer
           in the chat tab, then re-run `cgpt agent <follow-up>`).
         - status == continue -> print a line, exit 0 (nothing to run).
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
- **No network port**: Chrome talks to the host through pipes Chrome itself owns. The host listens on no TCP port (the OpenCode gateway's loopback port is a separate client, §1).
- **Lifecycle**: Chrome spawns the host on `connectNative` and terminates it when the port disconnects. The host should be robust to short lifetimes.

### 4.1 Why Native Messaging over a localhost HTTP/WebSocket server

- **No open port.** A localhost server is reachable by anything on the loopback interface, including other browser tabs and any local process. Native Messaging is pipe-based; only Chrome's spawned child has the handle. (That is why the OpenCode gateway, which needs an HTTP port because OpenCode speaks HTTP, takes a bearer token on every request.)
- **Origin pinning.** `allowed_origins` restricts the host to one specific extension id. A rogue local extension cannot connect to our host by mistake.
- **No CORS surface.** No browser-vs-server origin policy to misconfigure.
- **No need to manage a daemon.** Chrome owns the host's lifecycle.

The bridge itself (CLI, host, extension) still has no localhost server. The only one is the optional OpenCode gateway (§1).

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
- File logs under `.cgpt-bridge/logs/` (rotated by size, 5 MB per file, 3 files) are **not implemented**; the folder stays empty. The gateway appends to its own `gateway.log` in its install folder (no rotation on macOS; the Windows start script replaces it at each start).

---

## 9. Concurrency model

- One active `cgpt agent` session at a time per project directory is the intended rule. A lockfile at `.cgpt-bridge/.lock` is **not implemented**: nothing prevents two agent loops in the same project.
- Multiple `cgpt ask` invocations may run, but they all target the same active ChatGPT tab and will be serialized at the extension layer (one in-flight content-script request per tab): a second one waits for the first and is then sent into the same chat, it is not refused.
- Commands run sequentially. Never in parallel.

---

## 10. Configuration

**Not implemented.** The CLI reads no configuration file in v0.1, and none of the keys below exists in the code; this section is a design note only. The command timeout cap is fixed at 600000 ms, and the denylist and redaction patterns are built in (`docs/security.md` §6, §7). The gateway has its own `config.json` (`docs/security.md` §15).

The planned configuration (not implemented) would be minimal and live at `~/.config/cgpt-bridge/config.toml` (XDG on Linux, `~/Library/Application Support/cgpt-bridge/config.toml` on macOS). Planned keys:

- `default_timeout_ms` (integer, default 60000)
- `command_timeout_cap_ms` (integer, default 600000)
- `redaction_extra_patterns` (array of strings, regex; not implemented)
- `denylist_extra` (array of strings, regex on the trimmed command; not implemented)
- `allow_sudo` (boolean, default false; not implemented)
- `log_level` (string)

Per-project overrides (not implemented) would live at `.cgpt-bridge/config.toml` (project takes precedence for non-security knobs; security knobs may only be loosened in user config, never in project config, to avoid a repo lowering its own bar).

---

## 11. What this architecture deliberately excludes

- No localhost HTTP or WebSocket server in the CLI, the native host or the extension. The one listener is the optional OpenCode gateway: `127.0.0.1` only, bearer token required.
- No remote control of `cgpt`.
- No multi-tab orchestration.
- No background workers polling the ChatGPT DOM.
- No persistent connection from the CLI to ChatGPT — each turn is a discrete request.
- No automatic command execution path.
