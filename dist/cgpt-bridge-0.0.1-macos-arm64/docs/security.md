# cgpt-bridge — Security (v0.1)

This document defines the v0.1 security posture: threat model, trust boundaries, confirmation model, command-execution risks, prompt-injection defenses, secret handling, redaction, filesystem boundaries, Native Messaging hardening, extension hardening, the v0.1 denylist, and what v0.1 intentionally does not support.

The goal is to make the **default path safe**: a careful user, following the on-screen confirmation prompts, should not be silently harmed by anything ChatGPT proposes or by content that ChatGPT echoes from files, logs, or the web.

---

## 1. Threat model

Primary threats considered:

1. **Untrusted assistant output.** The assistant may propose dangerous, sensitive, or destructive commands, either by mistake or because it has been prompt-injected through content it has read.
2. **Prompt injection via untrusted text.** Files, logs, READMEs, issue bodies, package scripts, error messages, and any text reaching the assistant may contain instructions that try to redirect the assistant's behavior.
3. **Local credential exfiltration.** Attackers (via the channel above) attempting to read secrets from disk or environment and send them back through the assistant to a third party.
4. **Filesystem damage.** Destructive commands acting on the user's home directory or system paths.
5. **Privilege escalation.** Commands invoking `sudo`/`su` to act outside the user's normal scope.
6. **Native Messaging confusion.** Another local process or extension piggy-backing on the native host.
7. **Browser-side abuse.** A compromised or malicious script in another tab trying to use the extension. Or the extension being granted excessive permissions.
8. **Side channels.** Clipboard, environment variables, or background page state leaking data.

Not in v0.1 threat model (out of scope):

