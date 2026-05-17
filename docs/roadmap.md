# cgpt-bridge — Roadmap (v0.1)

Staged delivery plan. Each milestone has a clear definition of "done" and a small smoke test. Milestones are ordered to minimize integration risk: documentation and protocol first, then a minimal usable browser path, then the local side, then the safety machinery, then polish.

The current stage is **M1**. No application code has been written.

---

## M1 — Documentation and protocol

Scope:

- `docs/requirements.md`
- `docs/protocol.md`
- `docs/architecture.md`
- `docs/security.md`
- `docs/roadmap.md`
- `README.md`

Done when:

- All six files exist and are internally consistent.
- The protocol document defines `cgpt-agent-response-v1`, `cgpt-command-result-v1`, and the agent prompt contract.
- The security document enumerates the v0.1 denylist and redaction patterns.
- The architecture document includes an ASCII system diagram and message flows for `ask`, `agent`, and command-result feedback.

Smoke test: a reader unfamiliar with the project can read the docs end-to-end and explain, without reading code, what `cgpt agent` does on a successful loop and what happens on a malformed assistant block.

---

## M2 — Chrome extension MVP

Scope:

- Manifest V3 skeleton with `nativeMessaging` and `tabs` permissions and `host_permissions: ["https://chatgpt.com/*"]`.
- Static content script declared in the manifest for `https://chatgpt.com/*`.
- Background service worker.
- A hardcoded prompt-insertion path: when the service worker receives a fixed test message, the content script types a known string into the composer, submits, and returns the visible response text.
- No native host integration yet — the service worker simulates the request locally (e.g. via an extension command or the action popup).

Done when:

- A developer can load the unpacked extension, open `https://chatgpt.com`, trigger the test action, and see the prompt inserted and the response returned via `chrome.tabs.sendMessage`.
- The content script does no work when idle.
- No clipboard usage anywhere.

---

## M3 — ChatGPT DOM adapter

Scope:

- `chatgptAdapter.ts` with the surface defined in `architecture.md` §2.5.
- Stable semantic selectors where possible.
- Robust input event dispatch.
- `submit` via Enter key event and/or send-button click, whichever is needed for the current page.
- `waitForNewAnswer` stabilization logic (new turn appears, stop-indicator disappears, quiescence window).

Done when:

- The MVP from M2 works through the adapter, with no ChatGPT-specific code outside `chatgptAdapter.ts`.
- A short README inside the adapter file explains how to verify each function manually if the page changes.

---

## M4 — Native Messaging ping/pong

Scope:

- Rust native host binary.
- Length-prefixed JSON framing on stdin/stdout, with size cap.
- Logs only to stderr.
- Install script for the OS-specific manifest location with `allowed_origins: ["chrome-extension://<id>/"]`.
- Extension service worker calls `chrome.runtime.connectNative("com.cgpt_bridge.host")` and exchanges a `{type:"ping"}` → `{type:"pong"}` message.

Done when:

- A user runs the install script, loads the extension, and a ping round-trip succeeds.
- Killing the host mid-message produces a clean error in the service worker, not a hang.

---

## M5 — `cgpt ask`

Scope:

- Rust CLI binary with subcommand parsing.
- `cgpt ask` reads args + stdin, opens a connection through the native host to the extension, sends `insert_and_get_response`, prints the response to stdout, logs/errors to stderr.
- Per-request timeout flag.
- Clear stdout/stderr discipline.

Done when:

- `echo "hello" | cgpt ask "say"` works against the active ChatGPT tab.
- `cargo test 2>&1 | cgpt ask "explain"` works as a single shell pipeline.
- All categorized errors map to documented exit codes.

---

## M6 — Agent protocol parser

Scope:

- Rust parser for `cgpt-agent-response-v1` (exactly one fenced block, JSON, schema validation per `protocol.md` §5).
- Strict top-level / lenient nested unknown-fields policy.
- `timeout_ms` clamp.
- `cwd` containment check.
- Repair prompt logic (at most one retry).

Done when:

- Unit tests cover: valid block, missing block, two blocks, wrong version, missing required field, oversized timeout, escape from project root in `cwd`, unknown event type.
- A malformed block triggers exactly one repair prompt and, if the second response is still bad, exits with code 7.

