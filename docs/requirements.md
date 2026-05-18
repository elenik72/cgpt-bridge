# cgpt-bridge — Requirements (v0.1)

Status: design stage. This document defines the product requirements for the first usable release (v0.1) of `cgpt-bridge`. It is binding for Stage 1 implementation planning. Anything not described here is out of scope for v0.1.

---

## 1. Product overview

`cgpt-bridge` is a local developer tool that connects a terminal to the user's already-open ChatGPT web session in Google Chrome. It is invoked through a single user-facing CLI named `cgpt`.

The system has two halves:

- A **local Rust side**: the `cgpt` CLI and a Chrome Native Messaging host.
- A **browser side**: a Chrome extension (Manifest V3, TypeScript) with a content script running on `https://chatgpt.com/*`.

These two halves communicate through Chrome Native Messaging. There is no localhost HTTP or WebSocket server in v0.1.

The core product principle is:

> **ChatGPT proposes. The CLI validates and shows. The user approves. The CLI runs. The result is returned to ChatGPT. The plan is updated locally.**

`cgpt-bridge` is for personal, user-initiated, interactive use against the user's own active ChatGPT tab. For programmatic, batch, server-side, or production integrations, users should prefer official APIs.

This tool is not affiliated with, endorsed by, or approved by any vendor.

---

## 2. Goals

1. Let a developer send prompts from a terminal to their existing ChatGPT tab without copy-paste, while keeping the conversation visible in the browser.
2. Support an interactive agent loop where ChatGPT can propose **one shell command at a time**, which the user reviews and approves before execution.
3. Persist a local, human-readable plan (`plan.md`) and a structured event log (`plan.jsonl`) for the active task.
4. Be safe by default: no command runs without explicit user confirmation.
5. Be diagnosable: `cgpt doctor` should pinpoint setup issues.

## 3. Non-goals

`cgpt-bridge` is explicitly **not**:

- A scraper, crawler, or chat history exporter.
- A batch automation framework.
- A way to bypass login, CAPTCHA, rate limits, paywalls, or any ChatGPT protection.
- A background monitor of the ChatGPT page.
- A scheduled or unattended job runner.
- A multi-tab orchestrator.
- A parallel command executor.
- A replacement for the official OpenAI API for programmatic workloads.

Specifically excluded for v0.1:

- No reading or exporting old conversations.
- No browsing chat history.
- No background polling of the page.
- No autonomous / silent / unattended command execution.
- No parallel commands.
- No remote control of `cgpt` from outside the local machine.

---

## 4. Supported platforms (v0.1)

- macOS (Apple Silicon and Intel) and Linux (x86_64) for the local side.
- Google Chrome (stable channel) as the only supported browser.
- Chrome Extension Manifest V3.
- TypeScript for the extension.
- Rust (stable) for the CLI and Native Messaging host.
- A single host URL: `https://chatgpt.com/*`.

Windows, Brave, Edge, Chromium forks, and Firefox are out of scope for v0.1 but should not be architecturally precluded.

---

## 5. User-facing commands

The CLI is named `cgpt`. v0.1 ships these subcommands:

- `cgpt ask` — single-shot prompt/response.
- `cgpt agent` — interactive agent loop with confirmed command execution.
- `cgpt doctor` — health and connectivity diagnostics.
- `cgpt history` — list past agent sessions from `.cgpt-bridge/runs/`.
- `cgpt replay <session-id>` — re-render the final markdown of a stored
  session without contacting ChatGPT.
- `cgpt last` — shortcut for `cgpt replay <most-recent-session>`.

Common flags (all subcommands):

- `--timeout <seconds>` — overall request timeout (per round-trip to ChatGPT).
- `--log-level <level>` — `error|warn|info|debug|trace`. Default `warn`.
- `--no-color` — disable ANSI colors on stderr.
- `--json` — machine-readable output on stdout where applicable (post-v0.1 may extend).
- `--buffer` — for subcommands that take a prompt/task, read it from the OS
  clipboard instead of arg/stdin. Applies to `cgpt ask` and `cgpt agent`.
  Implementation shells out to `pbpaste` on macOS and tries
  `wl-paste` → `xclip` → `xsel` on Linux. When `--buffer` is set, stdin is
  not consumed even if piped; positional args, if any, are prepended to the
  clipboard contents as `<args>\n\n<clipboard>`.
