# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

Pre-MVP implementation. The repository holds a Rust workspace (`cli/`, `host/`, `protocol/`, `core/`, `gateway/`), the Chrome extension (`extension/`, TypeScript), installers (`install/`), build and installer-check scripts (`scripts/`) and the CI workflow (`.github/workflows/release.yml`). The documentation in `docs/` is binding: requirements, wire protocol, architecture, security policy and the M1–M11 roadmap. The OpenCode gateway (`cgpt-gateway`) follows `docs/superpowers/specs/2026-10-26-opencode-gateway-design.md` and the plan `docs/superpowers/plans/2026-10-26-cgpt-gateway.md`.

## Read the docs before doing anything substantive

The documentation is the source of truth at this stage. Before adding code, schemas, or policy:

- `docs/requirements.md` — v0.1 scope, the subcommands (`cgpt ask`, `cgpt agent`; `cgpt doctor` is planned, not implemented), plan storage layout, exit codes, acceptance criteria.
- `docs/protocol.md` — `cgpt-agent-response-v1` and `cgpt-command-result-v1` schemas, parser rules, repair flow, and the full agent prompt contract. **Read this first** before touching anything related to the agent loop.
- `docs/architecture.md` — ASCII system diagram, component responsibilities, message flows.
- `docs/security.md` — threat model, confirmation UI, denylist, redaction patterns, truncation caps, Native Messaging and extension hardening.
- `docs/roadmap.md` — milestones M1 (done) through M11; future items explicitly out of v0.1.
- `docs/superpowers/specs/2026-10-26-opencode-gateway-design.md` — the OpenCode gateway: OpenAI-compatible API, chat per session, tool calls, Windows and macOS installers.

If a doc and your intuition disagree, the doc wins. If the doc is wrong, update the doc in the same change.

## Big-picture architecture

Two clients reach ChatGPT in the user's Chrome through the same bridge (no clipboard):

```
Terminal / scripts                       OpenCode
  -> cgpt CLI (Rust)                       -> cgpt-gateway (Rust, 127.0.0.1 + bearer token)
         \                                   /
          -> local socket (Unix socket; named pipe with an owner-only DACL on Windows)
  -> Native Messaging host (Rust, thin pipe)
  -> Chrome extension service worker (MV3, TypeScript)
  -> Content script on https://chatgpt.com/*
  -> chatgptAdapter.ts (all DOM-specific code lives here)
  -> CLI asks: the user's ChatGPT tab (active one, sticky across agent turns)
     gateway asks: the extension's own ChatGPT tabs (a pool of up to 8),
                   in one own unfocused window
```

Load-bearing invariants — do not break these without updating the docs first:

- **Policy lives in the clients.** The CLI applies it for `cgpt agent`; the gateway applies it for OpenCode (denylist on every tool call, redaction of all text sent to ChatGPT; images go unredacted, `docs/security.md` §15). Both take denylist and redaction from `core/`. The native host is a thin pipe. The extension is a router. The content script does DOM work only on request, never in the background.
- **All ChatGPT-specific DOM knowledge is isolated in `chatgptAdapter.ts`.** No selectors, ARIA labels, or stabilization logic outside that file. UI breaks should be one-file fixes. The one exception is ChatGPT's answer stream (endpoint and event format): that knowledge lives in `pageShim.ts` and `sseParser.ts`.
- **The CLI never executes a command from prose, markdown, inline code, or any code block other than a single valid `cgpt-agent-response-v1.command` field.** Multiple, missing, or malformed protocol blocks are rejected; the CLI may send at most one repair prompt per turn.
- **The `risk` value supplied by ChatGPT is advisory.** The CLI runs its own classifier and applies the denylist independently. Both values are shown to the user.
- **Every command requires explicit user confirmation by default.** Two opt-in flags relax this: `--auto-readonly` (auto-runs commands the local classifier marks `read_only`) and `--yolo` (auto-runs every non-denylisted command, with a loud per-command warning). Default is interactive; pressing Enter never auto-runs. Denylist hits remain blocked regardless of flag.
- **One network listener, and only the gateway's.** `cgpt-gateway` binds `127.0.0.1` only (never another interface) and requires `Authorization: Bearer <token>` on every request. The CLI and the native host never listen on a port: clients reach the host over a local socket, and the host talks to the extension over Chrome Native Messaging only.
- **Gateway asks never touch the user's tabs.** They run in the extension's own ChatGPT tabs (a pool of up to 8 in one own window, opened without focus); the CLI's untargeted asks never use any tab of that window. The gateway runs turns of different OpenCode sessions in parallel (up to `max_parallel`, default 3, and no more than the extension reports as `own_tabs`), turns of one session one at a time (`docs/superpowers/specs/2026-11-05-parallel-sessions-design.md`).
- **No clipboard.** The content script writes the composer text via DOM property + input event dispatch.
- **Extension host permission is exactly `https://chatgpt.com/*`.** No wildcards. `allowed_origins` in the native messaging manifest is a single specific extension id, no wildcards.
- **Stdout discipline.** `cgpt ask` stdout = assistant message. `cgpt agent` stdout = `user_message` and final message. Everything else to stderr. The native host writes only length-prefixed JSON to stdout; logs go to stderr.