---

## M7 — Plan storage

Scope:

- `.cgpt-bridge/plan.jsonl` append-only writer.
- `.cgpt-bridge/plan.md` regenerator from the JSONL log.
- `.cgpt-bridge/session.json` write/update on session start and on each turn.
- `.cgpt-bridge/runs/<session-id>/transcript.jsonl` and `command-<id>.json`.
- Rotating logs under `.cgpt-bridge/logs/`.

Done when:

- After an interrupted run, the JSONL log is still parseable line-by-line.
- `plan.md` is regenerated deterministically from `plan.jsonl`.

---

## M8 — Shell command runner

Scope:

- Confirmation UI on stderr per `security.md` §3.
- Local risk classifier and denylist per `security.md` §6.
- `"$SHELL" -lc '<command>'` execution with `/bin/sh -lc` fallback.
- Timeout with SIGTERM/SIGKILL escalation.
- stdout/stderr capture with per-stream caps.
- Redaction pipeline and `send_output` truncation per `security.md` §7–§8.

Done when:

- A simple `ls` proposal can be confirmed and run.
- A `sudo rm -rf /` proposal is hard-blocked regardless of declared `risk`.
- A command emitting a known fake API key has the key replaced before persistence and transmission.

---

## M9 — Command-result feedback loop

Scope:

- Build `cgpt-command-result-v1` block from the captured (redacted, truncated) result.
- Send the next ChatGPT message with that block as the body.
- Continue the loop until `status: final`, user quits, or a terminal error.
- Sequential, no parallelism. Opt-in `--auto-readonly` / `--yolo` flags
  bypass the keypress prompt; default remains interactive.

Done when:

- A complete `cgpt agent` run can go: propose → confirm → run → result → propose → final.
- Ctrl+C mid-loop exits with code 10 and leaves `plan.jsonl` valid.

---

## M10 — `cgpt doctor`

Scope:

- All checks in `requirements.md` §5.3.
- Each check prints PASS/FAIL and a remediation hint.
- Exits 0 if all pass, 1 otherwise.

Done when:

- A user with a broken install (missing manifest, wrong `allowed_origins`, no active ChatGPT tab, composer not findable, etc.) sees the right FAIL for the right reason.

---

## M11 — Packaging

Scope:

- macOS install script: builds the host, copies the binary, writes the Native Messaging manifest to the correct path, prints next steps.
- Linux install script: same, for the Linux manifest path.
- Documentation for loading the unpacked extension and capturing its extension id to wire into `allowed_origins`.
- Release build instructions (`cargo build --release`).
- Version stamping for `cgpt --version` and the host.

Done when:

- A clean macOS or Linux machine, with Chrome installed and a ChatGPT login, can go from `git clone` to a passing `cgpt doctor` in under ten minutes by following the README.

---

## Out of v0.1 (future work, not committed)

- **Windows support.** Native Messaging manifest paths and shell semantics differ; needs its own design pass.
- **Chrome Web Store packaging.** v0.1 is unpacked / developer mode only.
- **Brave / Edge / Chromium / Vivaldi support.** Similar to Chrome but with separate manifest paths and extension stores.
- **Firefox support.** Manifest V3 differences and Native Messaging differences.
- **Hardened auto-approve.** `--auto-readonly` and `--yolo` ship as opt-in
  flags in v0.1 (see `security.md` §3). Hardening the classifier so the
  default UX can lean on auto-approve is post-v0.1.
- **Sandboxed command execution.** `sandbox-exec` (macOS) or `bwrap`/user-namespace (Linux) per-command.
- **Patch application flow.** A `kind: "patch"` command type carrying a unified diff that the CLI applies after diff review, instead of free-form shell.
- **Structured file edits.** Beyond patch, a higher-level `apply_changes` request the CLI renders as a diff.
- **Voice / hotkey integration.** A global hotkey to send the selected terminal text to ChatGPT.
- **Raycast / Alfred / Spotlight integration.**
- **Multiple ChatGPT tab routing.** v0.1 targets only the active tab; future versions may name targets.
- **History / search of past local sessions** under `.cgpt-bridge/runs/`.
- **Configurable extension origin for forks** (still single value, not wildcard).