- A fully compromised local machine. If the attacker already has user-level code execution, this tool is not a hardening boundary.
- A malicious Chrome browser binary.
- A malicious OS kernel.
- Network-level attackers (the tool does not open a network port; TLS to ChatGPT is the browser's job).

---

## 2. Trust boundaries

Listed from **least trusted** to **most trusted**:

1. **ChatGPT output** (assistant text, fenced JSON blocks). **Untrusted.** Parsed and validated locally; never executed without user confirmation.
2. **Inputs to ChatGPT** (files, logs, web pages the user pastes or pipes in). **Untrusted.** Treated as data that may attempt prompt injection.
3. **Chrome extension** (service worker + content script). **Semi-trusted.** Runs in the user's browser, minimal permissions, no remote code, no eval. Trust comes from the user installing it and from the manifest+code being auditable.
4. **Native Messaging host**. **Trusted as a thin pipe.** Validates message sizes and JSON shape; performs no policy decisions.
5. **`cgpt` CLI**. **Trusted decision point.** Performs parsing, validation, classification, redaction, plan management, and user-facing confirmation.
6. **The user**. **Final authority.** Approves or rejects every command.

The CLI is where the safety policy lives. The browser side is a transport for prompt text and assistant text; it must never gain the ability to run commands on the local machine.

---

## 3. User confirmation model

**Every command requires explicit user confirmation by default.** Two opt-in
flags relax this for trusted contexts:

- `cgpt agent --auto-readonly <task>` — commands the **local** classifier
  marks `read_only` auto-approve. The confirmation panel is still printed so
  the run is auditable; the keypress prompt is skipped. Anything classified
  `write_local`, `risky`, or `unknown` still prompts. Denylist hits remain
  blocked.
- `cgpt agent --yolo <task>` — every non-denylisted command auto-approves
  regardless of risk class. The confirmation panel is still printed and a
  loud `⚠  --yolo` line is emitted per command. Intended for disposable VMs
  or sandboxes where the user explicitly accepts the blast radius. The
  denylist still blocks dangerous patterns.

Both flags are off by default. Neither bypasses the local classifier or the
denylist; they only short-circuit the `[r]un` keypress.

When the assistant proposes a command, the CLI renders a confirmation panel on stderr containing:

- The session id and `command.id`.
- The resolved absolute `cwd`, and whether it is the project root.
- The exact command string.
- The assistant's `expected_effect`.
- The assistant's declared `risk`.
- The CLI's **locally classified** risk, with the specific rule(s) that matched.
- The declared `timeout_ms` (and whether it was clamped).
- The declared `send_output` (and whether the CLI plans to downgrade it).
- Any warnings (denylist hit, ambiguity, `&` backgrounding, recursive flags, etc.).

Then a single-keystroke prompt:

```
[r]un  [e]dit  [s]kip  [q]uit  >
```

- **`r`** — run as proposed.
- **`e`** — open an inline edit prompt; the user can modify the command text. The post-edit command is re-classified and the panel is re-rendered before another confirmation. The edited string is what is run and what is reported in `cgpt-command-result-v1.command`.
- **`s`** — skip; CLI emits a `user_rejected` result block.
- **`q`** — quit the loop; exit code 10.

There is no default "yes". Pressing Enter at the prompt re-renders the prompt, it does not run the command.

For commands flagged by the local denylist, the prompt is replaced by:

```
BLOCKED by local policy: <rule>
[e]dit  [s]kip  [q]uit  >
```

The `r` option is not offered. The user must edit the command into something that does not trigger the denylist, or skip, or quit.

---

## 4. Command execution risks

Risks the CLI mitigates by construction:

- **Silent execution.** Mitigated: every command needs confirmation.
- **Wrong working directory.** Mitigated: `cwd` is normalized and verified to be at or under the project root.
- **Runaway commands.** Mitigated: every command has a timeout (default 60s, hard cap 600000 ms).
- **Inheriting unsafe environment.** Mitigated by being explicit: commands run under `$SHELL -lc` with the user's normal environment. The user is told they are running with their full env; sensitive env vars are the user's responsibility (and are partially mitigated by the env-printing block in §6).
- **Output flooding.** Mitigated: stdout/stderr are bounded by per-stream caps (default 1 MB each at capture; further truncated for `send_output`).

Risks the CLI **does not** mitigate by construction:

- **Network egress from a confirmed command.** If the user approves a command that calls out to the network, that is their decision.
- **Commands that mutate state by running tools (e.g. `npm install`).** Confirmation is the only gate.
- **TOCTOU between classification and execution.** The CLI classifies the literal command string; runtime behavior can diverge.

---

## 5. Prompt injection risks

Anywhere the assistant reads text, the assistant may be exposed to injected instructions. Sources to assume hostile:

- Files in the repo (READMEs, code comments, fixtures, test data).
- Log output.
- Issue/PR bodies the user pastes in.
- HTML / scraped content.
- Package scripts (`package.json`, `Makefile`, etc.).
- Error messages and stack traces.

Defenses:

- The CLI **never** executes a command derived from prose or from non-protocol code blocks. Only a single valid `cgpt-agent-response-v1.command` field is eligible.
- The CLI's validator does not look at `risk` declared by the assistant when deciding whether to block — it runs its own classifier.
- The user is shown both the declared and locally classified risk; disagreements are highlighted.
- The CLI's denylist (§6) is independent of assistant-provided metadata.
- The confirmation step itself is an out-of-band check that prompt injection cannot bypass: it requires a keypress at the local TTY.

What we explicitly do not promise:

- We do not claim to detect every injection attempt. The defense is "the user approves every command", not "we classify intent correctly".

---

## 6. Denylist (v0.1 hard blocks)

The denylist applies to the literal command string after tokenization. Matching is conservative; on doubt the CLI marks the command as needing edit/skip and shows the rule that matched.

v0.1 hard blocks (the command will not be offered `[r]un`):

- **Privilege escalation**: `sudo`, `sudo -i`, `sudo -s`, `su`, `pkexec`, `doas` (unless `allow_sudo = true` in user config **and** the user re-confirms; project config cannot loosen this).
- **Recursive root/home destruction**: any `rm` invocation matching `rm -rf /`, `rm -rf /*`, `rm -rf ~`, `rm -rf ~/`, `rm -rf $HOME`, `rm -rf "$HOME"`, `rm -fr /`, `rm -R -f /`, and analogous variants.
- **Raw disk writes**: `dd if=...`, `dd of=/dev/...`, `mkfs`, `mkfs.*`, `fdisk`, `parted`, `blkdiscard`, `wipefs`.
- **Permission/ownership sweeps**: `chmod -R / ...`, `chmod -R /* ...`, `chown -R / ...`, `chown -R /* ...`.
- **Network-fetch-then-execute pipelines**: `curl ... | sh`, `curl ... | bash`, `curl ... | zsh`, `wget ... | sh`, `wget ... | bash`, and equivalents using `-O- | sh`, `--output - | sh`. Also blocked: `bash -c "$(curl ...)"`, `sh -c "$(wget ...)"`, `eval "$(curl ...)"`.
- **Credential extraction**:
  - Reads under `~/.ssh/` (any file).
  - Reads under `~/.aws/credentials`, `~/.aws/config`, `~/.config/gcloud/`, `~/.kube/config`, `~/.netrc`, `~/.pgpass`, `~/.docker/config.json`.
  - Reads under macOS Keychain (`security find-generic-password`, `security find-internet-password`, `security dump-keychain`).
  - Reads from Chrome/Firefox/Edge profile directories (`Cookies`, `Login Data`, etc.).
  - `gh auth token`, `aws sts get-session-token`, `op` (1Password CLI), `bw` (Bitwarden CLI) — blocked by default; user may override via `denylist_extra` removal in user config (project config cannot).
- **Environment dumps**: bare `env`, `printenv`, `set` (when used as an environment-dump command), `export -p`, `compgen -e`. Note: `env VAR=val command ...` is **not** an env dump and is allowed.
- **`.env` reads by default**: `cat .env`, `cat .env.*`, `less .env`, `head .env`, `tail .env`, and `<.env` redirections. User config can allowlist specific files via `denylist_extra` removal.
- **Browser cookie / storage access**: any read targeting `Cookies`, `Local Storage`, `Login Data`, or `key4.db` under user-profile paths.

The denylist is applied to the resolved command string and to each pipeline stage independently. Compound commands (`a && b`, `a; b`, subshells) are tokenized so that a denylisted stage anywhere in the chain blocks the whole command.

Warnings (offered as `[r]un` with a clear warning, but not hard-blocked):

- `rm -rf <path>` where `<path>` resolves inside the project root.
- Any command using `&` to background a process.
- Commands writing outside the project root (e.g. `> /tmp/foo`).
- Network egress commands (`curl`, `wget`, `nc`, `ssh`, `scp`, `rsync` to a remote host).
- Long-running watchers (`tail -f`, `watch`, `npm run dev`) — flagged because they will likely hit the timeout.

This policy is intentionally not exhaustive. It will evolve. A v0.2 ticket should include a periodic review of recent assistant-proposed commands to identify rules to add.

---

## 7. Secret redaction

Before any command output is sent back to ChatGPT in a `cgpt-command-result-v1`, the CLI runs a redaction pipeline. Each pattern that matches replaces the matched span with `«redacted:<rule_name>»` and adds `<rule_name>` to `redactions_applied`.

v0.1 patterns:

| Rule name | Pattern (informal) |
| --- | --- |
| `openai_api_key` | `sk-[A-Za-z0-9_-]{20,}` and `sk-proj-[A-Za-z0-9_-]{20,}` |
| `anthropic_api_key` | `sk-ant-[A-Za-z0-9_-]{20,}` |
| `github_token_classic` | `ghp_[A-Za-z0-9]{30,}` |
| `github_token_fine_grained` | `github_pat_[A-Za-z0-9_]{20,}` |
| `github_oauth` | `gho_[A-Za-z0-9]{30,}` |
| `github_app` | `(ghu|ghs|ghr)_[A-Za-z0-9]{30,}` |
| `aws_access_key_id` | `\b(AKIA|ASIA)[0-9A-Z]{16}\b` |
| `aws_secret_access_key` | 40-char base64-ish strings adjacent to `aws_secret_access_key`, `AWS_SECRET_ACCESS_KEY`, or `secret_access_key` |
| `gcp_service_account_key` | `"private_key": "-----BEGIN PRIVATE KEY-----...` JSON-embedded blocks |
| `slack_token` | `xox[abprs]-[A-Za-z0-9-]{10,}` |
| `private_key_block` | `-----BEGIN (RSA |EC |OPENSSH |DSA |)PRIVATE KEY-----` through `-----END ... PRIVATE KEY-----` |
| `pgp_private_key_block` | `-----BEGIN PGP PRIVATE KEY BLOCK-----` through `-----END PGP PRIVATE KEY BLOCK-----` |
| `generic_bearer_token` | `Authorization:\s*Bearer\s+[A-Za-z0-9._\-]+` |
| `generic_basic_auth` | `Authorization:\s*Basic\s+[A-Za-z0-9+/=]+` |
| `dotenv_secret_assignment` | Lines matching `^(?i)(.*?)(SECRET\|TOKEN\|PASSWORD\|PASSWD\|API_KEY\|APIKEY\|PRIVATE_KEY)([A-Z0-9_]*)=.+` — value redacted, key preserved |
| `jwt_like` | `eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+` |
| `npm_token` | `npm_[A-Za-z0-9]{30,}` |
| `stripe_live_secret` | `sk_live_[A-Za-z0-9]{20,}` |

Rules:

- Redaction is applied to both stdout and stderr.
- Redaction runs **before** truncation, so high-entropy strings aren't half-cut into something that no longer matches a pattern.
- `redactions_applied` is included in the result block so the assistant knows redactions happened.
- The user's local `config.toml` may add patterns via `redaction_extra_patterns`. It may not remove built-in patterns in v0.1.
- Patterns are conservative. False positives are preferred over false negatives. A redaction is not an admission that a secret was present, only that something looked like one.

---

## 8. Output truncation

After redaction, output is truncated for transmission to ChatGPT based on the `send_output` value declared in `CommandRequest`:

- `summary` — keep first 2 KB of stdout and first 2 KB of stderr.
- `truncated` — keep first 16 KB and last 4 KB of stdout (with `«…N bytes elided…»` marker in the middle), same shape for stderr.
- `full` — keep up to a hard cap of 256 KB total across stdout+stderr. **The CLI may downgrade `full` to `truncated`** if total exceeds the cap; `output_truncated` is set true in that case.

The 256 KB hard cap is well below Chrome's 1 MB Native Messaging limit, leaving headroom for the surrounding JSON.

---

## 9. Filesystem boundaries

- `command.cwd` is normalized: `..` is resolved, symlinks are followed, the result must equal the project root or a path strictly under it. Any other path is rejected.
- The CLI itself only writes inside `.cgpt-bridge/` for its own state and logs.
- The CLI never reads outside the project root for its own purposes (commands are a separate matter — they are the user's authorized actions).
- Project root for v0.1 is the current working directory of the `cgpt` invocation. A future version may discover a `.cgpt-bridge/` ancestor automatically.

---

## 10. Native Messaging security

Manifest requirements:

- `name`: `com.cgpt_bridge.host` (reverse-DNS style).
- `type`: `stdio`.
- `allowed_origins`: a single string `"chrome-extension://<our-extension-id>/"`. **No wildcards.** No additional origins.
- `path`: absolute path to the installed Rust host binary. The installer must verify the file exists and is executable before writing the manifest.

Host runtime requirements:

- Reads 4-byte little-endian length, then a body of that exact length. Body sizes above the configured cap (default 1 MB) are dropped with a structured error response; the host does not allocate the buffer until the size is validated.
- Writes length-prefixed JSON to stdout only.
- Logs only to stderr. Any non-protocol write to stdout is a critical bug and must be impossible (e.g. `println!` is banned in the host; only the framing function writes to stdout).
- On JSON parse errors, the host sends `{"type":"error","code":"bad_json","detail":"..."}` and continues reading the next frame.
- The host does not exec other processes. Commands are run by the CLI, not the host.

---

## 11. Extension permissions

Manifest V3 manifest must declare only what is needed:

- `permissions`:
  - `nativeMessaging` (required to call `connectNative`).
  - `tabs` (required to find the active ChatGPT tab and message it).
  - `scripting` is **not** requested; the content script is declared statically in the manifest for `https://chatgpt.com/*`. (If a future version needs dynamic injection, this requirement is revisited.)
- `host_permissions`:
  - Exactly `"https://chatgpt.com/*"`. Nothing else.
- `content_security_policy.extension_pages`:
  - `script-src 'self'; object-src 'self'`. No `unsafe-eval`, no `unsafe-inline`, no remote `script-src`.
- `background.service_worker`: a single bundled file. No remote imports.
- No `web_accessible_resources` unless required.
- No `externally_connectable` to web pages.
- No analytics, telemetry, or remote code.

The content script:

- Runs at `document_idle` and only on `https://chatgpt.com/*`.
- Does no work until a message arrives from the service worker.
- Does not read across origins. Does not touch other tabs. Does not modify navigation.

---

## 12. Clipboard policy

The clipboard is not used by any component of `cgpt-bridge` for prompt insertion or response capture in v0.1. The content script writes the composer text via DOM property + input event dispatch. This avoids:

- Overwriting the user's clipboard contents.
- Side-channel leakage to clipboard watchers.
- Spurious paste UI in some apps.

---

## 13. Future sandboxing ideas (not in v0.1)

Listed for the roadmap, not implemented:

- Run confirmed commands inside a per-project sandbox (macOS `sandbox-exec` profile, Linux user namespaces or `bwrap`).
- An `--auto-readonly` mode that auto-approves commands classified as `read_only` by the local classifier (still showing a one-line notice). The classifier must be hardened further before this mode is offered.
- Structured file edits (patch application) as a first-class command kind, so file changes can be reviewed as diffs instead of free-form shell.
- Per-project allowlists of approved command shapes for repeated tasks.
- A second pair of eyes mode where a second model reviews the proposed command before the user sees it.

---

## 14. What v0.1 intentionally does not support

- **No silent auto-run by default.** A keypress is required unless the user
  explicitly passes `--auto-readonly` or `--yolo` (see §3).
- **No silent retries** of denied commands.
- **No remote control** of `cgpt`. There is no network listener.
- **No batch mode.** One project, one session, one user at the keyboard.
- **No reading or exporting of past ChatGPT conversations.**
- **No background polling** of the ChatGPT page.
- **No multi-tab orchestration.**
- **No installation of additional Chrome permissions** beyond `nativeMessaging`, `tabs`, and the single host permission.

The system is intentionally narrow. The narrower it is, the easier the safety story is to audit.