## Local state model

`cgpt agent` writes a per-project directory:

- `.cgpt-bridge/plan.jsonl` — append-only event log. **Source of truth.**
- `.cgpt-bridge/plan.md` — regenerated from `plan.jsonl`. Hand edits are not preserved.
- `.cgpt-bridge/session.json` — metadata of the latest session. No command output.
- `.cgpt-bridge/runs/<session-id>/` — transcript, per-command (redacted) records, and the session's own `session.json` (status, chat address).
- `.cgpt-bridge/logs/` — created empty, reserved for CLI/host logs. **Not implemented:** nothing writes there, nothing is rotated.

Any crash must leave `plan.jsonl` parseable line-by-line.

## Safety guardrails to preserve in every change

When implementing or reviewing code that touches command handling, protocol parsing, or messaging:

- Validate `command.cwd` by normalizing `..` and resolving symlinks, then confirm the result is at or under the project root.
- Apply the denylist per pipeline stage (split on `|`, `&&`, `;`, subshells); a denylisted stage anywhere blocks the whole command.
- Redact secrets **before** truncating output (so high-entropy strings are not half-cut into something that no longer matches).
- The CLI may downgrade `send_output: "full"` to `"truncated"` when caps would be exceeded; total output to ChatGPT capped at 256 KB (Chrome's 1 MB Native Messaging limit must not be approached).
- Clamp `timeout_ms` above the configured cap (default 600000 ms) with a warning; do not reject.
- Use `"$SHELL" -lc '<command>'` with `/bin/sh -lc` fallback (the `cgpt` CLI runs on macOS and Linux). On Windows commands are run by OpenCode, not by this repo; the gateway, native host and extension are supported on Windows and macOS.

## Documentation-only conventions

- Do not pin ChatGPT CSS class names in docs or in code outside the adapter. Prefer ARIA / semantic selectors.
- Do not introduce features marked out of v0.1 in `docs/roadmap.md` (sandboxing, patch kind, Web Store packaging, etc.) without a roadmap update first. Windows support is in scope for the gateway path (gateway, native host, extension, installer) per the gateway spec; the `cgpt` CLI stays macOS and Linux.
- Keep `docs/protocol.md` §5 (parser rules) and `docs/protocol.md` §8 (locked design choices) consistent. If you change one, change both.

## Build / test commands

```sh
. "$HOME/.cargo/env"                          # cargo/rustup on PATH
cargo build --workspace
cargo test --workspace                        # all Rust tests; output must be warning-free
cargo test -p cgpt-bridge-protocol local_socket   # local socket / named pipe alone

cd extension && npm ci && npm run typecheck && npm test && npm run build   # extension → extension/dist

./scripts/build-release.sh                    # macOS/Linux bundle in dist/ (DIST_DIR=… to stage elsewhere)
pwsh ./scripts/build-release.ps1              # Windows bundle (.zip)
./scripts/check-install-macos.sh <unpacked-bundle-dir>   # installer checks on a temporary HOME
```

`scripts/check-install-windows.ps1` writes the real HKCU registry keys and values (native host key, the Run autostart value) and looks into the Startup folder: it runs in CI only (`GITHUB_ACTIONS` set) unless given `-Force`. CI (`.github/workflows/release.yml`) runs all of the above on `windows-latest` and `macos-latest`; `ubuntu-latest` runs the tests, packs the Linux bundle (CLI, native host, extension) and checks `install/linux/install-host.sh` on it.

Never test against the live bridge: no `cgpt ask`, no gateway request against the default endpoint, no `BridgeBackend` on `default_endpoint()` — every message there lands in the user's real ChatGPT account. Use fakes and temporary endpoints.