- `--editor` — open `$EDITOR` (fallback `vi`, then `nano`) on a tmpfile
  pre-populated with whatever the combination of positional args / stdin /
  `--buffer` would have produced. The user's saved buffer becomes the final
  prompt/task. Applies to `cgpt ask` and `cgpt agent`.
- `--copy` — after printing the final assistant message (or the response in
  `cgpt ask` / the rendered final in `cgpt replay`, `cgpt last`), copy the
  same text to the OS clipboard via `pbcopy` / `wl-copy` / `xclip` /
  `xsel`.

### 5.1 `cgpt ask`

Purpose: send one prompt to the active ChatGPT tab and print the visible assistant response.

Behavior:

- Reads the prompt from positional arguments and/or stdin. If both are present, the two are concatenated as: `<arg text>\n\n<stdin>`.
- If neither is present, exits with a clear usage error to stderr.
- Sends the prompt to the active ChatGPT tab via the extension.
- Waits for the assistant's response to stabilize.
- Prints the assistant response to **stdout**.
- Prints progress, warnings, and errors to **stderr** only.
- Exits non-zero on failure (tab unavailable, composer not found, timeout, native host error, etc.).

Pipe support example:

```sh
cargo test 2>&1 | cgpt ask "explain the failure"
```

Constraints:

- `cgpt ask` does **not** parse the response.
- `cgpt ask` does **not** execute any commands.
- `cgpt ask` does **not** modify any local files except its own logs.

### 5.2 `cgpt agent`

Purpose: hand a task to ChatGPT and let it propose shell commands one at a time, with the user approving each before execution.

Behavior:

- Reads the task from arguments and/or stdin.
- Prepends the **agent prompt contract** (see `protocol.md`) to the task on the first turn.
- Sends the full prompt to the active ChatGPT tab.
- Expects exactly one `cgpt-agent-response-v1` fenced JSON block in the assistant's response.
- Parses the block. On invalid/missing/duplicate blocks, optionally sends a single **repair prompt** and retries.
- Applies `plan_update` events to `.cgpt-bridge/plan.jsonl` and regenerates `.cgpt-bridge/plan.md`.
- Prints `user_message` to stdout.
- If `command` is non-null:
  - Renders a confirmation panel on stderr showing: `cwd`, the command string, declared `risk`, locally classified risk, declared `expected_effect`, declared `timeout_ms`, declared `send_output`, and any warnings.
  - Prompts the user: `[r]un / [e]dit / [s]kip / [q]uit`.
  - If `run`, executes the command in shell mode (see §8), capturing stdout, stderr, exit code, duration, and timeout status.
  - Redacts likely secrets and truncates output per `send_output` (see `protocol.md` and `security.md`).
  - Sends a `cgpt-command-result-v1` block back to ChatGPT in the next turn.
