# cgpt-bridge — remaining work

Snapshot: 2026-05-17. Working v0.1 bundle exists
(`dist/cgpt-bridge-0.0.1-<os>-<arch>.tar.gz`). Below is what is **not**
done yet, ordered roughly by user impact.

Numbers in parentheses reference `docs/roadmap.md` milestones.

---

## 1. v0.1 — must ship before calling it done

### 1.1 `cgpt doctor` (M10)

No subcommand exists yet. Required checks:

- Native Messaging manifest present at the OS-specific path
  (macOS / Linux differ — see `install/*/install-host.sh` for paths).
- `allowed_origins` in the manifest contains a `chrome-extension://…/`
  origin and the host `path` is executable and matches `bin/cgpt-bridge-host`.
- UDS socket exists at `default_socket_path()` and accepts a connection.
- Host responds to a ping frame within a short timeout.
- Service worker responds to `pingNative()` (extension side).
- At least one `https://chatgpt.com/*` tab is open in Chrome.
- Content script in that tab answers a `diagnose` request: composer
  selector resolves, send button resolves, last-assistant `data-message-id`
  present.
- `$SHELL` resolves to an executable; `/bin/sh -lc` works as a fallback.
- `.cgpt-bridge/` writable in the current project root.

Each check prints `PASS` / `FAIL` + a one-line remediation. Overall
exit 0 if all pass, 1 otherwise.

Wire-up:
- Add `Command::Doctor(DoctorArgs)` in `cli/src/args.rs`.
- Add `cli/src/doctor.rs` with one function per check, each returning
  `Result<(), String>`. Stable order so output is diffable.
- Help text + a README pointer.

### 1.2 Packaging polish (M11)

- README needs a "fresh-machine install" section that mirrors
  `INSTALL.md` from the bundle. Currently the bundle has the steps but
  the repo root README does not.
- `cgpt --version` already works via clap. Verify
  `cgpt-bridge-host --version` does too (or add it).
- The bundler currently bakes the binary's absolute path into the
  manifest at install time. Document the implication: if the user
  moves the unpacked bundle after running the installer, Chrome
  Native Messaging will fail to launch the host. Either:
  a) document "do not move after install"; or
  b) make `install-host.sh` re-resolve the path on every run and
     suggest a fixed location like `~/.local/cgpt-bridge/`.
- Add `scripts/build-release.sh --linux` cross-build support if we
  ever publish from macOS for Linux users. Today it builds for the
  host machine only.

### 1.3 Tests for the auto-approve paths

`runner::prompt_and_run` has new branches that are currently uncovered:
- `--auto-readonly`: `LocalRisk::ReadOnly` should run without TTY.
- `--yolo` with `LocalRisk::WriteLocal` / `Unknown`: should run.
- `--yolo` with `LocalRisk::Blocked`: must still hit
  `RunOutcome::PolicyBlocked`.

Add unit tests in `cli/src/runner.rs::tests` using `assume_no_tty: true`
plus the new flags so the read-from-stdin path doesn't fire.

### 1.4 Stale `cgpt ask` UX

- Add a phase label hint for `cgpt ask` similar to `cgpt agent`'s turn
  labels ("asking ChatGPT (timeout Ns)…"). Today it just says
  "asking ChatGPT…" which is fine but doesn't show the timeout the
  user picked.
- Decide whether `--yolo` / `--auto-readonly` belong on `cgpt ask`.
  Today they are only on `cgpt agent` because `ask` does not run
  commands. Probably leave as-is; document explicitly.

### 1.5 Documentation drift

- `CLAUDE.md` and `docs/security.md` were updated for `--yolo` /
  `--auto-readonly`. Verify `docs/requirements.md` does not still
  claim "no auto-run in v0.1".
- `docs/roadmap.md` M11 acceptance still says "from `git clone` to a
  passing `cgpt doctor` in under ten minutes" — blocked by §1.1
  above. Re-time the install once `doctor` lands.

---

## 2. Reliability tech debt from the UX work

### 2.1 Hidden-tab keep-alive caveat

`extension/src/chatgptAdapter.ts` uses a silent `OscillatorNode` to mark
the tab audible so Chrome doesn't throttle it in the background. This
works only after the page has received a user gesture (Chrome autoplay
policy).

- Detect the suspended state in `startKeepAlive` and surface a clearer
  user-facing error code (`tab_throttled` or similar) when the keep-alive
  fails AND `waitForNewAnswer` later times out.
- Consider auto-clicking inside the page on a non-interactive element
  to satisfy the gesture requirement — risky, may trip ChatGPT's anti-bot.
- A safer alternative: warn from the CLI ("if this hangs > 30 s without
  a phase update, focus the ChatGPT tab once and retry").

### 2.2 Selector fragility

`chatgptAdapter.ts` carries the only ChatGPT-specific selectors. They
match by `data-message-id`, role, and stop-button text in English /
Russian. ChatGPT redesigns silently. Add:

- A tiny `npm run snapshot` script that dumps a current chatgpt.com
  page tree to `extension/snapshots/<date>.html` so we have a baseline
  when something breaks.
- A `diagnose` invocation in CI (skipped by default, runnable manually)
  to surface "selectors no longer match".

### 2.3 MutationObserver granularity

Today we observe `document.body` with `subtree: true, characterData:
true`. That fires on every page mutation, not just the assistant
message. It's fine for correctness but burns CPU. Narrow to the
message-list container once we have a stable selector for it.

### 2.4 Flaky `host/tests/router_e2e.rs`

`cli_ask_round_trips_through_extension_mock` fails ~1 in 3 runs
locally with `read response: Eof` — looks like a race between the
mock extension thread closing stdin and the test thread reading the
response. Investigate and fix; this is the one perennially-flaky
test in the suite.

---

## 3. Out of v0.1, deferred (already listed in `docs/roadmap.md`)

These are tracked here only so they're visible in one place; do **not**
do them as part of "ship v0.1":

- Windows support.
- Brave / Edge / Firefox / Vivaldi support.
- Chrome Web Store packaging.
- Hardened auto-approve so it can be the default UX.
- Sandboxed command execution (`sandbox-exec` on macOS, `bwrap` on Linux).
- `kind: "patch"` command type — assistant sends a unified diff, CLI
  applies it after diff review, instead of `kind: "shell"` with patch
  commands.
- `apply_changes` structured edits.
- Multi-tab orchestration / parallel agent loops.
- Reading or exporting past ChatGPT conversations (intentionally never).

---

## 4. Suggested next session order

1. `cgpt doctor` (§1.1). Highest leverage — every "it doesn't work"
   report turns into a one-line FAIL with a fix.
2. Tests for `--yolo` / `--auto-readonly` (§1.3). Cheap; raises
   confidence in the policy-bypass code paths.
3. README install section (§1.2 first bullet).
4. Selector snapshot harness (§2.2) — small but pays off the next time
   ChatGPT redesigns.
5. Flaky `router_e2e.rs` (§2.4) — annoying noise in the test signal.

Everything else is honest post-v0.1.
