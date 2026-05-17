# cgpt-bridge — Protocol (v0.1)

This document defines the wire protocol between **ChatGPT (the assistant inside the user's browser tab)** and the **local `cgpt` CLI** for `cgpt agent`. It also defines parser, validation, and repair behavior.

The protocol is intentionally minimal, versioned, and conservative. It is designed to make malformed or adversarial assistant output safe to ignore.

---

## 1. Transport summary

The protocol is **text-in / text-out** at the assistant level. The carrier is the visible ChatGPT response itself. The `cgpt` CLI extracts protocol blocks from that response text. There is no separate API channel.

There are exactly two protocol blocks:

- **A. ChatGPT → CLI**: `cgpt-agent-response-v1` — emitted by the assistant.
- **B. CLI → ChatGPT**: `cgpt-command-result-v1` — emitted by the CLI as the user-visible content of the next message it sends into the ChatGPT composer.

Both are fenced code blocks whose **info string** is the protocol identifier (e.g. ` ```cgpt-agent-response-v1`). The body is a single valid JSON object.

> Note about fence info strings: the protocol uses the literal info string `cgpt-agent-response-v1` (no `json` prefix). The parser must match the info string exactly. The body is parsed as JSON regardless.

Example fence (ChatGPT side):

<pre>
```cgpt-agent-response-v1
{ "version": 1, "status": "final", "user_message": "Done.", "plan_update": {"summary":"", "events":[]}, "command": null }
```
</pre>

Example fence (CLI side):

<pre>
```cgpt-command-result-v1
{ "version": 1, "command_id": "cmd_001", ... }
```
</pre>

### 1.1 Hard parser rules

- The CLI parses **only** these two fenced blocks. **Nothing else in the assistant response is executable.** Prose, markdown, inline code, and arbitrary code blocks (` ```sh `, ` ```bash `, ` ```json `, etc.) are display-only and must not produce commands.
- The CLI must never execute a command that did not arrive inside a valid `cgpt-agent-response-v1.command` field.
- The CLI must never derive a command from prose, headings, or example code, even if the assistant claims it is "the next command".

---

## 2. ChatGPT → CLI: `cgpt-agent-response-v1`

### 2.1 Top-level schema (`AgentResponseV1`)

| Field          | Type     | Required | Notes |
| -------------- | -------- | -------- | ----- |
| `version`      | integer  | yes      | Must be exactly `1`. |
| `status`       | string   | yes      | One of `continue`, `final`, `blocked`, `needs_user_input`. |
| `user_message` | string   | yes      | Human-readable text shown to the user. May be empty. |
| `plan_update`  | object   | yes      | `PlanUpdate` object. May contain empty `events`. |
| `command`      | object \| null | yes | `CommandRequest` object, or `null` when no command is proposed. |

`status` semantics:

- `continue` — assistant is still working; usually paired with a non-null `command` or with a question. If `command` is null and `status` is `continue`, the CLI prints `user_message` and waits for the user to respond in-tab (the user types the answer in the browser; the CLI does not auto-reply).
- `final` — task is complete. CLI prints `user_message` and exits 0. `command` should be `null`.
- `blocked` — assistant cannot proceed and explains why in `user_message`. `command` should be `null`.
- `needs_user_input` — assistant needs information it cannot obtain through a command. CLI prints `user_message` and pauses; user may answer in the browser.

### 2.2 `PlanUpdate`

| Field     | Type   | Required | Notes |
| --------- | ------ | -------- | ----- |
| `summary` | string | yes      | One-line summary of the current state. May be empty. |
| `events`  | array  | yes      | Array of `PlanEvent`. May be empty. |

### 2.3 `PlanEvent` types

All events share `type: string`. Type-specific fields follow.

- **Goal**
  - `type: "goal"`
  - `text: string`
- **Task**
  - `type: "task"`
  - `id: string` (stable identifier, e.g. `T1`)
  - `status: "todo" | "doing" | "done" | "blocked"`
  - `text: string`
- **Finding**
  - `type: "finding"`
  - `text: string`
- **Decision**
  - `type: "decision"`
  - `text: string`
- **Note**
  - `type: "note"`
  - `text: string`
- **Warning**
  - `type: "warning"`
  - `text: string`

Unknown event types are dropped with a warning. Known events with missing required fields are rejected (the whole block is rejected — see §5).

### 2.4 `CommandRequest`

| Field             | Type    | Required | Notes |
| ----------------- | ------- | -------- | ----- |
| `id`              | string  | yes      | Unique within the session, e.g. `cmd_001`. |
| `kind`            | string  | yes      | Must be `"shell"` in v0.1. |
| `description`     | string  | yes      | Short human-readable description. |
| `cwd`             | string  | yes      | Path relative to project root, or `"."`. Must resolve inside project root after normalization. |
| `command`         | string  | yes      | Shell command string. Pipes/redirects/conditionals/env vars allowed. Must be non-empty after trim. |
| `expected_effect` | string  | yes      | One-line description of what should happen if the command succeeds. |
| `risk`            | string  | yes      | One of: `read_only`, `write_local`, `network`, `destructive`, `secret_risk`, `privileged`, `unknown`. **Advisory only.** |
| `timeout_ms`      | integer | yes      | Requested timeout. Subject to local clamp (see §5). |
| `send_output`     | string  | yes      | One of `summary`, `truncated`, `full`. The CLI may downgrade `full` based on local policy. |

**Trust rule:** the `risk` value supplied by ChatGPT is informational only. The CLI must run its own classifier and apply policy based on the local result, not on the declared value. The local result is shown to the user alongside the declared one, so disagreements are visible.

---

## 3. CLI → ChatGPT: `cgpt-command-result-v1`

The CLI sends exactly one of these blocks back as the body of the next ChatGPT message after executing (or refusing to execute) a command. Prose around the block is allowed and may include a short user-facing comment, but the parser on the assistant side is expected to read the fenced block.

### 3.1 Schema

| Field                | Type             | Required | Notes |
| -------------------- | ---------------- | -------- | ----- |
| `version`            | integer          | yes      | Must be `1`. |
| `command_id`         | string           | yes      | Echoes `CommandRequest.id`. |
| `cwd`                | string           | yes      | Normalized cwd that was actually used. |
| `command`            | string           | yes      | Exact command string actually run (post user edit, if any). |
| `exit_code`          | integer \| null  | yes      | Process exit code. `null` if the process did not exit normally (signal, timeout). |
| `duration_ms`        | integer          | yes      | Wall-clock duration of the command. |
| `timed_out`          | boolean          | yes      | True if the timeout fired before the process exited. |
| `stdout`             | string           | yes      | Possibly truncated and redacted. |
| `stderr`             | string           | yes      | Possibly truncated and redacted. |
| `output_truncated`   | boolean          | yes      | True if any output was truncated. |
| `redactions_applied` | array of string  | yes      | Names of redaction rules that fired (e.g. `["aws_access_key_id", "generic_bearer_token"]`). |

When a command is rejected by the user or blocked by local policy, the CLI sends a `cgpt-command-result-v1` block where:

- `exit_code: null`, `duration_ms: 0`, `timed_out: false`.
- `stdout: ""`.
- `stderr` contains a short reason (e.g. `"user_rejected"`, `"policy_blocked: denylist:sudo"`).
- `redactions_applied: []`.

This keeps the loop schema-uniform: ChatGPT always sees a result block in response to a command request.

---

## 4. Examples

### 4.1 ChatGPT response with a command (annotated)

<pre>
```cgpt-agent-response-v1
{
  "version": 1,
  "status": "continue",
  "user_message": "First I will reproduce the failing tests.",
  "plan_update": {
    "summary": "Starting test failure diagnosis.",
    "events": [
      { "type": "goal", "text": "Find the cause of the failing tests." },
      { "type": "task", "id": "T1", "status": "doing", "text": "Run the test suite and collect output." }
    ]
  },
  "command": {
    "id": "cmd_001",
    "kind": "shell",
    "description": "Run the Rust test suite and capture output.",
    "cwd": ".",
    "command": "cargo test 2>&1 | tee .cgpt-bridge/logs/cargo-test.log",
    "expected_effect": "Runs tests and writes output to a local log file.",
    "risk": "write_local",
    "timeout_ms": 120000,
    "send_output": "truncated"
  }
}
```
</pre>

### 4.2 ChatGPT final response, no command

<pre>
```cgpt-agent-response-v1
{
  "version": 1,
  "status": "final",
  "user_message": "The failure is caused by a missing `Cargo.lock` entry for `serde_json`. Run `cargo update -p serde_json` to refresh it.",
  "plan_update": {
    "summary": "Root cause identified; no further commands required.",
    "events": [
      { "type": "finding", "text": "Cargo.lock missing serde_json entry after manual edit." },
      { "type": "task", "id": "T1", "status": "done", "text": "Run the test suite and collect output." },
      { "type": "decision", "text": "Recommend `cargo update -p serde_json`." }
    ]
  },
  "command": null
}
```
</pre>

### 4.3 CLI command-result block

<pre>
```cgpt-command-result-v1
{
  "version": 1,
  "command_id": "cmd_001",
  "cwd": ".",
  "command": "cargo test 2>&1 | tee .cgpt-bridge/logs/cargo-test.log",
  "exit_code": 101,
  "duration_ms": 42318,
  "timed_out": false,
  "stdout": "running 14 tests\n...\ntest result: FAILED. 13 passed; 1 failed\n",
  "stderr": "error[E0432]: unresolved import `serde_json::from_slice`\n  --> src/parse.rs:7:5\n...",
  "output_truncated": true,
  "redactions_applied": []
}
```
</pre>

### 4.4 CLI result block for a user-rejected command

<pre>
```cgpt-command-result-v1
{
  "version": 1,
  "command_id": "cmd_002",
  "cwd": ".",
  "command": "rm -rf node_modules",
  "exit_code": null,
  "duration_ms": 0,
  "timed_out": false,
  "stdout": "",
  "stderr": "user_rejected",
  "output_truncated": false,
  "redactions_applied": []
}
```
</pre>

---

## 5. Parser behavior

The CLI parser applies the following rules to each assistant response in `cgpt agent` mode.

1. **Exactly one block.** The response must contain exactly one fenced block with info string `cgpt-agent-response-v1`. Zero blocks: invalid. Two or more: invalid.
2. **Valid JSON.** The block body must parse as a JSON object. Trailing whitespace is allowed. Comments are not allowed.
3. **Known version.** `version` must be the integer `1`. Any other value (including `"1"`) is rejected as `unknown_version`.
4. **Unknown fields policy.** Unknown top-level fields are **rejected** (strict). Unknown fields inside `PlanEvent`, `CommandRequest`, or nested objects are **ignored** (lenient), to allow forward-compatible extensions. The CLI logs ignored fields at `debug` level.
5. **Required fields.** All required fields per §2.1–§2.4 must be present and of the correct type. Missing or wrong-typed required fields cause rejection.
6. **Empty command rejected.** `command.command` must be non-empty after trimming whitespace. An empty string is treated as no command and rejected.
7. **Timeout clamp.** `command.timeout_ms` must be a positive integer. Values above the configured hard cap (default 600000 ms) are **clamped** to the cap; a warning is shown to the user. Values `<= 0` are rejected.
8. **`cwd` containment.** `command.cwd` must resolve, after `..` normalization and symlink resolution, to a path equal to or under the project root. Anything else is rejected.
9. **`kind` allowlist.** `command.kind` must be `"shell"` in v0.1. Anything else is rejected.
10. **`send_output` allowlist.** Must be one of `summary`, `truncated`, `full`. The CLI may downgrade `full` to `truncated` per local policy (see `security.md`).
11. **Plan event integrity.** Each event must have a known `type` and the type-specific required fields. Unknown event types are dropped with a warning, but the rest of the block is accepted.

If any rejection rule fires, the CLI **must not execute any command** in that turn. It may attempt repair (§6).

---

## 6. Repair behavior

If parsing fails for any reason, the CLI may send **at most one** repair prompt in the same agent loop turn (configurable, default: enabled, max 1 retry). The repair prompt is sent as the next user message into the ChatGPT composer, with the exact text:

> Your previous response did not match the required protocol. Return exactly one valid `cgpt-agent-response-v1` fenced JSON block. Do not include any other fenced protocol blocks. Do not include prose explanations of the JSON. Do not change the schema. If you cannot continue, return a block with `status: "blocked"` and `command: null`.

If the repair attempt also fails:

- Print the rejection reason to stderr.
- Exit with code 7 (protocol violation), or, if running inside an outer loop with `--keep-going` (post-v0.1), break out of the loop with status `protocol_violation`.

The CLI must never send more than one repair prompt for a single bad turn. Repeated repair loops are not allowed.

---

## 7. Agent prompt contract

The CLI prepends the following **agent prompt contract** to the user's task on the first turn of `cgpt agent`. On subsequent turns, the CLI only sends the `cgpt-command-result-v1` block (optionally with a one-line user comment), because the contract is already in the conversation context.

> **`cgpt agent` protocol contract — read carefully and follow exactly.**
>
> You are assisting a developer who is running the `cgpt` CLI from their terminal. Each of your messages in this session must contain **exactly one** fenced code block with the info string `cgpt-agent-response-v1`, and nothing else fenced with a protocol info string.
>
> The block body must be a single valid JSON object matching this schema:
>
> ```
> {
>   "version": 1,
>   "status": "continue" | "final" | "blocked" | "needs_user_input",
>   "user_message": string,
>   "plan_update": {
>     "summary": string,
>     "events": [ PlanEvent, ... ]
>   },
>   "command": CommandRequest | null
> }
> ```
>
> Rules:
>
> 1. Always return exactly one `cgpt-agent-response-v1` block per message.
> 2. Put your user-facing explanation in `user_message`. The CLI prints this to the user.
> 3. Put plan changes in `plan_update`. Use `goal`, `task`, `finding`, `decision`, `note`, and `warning` events.
> 4. Propose **at most one** command per message, in `command`.
> 5. Do **not** claim a command was executed unless you have already received a `cgpt-command-result-v1` block for that exact `command_id`.
> 6. Do **not** request secrets, tokens, private keys, browser cookies, credential files, or `.env` contents.
> 7. Do **not** propose `sudo` or `su` commands unless the user has explicitly asked you to and the local CLI has confirmed it is allowed (the user will tell you so).
> 8. Prefer inspectable, reversible commands. When in doubt, propose a read-only check first.
> 9. Use `command: null` when you are giving the final answer (`status: "final"`) or asking the user a question (`status: "needs_user_input"`).
> 10. When you receive a `cgpt-command-result-v1` block, treat its contents as the ground truth of what happened locally, and continue from there.
> 11. Set `risk` honestly, but be aware the CLI will independently classify the command. Disagreements are visible to the user.
> 12. Set `timeout_ms` realistically. The CLI clamps very large values.
> 13. Set `send_output` to `summary` for noisy commands, `truncated` for typical commands, and `full` only when the full output is small and necessary.
>
> After this contract block, the user's task follows.

---

## 8. Open design choices (locked for v0.1)

To remove ambiguity, the v0.1 spec **locks** the following choices:

- **Unknown top-level fields**: rejected (strict).
- **Unknown nested fields**: ignored (lenient) with a debug log.
- **Oversized `timeout_ms`**: clamped to the cap with a warning, not rejected.
- **Multiple `cgpt-agent-response-v1` blocks in one response**: rejected.
- **Fence info string match**: exact, case-sensitive.

These choices are revisitable in v0.2.