- If `command` is null and `status == final`, prints the final message and exits 0. When stdout is a TTY, the final `user_message` is rendered as pretty markdown via the built-in `termimad` renderer (headers, lists, tables, inline emphasis). Fenced code blocks inside the final message are passed through `syntect` so the code is syntax-highlighted (the info string after the opening fence — e.g. ```` ```rust ```` — chooses the syntax; unknown/missing falls back to plain). When stdout is piped or redirected, raw markdown is emitted so downstream consumers get clean text. `--no-pretty` forces the raw path unconditionally.
- The final `user_message` is additionally archived to
  `.cgpt-bridge/runs/<session-id>/final.md`, and each session writes a
  `meta.json` describing the task and start time, so
  `cgpt history` / `cgpt replay` / `cgpt last` can enumerate and re-render
  past sessions without contacting ChatGPT.
- `--continue` / `-c` reuses the most recent session's id, skips the
  prompt-contract preamble (ChatGPT already has it in the tab's context),
  and appends new turns to the same `plan.jsonl` and `runs/<id>/` dir.
  `--resume <session-id>` is the explicit-id variant; the two are mutually
  exclusive.
- Loops until: `status == final`, user quits, repair fails, policy block, timeout, tab unavailable, or fatal error.

There is **no fixed maximum step count** in interactive mode. Each iteration still requires explicit user confirmation, so the user is the rate limit.

### 5.3 `cgpt doctor`

Purpose: verify the installation is healthy.

Checks (each prints PASS/FAIL with remediation hints):

1. `cgpt` binary version and build target.
2. Native host manifest file present at the OS-correct location (per Chrome docs) and well-formed JSON.
3. Native host manifest `path` points to an existing, executable Rust host binary.
4. `allowed_origins` in the manifest contains exactly the expected extension origin.
5. Extension is installed and enabled (probed via a native message round-trip).
6. Native Messaging round-trip (`ping` → `pong`) succeeds.
7. At least one Chrome tab matches `https://chatgpt.com/*` and is the active tab in its window.
8. Content script answers an `isReady` probe in the active ChatGPT tab.
9. The composer element can be located by the DOM adapter.
10. Project directory `.cgpt-bridge/` is writable (or can be created).

Exit code: 0 if all PASS, 1 if any FAIL.

---

## 6. Plan storage

`cgpt agent` writes to a project-local directory:

```
.cgpt-bridge/
  plan.jsonl       # append-only event log (source of truth)
  plan.md          # human-readable, regenerated from plan.jsonl
  session.json     # current session metadata (id, started_at, last turn, model hints if any)
  runs/            # per-run artifacts (one subdir per agent session)
    <session-id>/
      transcript.jsonl        # full message-level transcript
      command-<id>.json       # per-command record (request + result)
  logs/            # rotating CLI/host logs (text)
```

Rules:

- `plan.jsonl` is the source of truth. Each line is one `PlanEvent` (see `protocol.md`).
- `plan.md` is **derived** and may be regenerated at any time from `plan.jsonl`. Hand edits to `plan.md` are not preserved.
- `session.json` contains: `session_id`, `started_at`, `cwd`, `last_turn_at`, `status`, optional `task_summary`. It must not contain command output.
- `runs/<session-id>/transcript.jsonl` stores prompts, raw assistant responses, parsed blocks, command requests, and (redacted) command results.
- `logs/` holds rotating diagnostic logs from the CLI and the native host (host logs come via stderr capture).
- Full **raw** command output is not persisted by default (see §8). If a command itself writes a log file (e.g. `tee`), that file is the user's responsibility.

---

## 7. Command execution behavior

Shell commands are first-class. Pipes, redirects, conditionals, subshells, environment variables, and normal shell syntax must work — but only after user confirmation.

Requirements:

- Commands are executed only after explicit user confirmation per command.
- Execution mode (macOS/Linux):
  - Primary: `"$SHELL" -lc '<command>'`.
  - Fallback if `$SHELL` is unset or fails to launch: `/bin/sh -lc '<command>'`.
  - `-l` (login shell) ensures user `PATH` and standard env are loaded.
- Windows execution is documented as future work, not in v0.1.
- The working directory is the current project root by default, or a subdirectory if the `command.cwd` field (after validation) resolves to one. Paths outside the project root are rejected (see `security.md`).
- Every command has a timeout. Default 60s; ChatGPT may request a larger value via `timeout_ms`, subject to a configured hard cap (default cap: 600000 ms / 10 min). Requests above the cap are clamped to the cap and a warning is shown.
- The CLI captures stdout, stderr, exit code, wall-clock duration, and a `timed_out` flag.
- Output sent back to ChatGPT is redacted and truncated per `send_output` and per `security.md`.
- Raw, unredacted output is not persisted by default. The redacted version goes into `runs/<session-id>/command-<id>.json` along with the request and metadata.
- Commands run sequentially. Never in parallel.
- No backgrounding by default. If a user types a `&`-suffixed command, the CLI shows a warning before run.

---

## 8. Agent loop behavior

- No fixed maximum number of steps in interactive mode.
- Each turn is one prompt to ChatGPT, one parsed response, optionally one approved command, optionally one result returned to ChatGPT.
- The loop exits on any of:
  - `status == final`.
  - User selects `q` (quit) at the confirmation prompt.
  - User refuses a command and ChatGPT yields no further proposal (i.e. ChatGPT's next response has `command == null` and `status == final`).
  - Protocol violation that cannot be repaired in one attempt.
  - Locally blocked command (policy denylist) where ChatGPT does not produce an acceptable alternative.
  - Command timeout if the user marks the loop terminal on timeout (default: not terminal; result is returned with `timed_out: true` and the loop continues).
  - Tab becomes unavailable.
  - Ctrl+C / SIGINT.

The CLI must support clean interruption: Ctrl+C cancels the in-flight wait or command, prints a status line, and exits non-zero without corrupting `plan.jsonl`.

---

## 9. Error handling requirements

- All user-visible errors go to **stderr**. Stdout is reserved for assistant content (`ask`) and for `user_message` text (`agent`).
- Errors are categorized and include a short remediation hint where possible. Categories: `setup`, `transport`, `tab`, `dom`, `protocol`, `policy`, `command`, `internal`.
- Exit codes:
  - `0` success.
  - `1` generic failure.
  - `2` usage / argument error.
  - `3` setup error (native host, extension, manifest).
  - `4` transport / native messaging error.
  - `5` tab unavailable / wrong URL / not active.
  - `6` DOM adapter failure (composer not found, response not detected).
  - `7` protocol violation that could not be repaired.
  - `8` policy block (denylist).
  - `9` command failed (non-zero exit).
  - `10` user cancelled.
- Partial state must be safe: a crash mid-loop must leave `plan.jsonl` valid (append-only, newline-terminated JSON).

---

## 10. Privacy requirements

- No network calls from the CLI or native host other than through the user's Chrome to the user's open ChatGPT tab. No telemetry, no analytics, no crash reporting.
- The extension makes no network calls beyond what is required by the active ChatGPT page itself.
- No remote code loading in the extension. No `eval`. No third-party scripts.
- No background scraping of chat history.
- Logs default to local-only and exclude command output. Users opt in for verbose logging.

---

## 11. Security requirements

Full detail in `security.md`. Summary:

- ChatGPT output is **untrusted**. Treat every fenced JSON block as adversarial input until parsed and validated locally.
- Every command requires explicit user confirmation. Confirmation cannot be auto-accepted by config in v0.1.
- A local denylist blocks or warns on dangerous patterns (sudo, recursive root deletion, credential extraction, `curl|sh`, etc.).
- Secrets are redacted from command output before it is sent back to ChatGPT.
- Native Messaging manifest must use `allowed_origins` with a single specific extension id, no wildcards.
- Extension permissions are minimal; host permission is exactly `https://chatgpt.com/*`.
- No system clipboard is used for prompt insertion.

---

## 12. Acceptance criteria

### 12.1 Stage 1 (documentation) acceptance

Stage 1 is complete when:

- `docs/requirements.md`, `docs/protocol.md`, `docs/architecture.md`, `docs/security.md`, `docs/roadmap.md`, and `README.md` exist.
- The protocol document defines the full `cgpt-agent-response-v1` and `cgpt-command-result-v1` schemas with at least one example each.
- The agent prompt contract is included verbatim in `protocol.md`.
- The security document enumerates the v0.1 denylist and redaction patterns.
- The architecture document includes an ASCII system diagram and the message flows for `ask`, `agent`, and command-result feedback.
- The roadmap lists milestones M1–M11 and marks future items as out of v0.1.

### 12.2 v0.1 product acceptance

v0.1 is complete when **all** of the following hold:

1. On macOS and Linux, with a clean install per the documented install script, `cgpt doctor` reports all checks PASS.
2. `cgpt ask "say hello"` inserts the prompt into the user's active ChatGPT tab without using the system clipboard, and prints the assistant's visible response to stdout.
3. `cargo test 2>&1 | cgpt ask "explain"` works as a single shell pipeline.
4. `cgpt agent "diagnose failing tests"` performs at least one full loop iteration: receives a valid `cgpt-agent-response-v1`, shows a proposed command, runs it after confirmation, sends back a `cgpt-command-result-v1`, and receives a follow-up turn.
5. A `cgpt-agent-response-v1` block that is missing, malformed, has an unknown version, or appears more than once causes the CLI to either send exactly one repair prompt or abort with exit code 7. In no case does it execute a command from a malformed block.
6. A command on the v0.1 denylist (e.g. `sudo rm -rf /`) is hard-blocked locally regardless of `risk` value declared by ChatGPT.
7. Command output sent back to ChatGPT has been passed through the redaction pipeline; raw secrets detected by the v0.1 patterns do not appear in the outgoing block.
8. `.cgpt-bridge/plan.jsonl` is append-only and parseable line-by-line after any normal termination, Ctrl+C, or single-command crash.
9. The Chrome extension runs only on `https://chatgpt.com/*` and has no other host permissions.
10. No localhost HTTP or WebSocket port is opened by the CLI, native host, or extension.
