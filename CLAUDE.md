# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

Design stage / pre-MVP. **No application code yet.** Only `docs/` and `README.md` exist. The Stage 1 documentation in `docs/` is binding: it defines requirements, the wire protocol, architecture, security policy, and the M1–M11 roadmap that the implementation must follow.

Future stages will add Rust (`cli/`, `host/`) and TypeScript (`extension/`) directories. Until then, code-related questions should be answered by consulting `docs/`, not by inventing implementation details.

## Read the docs before doing anything substantive

The documentation is the source of truth at this stage. Before adding code, schemas, or policy:

- `docs/requirements.md` — v0.1 scope, the three subcommands (`cgpt ask`, `cgpt agent`, `cgpt doctor`), plan storage layout, exit codes, acceptance criteria.
- `docs/protocol.md` — `cgpt-agent-response-v1` and `cgpt-command-result-v1` schemas, parser rules, repair flow, and the full agent prompt contract. **Read this first** before touching anything related to the agent loop.
- `docs/architecture.md` — ASCII system diagram, component responsibilities, message flows.
- `docs/security.md` — threat model, confirmation UI, denylist, redaction patterns, truncation caps, Native Messaging and extension hardening.
- `docs/roadmap.md` — milestones M1 (done) through M11; future items explicitly out of v0.1.

If a doc and your intuition disagree, the doc wins. If the doc is wrong, update the doc in the same change.

## Big-picture architecture

The system has two halves bridged by Chrome Native Messaging (no localhost port, no clipboard):

```
Terminal / scripts
  -> cgpt CLI (Rust)
  -> Native Messaging host (Rust, thin pipe)
  -> Chrome extension service worker (MV3, TypeScript)
  -> Content script on https://chatgpt.com/*
  -> chatgptAdapter.ts (all DOM-specific code lives here)
  -> Active ChatGPT tab
```

Load-bearing invariants — do not break these without updating the docs first:

- **Policy lives in the CLI.** The native host is a thin pipe. The extension is a router. The content script does DOM work only on request, never in the background.
- **All ChatGPT-specific DOM knowledge is isolated in `chatgptAdapter.ts`.** No selectors, ARIA labels, or stabilization logic outside that file. UI breaks should be one-file fixes.
- **The CLI never executes a command from prose, markdown, inline code, or any code block other than a single valid `cgpt-agent-response-v1.command` field.** Multiple, missing, or malformed protocol blocks are rejected; the CLI may send at most one repair prompt per turn.
- **The `risk` value supplied by ChatGPT is advisory.** The CLI runs its own classifier and applies the denylist independently. Both values are shown to the user.
- **Every command requires explicit user confirmation by default.** Two opt-in flags relax this: `--auto-readonly` (auto-runs commands the local classifier marks `read_only`) and `--yolo` (auto-runs every non-denylisted command, with a loud per-command warning). Default is interactive; pressing Enter never auto-runs. Denylist hits remain blocked regardless of flag.
- **No localhost HTTP or WebSocket server.** Transport is Chrome Native Messaging only.
- **No clipboard.** The content script writes the composer text via DOM property + input event dispatch.
- **Extension host permission is exactly `https://chatgpt.com/*`.** No wildcards. `allowed_origins` in the native messaging manifest is a single specific extension id, no wildcards.
- **Stdout discipline.** `cgpt ask` stdout = assistant message. `cgpt agent` stdout = `user_message` and final message. Everything else to stderr. The native host writes only length-prefixed JSON to stdout; logs go to stderr.

## Local state model

`cgpt agent` writes a per-project directory:

- `.cgpt-bridge/plan.jsonl` — append-only event log. **Source of truth.**
- `.cgpt-bridge/plan.md` — regenerated from `plan.jsonl`. Hand edits are not preserved.
- `.cgpt-bridge/session.json` — current session metadata. No command output.
- `.cgpt-bridge/runs/<session-id>/` — transcript and per-command (redacted) records.
- `.cgpt-bridge/logs/` — rotating CLI/host logs.

Any crash must leave `plan.jsonl` parseable line-by-line.

## Safety guardrails to preserve in every change

When implementing or reviewing code that touches command handling, protocol parsing, or messaging:

- Validate `command.cwd` by normalizing `..` and resolving symlinks, then confirm the result is at or under the project root.
- Apply the denylist per pipeline stage (split on `|`, `&&`, `;`, subshells); a denylisted stage anywhere blocks the whole command.
- Redact secrets **before** truncating output (so high-entropy strings are not half-cut into something that no longer matches).
- The CLI may downgrade `send_output: "full"` to `"truncated"` when caps would be exceeded; total output to ChatGPT capped at 256 KB (Chrome's 1 MB Native Messaging limit must not be approached).
- Clamp `timeout_ms` above the configured cap (default 600000 ms) with a warning; do not reject.
- Use `"$SHELL" -lc '<command>'` with `/bin/sh -lc` fallback. Windows is future work.

## Documentation-only conventions

- Do not pin ChatGPT CSS class names in docs or in code outside the adapter. Prefer ARIA / semantic selectors.
- Do not introduce features marked out of v0.1 in `docs/roadmap.md` (Windows, `--auto-readonly`, sandboxing, patch kind, Web Store packaging, etc.) without a roadmap update first.
- Keep `docs/protocol.md` §5 (parser rules) and `docs/protocol.md` §8 (locked design choices) consistent. If you change one, change both.

## Build / test commands

None yet — no source tree, no `Cargo.toml`, no `package.json`. When implementation begins (M2 onward per `docs/roadmap.md`), add the commands here in the same change that adds the build files.
