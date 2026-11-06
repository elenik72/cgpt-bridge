# cgpt-bridge — Security (v0.1)

This document defines the v0.1 security posture: threat model, trust boundaries, confirmation model, command-execution risks, prompt-injection defenses, secret handling, redaction, filesystem boundaries, Native Messaging hardening, extension hardening, the v0.1 denylist, and what v0.1 intentionally does not support.

There are two paths from ChatGPT to your machine, and they are protected differently:

- **CLI path** (`cgpt agent`). The goal is to make the default path safe: a careful user, following the on-screen confirmation prompts, should not be silently harmed by anything ChatGPT proposes or by content that ChatGPT echoes from files, logs, or the web. The keypress is the main protection.
- **Gateway path** (`cgpt-gateway` for OpenCode). OpenCode runs the tools, and with its default settings it runs them without asking. Without `"bash": "ask"` (and `"edit": "ask"`) in OpenCode's `permission` settings, the denylist is the only protection, and it is a heuristic, not a sandbox (§15).

Every guarantee below is one of two kinds:

- **Construction**: enforced by how the code is built, whatever the input (a check on a value, a bound on a wait, a permission that is not requested).
- **Heuristic**: pattern matching on text. It catches the shapes it knows and misses others. It is not designed to stand up to a model that is trying to get around it.

## Guarantees at a glance

| Guarantee | Kind | Path | § |
| --- | --- | --- | --- |
| Only the `command` of one valid `cgpt-agent-response-v1` block can be run; prose and other code blocks never are | construction | CLI | 5 |
| Every command waits for a keypress, unless `--auto-readonly` or `--yolo` is passed | construction | CLI | 3 |
| `--auto-readonly` runs only commands the classifier calls read-only that also pass the narrow auto-run list (15 reading commands with whitelisted flags; no git, version queries or `ps`), whose programs lie outside the project and whose every named path is inside the project root, at most 20 in a row. Use it only on repositories you trust | heuristic: the classifier and the list read the command's text; the program and path checks then ask the file system, and the cap counts commands | CLI | 3 |
| OpenCode runs tool calls without asking unless you set its `permission` to `ask` | — (OpenCode's behaviour) | gateway | 15 |
| Denylist hard blocks | heuristic | CLI and gateway | 6 |
| Local risk class shown next to the declared one | heuristic | CLI | 3, 6 |
| Control characters cannot repaint the panel: they are rejected in commands and escaped in model text. Unicode bidirectional and zero-width characters are rejected in commands (not in prose) | heuristic | CLI | 3, 5 |
| Once a `cgpt agent` session is bound to a chat, a reply from another chat is dropped and the session stops; the prompt of that turn has already reached the other chat | construction (detection after the fact) | CLI | 11 |
| `cwd` must resolve inside the project root | construction | CLI | 9 |
| Command records are written only inside `.cgpt-bridge/runs/<session>/` | construction | CLI | 9 |
| A command turn ends at most about 2 s after its timeout, even if background processes keep the output open | construction | CLI | 4 |
| Secret redaction before text goes to ChatGPT (and, for `cgpt agent`, before the task is saved under `.cgpt-bridge/`) | heuristic | CLI: command output and the text you give `ask` / `agent` (`--no-redact` turns the second off); gateway: all text it sends (images go as they are, §15) | 7 |
| The project folder's name is sent to ChatGPT (as the name of a project a new chat goes into, created in your account when missing) only when the redaction rules leave it unchanged; a CLI ask may open or create that project in your ChatGPT tab | heuristic (the redaction rules) | both | 11 |
| A tool call that would write the redaction marker over a real value is refused | construction | gateway | 15 |
| OpenCode's `execute` tool is hidden from ChatGPT unless `allow_tools` names it | construction | gateway | 15 |
| The gateway listens on `127.0.0.1` only and needs a bearer token | construction | gateway | 15 |
| Native host frame size caps; the host runs no processes | construction | both | 10 |
| Minimal extension permissions | construction (manifest) | both | 11 |

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
- Network-level attackers (the `cgpt` CLI and the native host open no network port; the optional OpenCode gateway listens on `127.0.0.1` only, behind a bearer token — see §15; TLS to ChatGPT is the browser's job).

---

## 2. Trust boundaries

Listed from **least trusted** to **most trusted**:

1. **ChatGPT output** (assistant text, fenced JSON blocks, tool calls). **Untrusted.** Parsed and validated locally. On the CLI path never executed without user confirmation, except in the opt-in modes of §3. On the gateway path, handed to OpenCode after the denylist check (§15).
2. **Inputs to ChatGPT** (files, logs, web pages the user pastes or pipes in, tool results). **Untrusted.** Treated as data that may attempt prompt injection.
3. **Chrome extension** (service worker + content script). **Semi-trusted.** Runs in the user's browser, minimal permissions, no remote code, no eval. Trust comes from the user installing it and from the manifest+code being auditable.
4. **Native Messaging host**. **Trusted as a thin pipe.** Validates message sizes and JSON shape; performs no policy decisions.
5. **`cgpt` CLI** (CLI path). **Trusted decision point.** Performs parsing, validation, classification, redaction of command output and of the text you give it, plan management, and user-facing confirmation.
6. **`cgpt-gateway`** (gateway path). **Trusted decision point without a user step.** Applies the denylist to tool calls and redacts all text it sends to ChatGPT (images go as they are, §15). It runs nothing itself and asks the user nothing.
7. **OpenCode** (gateway path). Runs the tool calls (shell, file reads and edits, web fetches) under its own `permission` settings; by default without asking.
8. **The user**. **Final authority.** On the CLI path the user approves or rejects every command (except in the modes of §3). On the gateway path only if OpenCode is set to ask.

The browser side is a transport for prompt text and assistant text; it must never gain the ability to run commands on the local machine.

---

## 3. User confirmation model (CLI path)

**Every command requires explicit user confirmation by default.** Two opt-in
flags relax this:

- `cgpt agent --auto-readonly <task>` — a command runs without a keypress only
  if all of this holds (otherwise it prompts, with the reason printed):
  - the **local** classifier marks it `read_only` (a heuristic, §6);
  - it passes the narrow auto-run list below, a separate check: the
    classifier's class and the panel do not change;
  - the program each stage runs, looked up in `PATH` as the shell does, is
    a file outside the project. An empty or relative `PATH` entry (`.`)
    counts from the command's folder. A program not found asks. `rg` asks
    while `RIPGREP_CONFIG_PATH` is set: its file may turn on flags that are
    off the list;
  - every word of the command that could name a path (operands, flag values
    such as `--file=x`, comma lists, also inside `bash -c '…'`) resolves,
    with every symlink followed, inside the canonical project root. A path
    that does not exist yet resolves through its nearest existing parent; a
    glob is matched against the directory it reads. A glob that matches a
    file whose name starts with `-` asks: the shell would hand that name to
    the program as a flag the list never saw. A glob that matches a secrets
    file (`cat .e*` with `.env` there, `cat secrets/*` with
    `credentials.json` inside) asks too; as in the shell, `*` does not match
    a name starting with `.` unless the pattern starts with `.`;
  - no word holds a shell expansion the runner cannot check: `$VAR`,
    `$(...)`, backticks, brace expansion `{a,b}`, `~user`;
  - fewer than 20 commands have been auto-approved in a row. The 21st prompts;
    a command you confirm by hand starts a new count.

  The confirmation panel is still printed so the run is auditable. When the
  mode is on, a banner at the start says so and that the output of auto-run
  commands is sent to ChatGPT after pattern-based redaction. Anything
  classified `write_local`, `network`, `destructive` or `unknown` still
  prompts. Denylist hits remain blocked.
- `cgpt agent --yolo <task>` — every non-denylisted command auto-approves
  regardless of risk class, with no path check and no cap. The confirmation
  panel is still printed and a loud `⚠  --yolo` line is emitted per command.
  Intended for disposable VMs or sandboxes where the user explicitly accepts
  the blast radius. The denylist still blocks dangerous patterns.

Both flags are off by default. Neither bypasses the denylist. `--auto-readonly`
only skips the `[r]un` keypress for the commands described above; `--yolo` also
ignores the risk class, the list, the program and path checks and the cap.

The auto-run list of `--auto-readonly`:

| Command | Flags that may run unasked |
| --- | --- |
| `ls` | `-1 -a -A -l -h -R -t -S -r -d -F -p -i -n -G`, `--color[=…]` |
| `cat` | `-n -b -s -v -e -t -A -E -T` |
| `head` | `-n N -c N -q -v` |
| `tail` | `-n N -c N -q -v -r` |
| `wc` | `-l -w -c -m -L` |
| `stat` | `-L -f -c FMT -t -x -s -l -n`, `--format`, `--printf`, `--terse` |
| `file` | `-b -i -I -L -h -k`, `--mime-type`, `--mime-encoding` |
| `grep` | `-r -n -i -w -x -c -l -L -h -H -o -v -E -F -G -P -s -q -I -O -p`, `-e PAT -f FILE -m N -A N -B N -C N`, `--include`, `--exclude`, `--exclude-dir`, `--color[=…]` |
| `rg` | `-i -s -S -w -x -n -N -l -c -F -o`, `-e PAT -g GLOB -t TYPE -T TYPE -A N -B N -C N -m N -d N`, `--max-depth`, `--files`, `--count`, `--json`, `--vimgrep`, `--heading`, `--no-heading`, `--sort`, `--no-config` |
| `find` | start options `-H -P`; paths; `-name -iname -path -ipath -regex -iregex -type -maxdepth -mindepth -size -mtime -mmin -newer -printf` with their value, `-empty -prune -print -print0 -ls -not ! -a -o -and -or ( )` |
| `tree` | `-a -d -f -i -s -h -D -F -C -n -p -u -g -J -X`, `-L N -I PAT -P PAT` (value in the next word), `--gitignore`, `--noreport`, `--dirsfirst`, `--charset` |
| `sort` | `-n -r -u -f -h -V -b -s -M -g -c -C -R`, `-k KEY -t SEP` |
| `echo` | any arguments |
| `pwd` | `-L -P` |
| `which` | program names only, no flags |

- Every stage of a pipeline or list (`|`, `;`, `&&`, `||`) must pass. A
  stage with `<`, `>` or `&` outside quotes asks. The list splits the
  command with the classifier's splitter, which does not read backslash
  escapes the way the shell does, so any backslash outside single quotes
  asks: write grep and rg patterns with `\` in single quotes
  (`grep 'a\.b' x`).
- The program is the first word exactly as written: a path (`./ls`), another
  case (`Cat`), an extension (`cat.com`), a wrapper (`env`, `xargs`,
  `bash -c`, `NAME=value`) or a leading `.`/`&` asks.
- Flags are compared with their case (`tree -L 2` runs, `tree -l` asks).
  Short flags are read one by one inside a cluster (`grep -rnS` asks for
  `-S`). Long flags count only by their full name: GNU tools accept
  abbreviations, the list does not. Every word up to `--` is checked, since
  GNU tools take flags after the operands too; after `--` come operands.
  Any flag not in the table asks.
- A word naming a known secrets location (`.env`, `.npmrc`, `.netrc`,
  `.git/`, keys, credentials files, the P2 list in
  `core/src/denylist/secrets.rs`) asks.
- `echo` may take any arguments only because no listed form reads the paths
  to open from stdin.
- Off the list, so they always ask: every `git` command, version queries
  (`node -v`, `cargo --version`, `node version`), `ps` and other process
  listings, `more`, `whoami`, `hostname`, `type`, `findstr`, and all
  PowerShell and cmd names (under the POSIX shell they mean any program of
  that name in `PATH`). Flags that ask include link following (`ls -L`,
  `grep -R`/`-S`, `find -L`/`-follow`, `rg -L`/`--follow`, `tree -l`),
  paths read from a file (`--files0-from`, `file -f`, `find -files0-from`,
  `tree --fromfile`), writing (`sort -o`/`-T`, `tree -o`/`-R`/`-H`,
  `file -C`, `find -fprint…`/`-fls`/`-delete`), running programs
  (`find -exec…`/`-ok…`, `rg --pre`/`--hostname-bin`/`-z`, `file -z`/`-Z`,
  `sort --compress-program`), hidden and ignored files for rg (`--hidden`,
  `-.`, `-u…`, `--no-ignore…`; recursive `grep -r` does run), and
  `tail -f`/`-F`, which never ends.
- The output is sent as ChatGPT asks (`send_output`), as for any other
  command.

Limits of `--auto-readonly`:

- The classifier is a heuristic, and some commands it counts as read-only
  can run code: git runs programs named in the repository's configuration,
  and some version queries run a file from the project or fetch a package.
  The panel still shows them as read-only; the auto-run list keeps them
  from running unasked. The classifier does not read backslash escapes
  either: a backslash before a quote, `;`, `|`, `&`, `<`, `>` or a newline
  outside single quotes makes the command `unknown` in the panel. Use
  `--auto-readonly` only on repositories you trust.
- The glob checks match names the way a default shell does. A login profile
  that turns on `dotglob` (bash) or `GLOB_DOTS`/`EXTENDED_GLOB` (zsh)
  changes what a glob expands to, and the checks do not see that.
- The program check reads the `PATH` of `cgpt` itself. The command runs in
  `$SHELL -lc`, whose profile may change `PATH` or define functions; a
  user's `RIPGREP_CONFIG_PATH` is caught, other tools' configuration files
  are not read.
- The path check covers the words of the command. It does not see paths a
  program finds by itself: links it meets while walking a directory, or
  paths it reads from a file or its configuration. The list keeps the flags
  that follow links or read paths from a file from running unasked, but
  `grep -r` still reads every file under the folder it searches, secrets
  files such as `.env` or `.git/config` included, and `find` and `tree` show
  their names; only the redaction (§7) stands between them and ChatGPT.
- The path check runs just before the command. A process already running as
  you could swap a link in between; that process could read the file itself,
  so this adds nothing for it. Hard links are not detected.

When the assistant proposes a command, the CLI renders a confirmation panel on stderr containing:

- The `command.id`.
- The resolved absolute `cwd`.
- The assistant's `description` and `expected_effect`.
- The assistant's declared `risk`.
- The CLI's **locally classified** risk.
- The declared `timeout_ms`. A value above the cap is clamped, and a separate line before the panel says so.
- The declared `send_output`.
- The exact command string. A command of several lines is shown with line numbers.
- Denylist hits (`blocks`) and warnings, each with its rule name.

Model-controlled text on the panel (and in the final message, in `cgpt replay` and in `cgpt last`) is printed with control characters escaped (`\x1b`, `\r`, `\u{9b}`; newline and tab stay), and continuation lines of a field are indented under a `┆` gutter, so the text cannot repaint the panel. Long lines are wrapped by the CLI itself with the gutter on every row, so padding cannot make the terminal start a row at a forged `command:` header. Unicode bidirectional marks and zero-width characters are not escaped in prose (right-to-left text and emoji sequences stay legal there). A `command` that contains any control character other than newline and tab, or any bidirectional or zero-width character, is rejected by the protocol validator before it reaches the panel (the CLI then asks ChatGPT once to repair its answer). `cgpt ask` prints the model's answer with control characters escaped the same way (`--copy` keeps the answer as received).

Then a prompt (type the letter, then Enter):

```
[r]un  [e]dit  [s]kip  [q]uit  >
```

- **`r`** — run as proposed.
- **`e`** — type a replacement command (one line). The edited command is re-classified before it runs; if it hits the denylist, the CLI shows it with its blocks and prompts again. The edited string is what is run and what is reported in `cgpt-command-result-v1.command`.
- **`s`** — skip; CLI emits a `user_rejected` result block.
- **`q`** — quit the loop; exit code 10.

There is no default "yes". Pressing Enter at the prompt re-prompts, it does not run the command. If stdin is not a terminal, the CLI does not run a proposed command that needs a keypress; it records a skip.

For commands flagged by the local denylist, the prompt is replaced by:

```
[BLOCKED — choose one] [e]dit  [s]kip  [q]uit >
```

The `r` option is not offered. The user must edit the command into something that does not trigger the denylist, or skip, or quit.

---

## 4. Command execution risks (CLI path)

The agent runner works on macOS and Linux. It is not ported to Windows, and the Windows bundle does not ship the CLI.

What the CLI guarantees by construction:

- **No silent execution by default.** Every command needs confirmation, unless `--auto-readonly` or `--yolo` is passed (§3).
- **Working directory.** `cwd` is canonicalized and must be the project root or a path under it (§9).
- **A turn always ends.** Every command has a `timeout_ms`, set by the assistant (required, > 0; values above 600 000 ms are clamped). There is no default. At the deadline the CLI sends SIGTERM to the shell it started and SIGKILL 0.5 s later. Only that shell gets the signals, not the processes it started.

  After the shell has exited or been killed, the CLI collects output for at most 1.5 s more and then returns the result. If background processes still hold the output pipes by then, the result's `stderr` ends with a note that output may be incomplete, and the user gets the same warning. A background process that keeps writing is stopped by SIGPIPE soon after (unless it ignores the signal). A background process that writes nothing keeps running.

  `duration_ms` and `timed_out` describe the shell: `duration_ms` runs to the shell's exit or kill and does not include the 1.5 s output wait.
- **Output flooding.** Each of stdout and stderr is captured up to 4 MiB; beyond that it is read and dropped. What is sent to ChatGPT is cut further (§8).

What the CLI does **not** guarantee:

- **Environment.** Commands run under `$SHELL -lc` (falling back to `/bin/sh`) with the user's full environment. Sensitive environment variables are the user's responsibility. The environment-dump rules of §6 are a heuristic with known gaps.
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

Defenses on the CLI path:

- The CLI **never** executes a command derived from prose or from non-protocol code blocks. Only a single valid `cgpt-agent-response-v1.command` field is eligible (construction).
- The CLI's validator does not look at `risk` declared by the assistant when deciding whether to block — it runs its own classifier (§6, heuristic).
- The user is shown both the declared and locally classified risk, side by side.
- The CLI's denylist (§6) is independent of assistant-provided metadata.
- The keypress itself cannot come from the model: it is read from the local terminal. Commands with control characters are rejected and model text is escaped (§3), so control characters cannot repaint the panel to show another command. Bidirectional and zero-width characters are rejected in commands too.

Defenses on the gateway path: the denylist on tool calls and redaction (§15). There is no keypress unless OpenCode is set to ask.

What we explicitly do not promise:

- We do not claim to detect every injection attempt. On the CLI path the defense is "the user approves every command" (unless `--auto-readonly` or `--yolo` is on), not "we classify intent correctly". The user can still be talked into approving a harmful command.

---

## 6. Denylist (v0.1 hard blocks)

The denylist is a **heuristic**: regular expressions and a tokenizer over the command text, applied by the CLI before the confirmation prompt and by the gateway to every `command` in ChatGPT's tool calls (§15). It is not a sandbox. It cannot see what a script, a file written in an earlier step, or a program's own configuration does when it runs.

The command text is split into stages at pipes, `&&`, `||`, `;`, newlines and subshells. Each rule is applied to each stage and to the whole command, so a blocked stage anywhere blocks the whole command. Wrappers (`bash -c '…'`, `sh -c`, `powershell -Command "…"`, `cmd /c`, `eval`, `iex`, `$(…)`) are unwrapped and their inner text is checked the same way; deeply nested wrappers are examined only up to a fixed depth. A command that hits no rule is not blocked: it gets a risk class (`read_only`, `write_local`, `network`, `destructive` or, when the classifier cannot tell, `unknown`) and prompts as usual.

v0.1 hard blocks (the command will not be offered `[r]un`):

- **Privilege escalation**: `sudo`, `su`, `pkexec`, `doas` as words of the command.
- **Recursive root/home destruction**: recursive `rm` of `/`, `/*`, a top-level directory, `~`, `$HOME`, `/Users/<name>` or `/home/<name>` (also with a trailing `/` or `/*`); the PowerShell and `cmd` forms (`Remove-Item -Recurse`, `rmdir /s`) of a drive root or the user profile.
- **Raw disk writes**: `dd` with `of=/dev/…`, `mkfs`, `mkfs.*`, `fdisk`, `parted`, `blkdiscard`, `wipefs`; macOS `diskutil` erase or partition subcommands and `newfs_*`; Windows `format`, `diskpart`, `Format-Volume`, `Clear-Disk` and similar.
- **Permission/ownership sweeps**: recursive `chmod` and `chown` of `/`.
- **Network-fetch-then-execute**: a download piped into a shell (`curl … | sh`, `wget -O- … | bash`, and equivalents), `bash -c "$(curl …)"` and similar, and on Windows a download and its execution in one command line; `mshta` or `regsvr32 /i:` with a URL.
- **Credential extraction**:
  - Paths under `~/.ssh/`, and `~/.aws/credentials`, `~/.aws/config`, `~/.config/gcloud/`, `~/.kube/config`, `~/.netrc`, `~/.pgpass`, with the home directory in its common spellings.
  - On Windows: `.ssh`, `.aws`, `.config\gcloud`, `.kube`, `.azure`, `.docker`, `.netrc`/`_netrc`, `.pgpass` under the user profile, and `gcloud`/`postgresql` under `%APPDATA%`; `cmdkey`, `vaultcmd`, `mimikatz`.
  - macOS Keychain: `security find-generic-password`, `security find-internet-password`, `security dump-keychain`.
  - `gh auth token`, `aws sts get-session-token`, `op read` / `op item get` / `op account token` (1Password CLI), `bw get` / `bw export` (Bitwarden CLI).
- **Environment dumps**: `env`, `printenv`, `set`, `export -p` or `compgen -e` with nothing after them in their stage, also when run through a runner (`docker exec`, `ssh`, `npm run`, …), a substitution or a wrapper; PowerShell `Get-ChildItem env:` and `[Environment]::GetEnvironmentVariables()`. `env VAR=val command …` is not a dump and is allowed.
- **`.env` reads**: `cat`, `less`, `more`, `head`, `tail` or `bat` of `.env`, `.env.<name>` or `<name>.env`, and `< .env` redirections; PowerShell `Get-Content`, `type`, `Select-String` and the like.
- **Browser cookie and login stores**: the store files `Cookies`, `Login Data`, `key4.db`, `cookies.sqlite`; on Windows any path into a Chrome, Edge, Brave, Vivaldi or Opera `User Data` folder or a Firefox profile.
- **Windows system settings**: deleting registry keys, `Set-ExecutionPolicy`, deleting shadow copies or backups, changing the boot configuration with `bcdedit`.
- **Commands that cannot be inspected**: PowerShell `-EncodedCommand`, and `Start-Process` of a shell with arguments the classifier cannot read.

Narrow exceptions to the base rules, for common development commands that the base rules blocked by mistake:

- `env` used as a name: a directory or virtual environment (`ls env`, `python3 -m venv env`, `rm -rf env`).
- Source files whose name merely contains "cookie" (`src/lib/cookies.ts`, `cargo test cookies`). The exact store names above stay blocked.
- Committed `.env` templates: `.env.example`, `.env.sample`, `.env.template`, `.env.dist`.

Each exception is decided for the whole command, not for one stage. It does not apply when the command also runs a shell or `eval`, holds an expansion whose value the classifier cannot see (`$…`, backticks, `%VAR%`, process substitution), or names a real `.env`, a browser store or a browser profile path anywhere else.

Warnings (offered as `[r]un` with a clear warning, but not hard-blocked):

- Recursive `rm` (`rm -rf <path>`) and PowerShell recursive removal.
- Any command ending in `&` (backgrounding a process).
- Output redirected to an absolute or home path (`> /tmp/foo`, `> ~/x`).
- Network egress commands (`curl`, `wget`, `nc`, `ssh`, `scp`, `rsync`, and the PowerShell web cmdlets).
- Long-running watchers (`tail -f`, `watch`, `npm run dev`, `yarn dev`) — flagged because they will likely hit the timeout.
- Any word that names a known secrets location (`.env` and `.env.*` except the templates, `.ssh`, SSH private keys by their default names (`id_rsa`, `id_ed25519`, ...), `.aws`, `.netrc`, `.npmrc`, `.pypirc`, `.git`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `credentials` files (`credentials.json` and similar exact names), `/proc/*/environ`, Chrome, Chromium, Firefox and Safari profiles and their store files (`Cookies`, `Login Data`, `key4.db`, ...), the macOS keychain): the command is a secrets read, whatever program reads it, and is never `read_only`. The rule sees only the words written in the command: paths built at run time (globs, variables) or found by the program itself (a recursive search) are not seen. A word that only excludes a location from a search (`grep --exclude-dir=.git`, `rg -g '!.env'`, find's `-prune` and `-not -path`, each within its own command) does not count. The list is `SECRET_LOCATIONS` in `core/src/denylist/secrets.rs`.

Known limitations. These gaps predate the current rules and are not closed:

- Environment and `.env` contents can still be read by commands other than the listed dump commands and readers.
- Some ways of reaching a browser store or profile are not caught.
- Commands that a shell reads from somewhere other than the command line itself are not inspected.
- The matcher works on the literal text; spellings the shell expands differently are not all seen.

This policy is intentionally not exhaustive. It will evolve.

---

## 7. Secret redaction

Redaction is a **heuristic**: a list of regular expressions. Where it applies:

- **CLI path**: command output (stdout and stderr) before it is sent back to ChatGPT in a `cgpt-command-result-v1`, and before it is stored in `runs/<session>/`. The text you give `cgpt ask` and `cgpt agent` (words, piped stdin, `--buffer`; for `agent` also the `--resume` / `--continue` text) is redacted once, as soon as it is collected and before `--editor` opens, so the redacted text is what goes to ChatGPT and what `cgpt agent` writes to `plan.jsonl`, `plan.md`, `session.json` and `runs/<session>/` (`meta.json`, `session.json`). `--no-redact` sends and stores the text as typed. One stderr line names the rules that fired (never the values): `cgpt: input redacted before sending (rules: …)`; nothing is printed when no rule fired. Text you save in `--editor` is not redacted again: you saw it. Not redacted: `--copy`, `cgpt last --copy` and `cgpt replay --copy`, which copy ChatGPT's answer, not your input. Redacting those would break code a person wants to paste, and a secret you sent is already a marker in the answer. Text that is not secret-shaped (a password in prose; a log line with more than 6 prefix fields is not looked through; the rest of the "Known limits" below) is not caught, and a redaction can hit ordinary code (`let token = "a".repeat(64);`): the stderr line tells you, and `--no-redact` sends it as it was. Do not paste data you must not give ChatGPT at all.
- **Gateway path**: all text the gateway sends to ChatGPT: the system text, user messages, tool results and file contents, before they are measured, cut or split into parts. Images are the exception: they are sent as they are, unredacted (§15).

Each match is replaced with `«redacted:<rule_name>»` (for a key-value pair or a header, only the value; the key or header name stays), and `<rule_name>` is added to `redactions_applied`.

v0.1 rules:

| Rule name | What it matches (informal) |
| --- | --- |
| `private_key_block` | `-----BEGIN (RSA \|EC \|OPENSSH \|DSA \|ENCRYPTED \|)PRIVATE KEY-----` through the matching `-----END … PRIVATE KEY-----` |
| `private_key_base64` | the same header base64-encoded (kubeconfig, Kubernetes secrets) and the base64 after it |
| `kube_client_key_data` | the value of `client-key-data:` |
| `pgp_private_key_block` | `-----BEGIN PGP PRIVATE KEY BLOCK-----` through `-----END PGP PRIVATE KEY BLOCK-----` |
| `anthropic_api_key` | `sk-ant-` and 20+ of `[A-Za-z0-9_-]`; runs before `openai_api_key` |
| `openai_api_key` | `sk-` or `sk-proj-` and 20+ of `[A-Za-z0-9_-]` |
| `github_token_classic` | `ghp_[A-Za-z0-9]{30,}` |
| `github_token_fine_grained` | `github_pat_[A-Za-z0-9_]{20,}` |
| `github_oauth` | `gho_[A-Za-z0-9]{30,}` |
| `github_app` | `(ghu\|ghs\|ghr)_[A-Za-z0-9]{30,}` |
| `aws_access_key_id` | `\b(AKIA\|ASIA)[0-9A-Z]{16}\b` |
| `gitlab_token` | `glpat-`, `gldt-`, `glrt-` and the other GitLab prefixes, 20+ characters, with a digit or a capital in the body |
| `google_api_key` | `AIza` and 35+ of `[A-Za-z0-9_-]` |
| `slack_token` | `xoxa-`, `xoxb-`, `xoxp-`, `xoxr-`, `xoxs-`, `xoxd-` and 10+ characters; `xoxc-`, `xoxe-`, `xoxo-` with a body that starts with a digit |
| `npm_token` | `npm_[A-Za-z0-9]{30,}` |
| `stripe_live_secret` | `sk_live_[A-Za-z0-9]{20,}` |
| `jwt_like` | `eyJ…` three dot-separated base64url segments |
| `generic_bearer_token` | `Authorization: Bearer <token>`, also as a JSON or YAML pair |
| `generic_basic_auth` | `Authorization: Basic <base64>`, also as a JSON or YAML pair |
| `cookie_header` | `Cookie:` / `Set-Cookie:` (also `Cookie=`, a quoted key, a PowerShell hashtable entry) whose value starts with a `name=` pair, anywhere in a line |
| `npmrc_auth` | `.npmrc` `_auth=` |
| `url_userinfo_password` | the password in `scheme://user:password@host` |
| `dotenv_secret_assignment` | `KEY=value` or `KEY = value` where the key names a secret; the value to the end of the line |
| `key_value_secret` | `"key": value` (anywhere) or `key: value` (YAML, at the start of a line) where the key names a secret |
| `netrc_password` | the password of a `.netrc` entry |

The `sk-`, `sk-ant-`, GitLab, Google and Slack prefixes count only at the start of a token, so `desk-booking-service-…` is not a key.

A key names a secret when one of its words is `secret`, `token`, `password`, `passwd`, `passphrase`, `apikey`/`api key` or `privatekey`/`private key` (`client_secret`, `secretAccessKey`, `X-Api-Token`, `password1`), or, in an assignment, when such a word is glued to another (`PGPASSWORD`, `GITHUBTOKEN`). Some secret-like key names and value shapes are not redacted; the tests in `core/src/redact.rs` are the authority on which. There is no separate rule for AWS secret access keys or GCP service-account keys: `aws_secret_access_key = …` is caught as an assignment, and a key file's `"private_key"` by `private_key_block` and `key_value_secret`.

Values that are left alone, because they read as code or are no secret: empty values, `null`, booleans, numbers, type names (`string`, `Optional[str]`), versions (`^9.0.0`, `workspace:*`), variables (`$VAR`, `${VAR}`, `{{ x }}`), a value that is already a marker, and some values that read as code.

Rules:

- Redaction is applied to both stdout and stderr.
- Redaction runs **before** truncation, so high-entropy strings aren't half-cut into something that no longer matches a pattern.
- `redactions_applied` is included in the result block so the assistant knows redactions happened.
- The patterns are built in. There is no setting to add or remove patterns.
- A redaction is not an admission that a secret was present, only that something looked like one.

Known limits (most are pinned by tests in `core/src/redact.rs`):

- A secret with no known prefix and no secret-named key (a password in prose, a random string under an innocent name) is not redacted.
- A known prefix is not recognised in every position or encoding it can appear in.
- Values that read as code, numbers, versions or variables are left alone, and some secret values have such a shape.
- Header and cookie values are recognised only in their usual forms.
- Redaction does not undo any transformation of the output (encoding, splitting, reordering).
- False hits: some long path segments, and some unspaced minified code, are redacted.

---

## 8. Output truncation (CLI path)

After redaction, output is truncated for transmission to ChatGPT based on the `send_output` value declared in `CommandRequest`:

- `summary` — keep first 2 KB of stdout and first 2 KB of stderr.
- `truncated` — keep first 16 KB and last 4 KB of stdout (with `«…N bytes elided…»` marker in the middle), same shape for stderr.
- `full` — keep up to a hard cap of 256 KB total across stdout+stderr. **The CLI downgrades `full` to `truncated`** if total exceeds the cap; `output_truncated` is set true in that case.

The 256 KB hard cap is well below Chrome's 1 MB Native Messaging limit, leaving headroom for the surrounding JSON. Before redaction, capture itself stops at 4 MiB per stream (§4).

---

## 9. Filesystem boundaries (CLI path)

- `command.cwd` is canonicalized: `..` is resolved, symlinks are followed, the result must equal the canonical project root or a path strictly under it. Any other path is rejected and reported to ChatGPT as a policy block (construction).
- `command.id` comes from the model and becomes part of a file name, so the protocol validator accepts only `[A-Za-z0-9_.-]{1,64}` without `..`. Any other id makes the answer invalid, and the CLI asks ChatGPT once to repair it. As a second layer, the record file name is built from the id with every character outside `[A-Za-z0-9_-]` replaced, and a path whose parent is not `runs/<session>/` is refused. Command records are therefore written only inside `.cgpt-bridge/runs/<session>/`, whatever the id (construction).
- The CLI keeps its own state only under `.cgpt-bridge/` in the project root (`plan.jsonl`, `plan.md`, `session.json`, `runs/<session>/`). `--editor` uses a temporary file in the system temp directory, and `--copy` writes the clipboard (§12).
- Commands are a separate matter: a command you approve can read and write wherever you can.
- Project root for v0.1 is the current working directory of the `cgpt` invocation. A future version may discover a `.cgpt-bridge/` ancestor automatically.

---

## 10. Native Messaging security

Manifest requirements:

- `name`: `com.cgpt_bridge.host` (reverse-DNS style).
- `type`: `stdio`.
- `allowed_origins`: a single string `"chrome-extension://<our-extension-id>/"`. **No wildcards.** No additional origins.
- `path`: absolute path to the installed Rust host binary. The installer must verify the file exists and is executable before writing the manifest.

Host runtime requirements:

- Reads 4-byte little-endian length, then a body of that exact length. Body sizes above the configured cap (default 1 MB) are dropped with a structured error response; the host does not allocate the buffer until the size is validated. For a frame from the extension the host reads and drops the body (keeping only its first KiB to find the `id`), fails the waiting caller with `oversize_frame` when the id is in that first KiB (always so for the extension's own frames) and keeps running, except that a length prefix over 64 MiB is taken as a desynced stream and ends the host, which Chrome restarts; the extension itself turns an answer too large for the frame into that same error before sending it.
- Writes length-prefixed JSON to stdout only.
- Logs only to stderr. Any non-protocol write to stdout is a critical bug and must be impossible (e.g. `println!` is banned in the host; only the framing function writes to stdout).
- A request from a client (CLI or gateway) that is not valid JSON or not a known request gets a `bad_request` error on that connection. A frame from the extension that the host does not understand fails the caller waiting on that frame's `id` at once with an `internal` error, instead of letting it run into its timeout. The reason in that error has every quoted string from the frame replaced by its length and is cut to 200 characters, so it cannot carry prompt or answer text. A frame with no waiting caller is logged and ignored. The host keeps reading in both cases.
- The host does not exec other processes. Commands are run by the CLI (or, on the gateway path, by OpenCode), not the host.

---

## 11. Extension permissions

Manifest V3 manifest must declare only what is needed:

- `permissions` (exactly these three):
  - `nativeMessaging` (required to call `connectNative`).
  - `storage` (`chrome.storage.session`: window id and tab ids of the gateway's own window and its pool of tabs, so a service-worker restart finds it again; cleared when the browser or the extension restarts. `chrome.storage.local`: the developer flag `devTestAsk` of the toolbar test, unset by default; the ids of the ChatGPT projects found (a cache); and the own window's record: its window id and the ids of its pool tabs (up to 8) with a random mark (a UUID). The gateway's asks write that mark into the own tab's page (its `sessionStorage` on chatgpt.com, which ChatGPT's own scripts can read too; it says nothing but "this is the bridge's window"). After an extension reload or update the service worker asks the recorded tabs (the ids stay) for the mark; one recorded tab carrying it proves the window, and the other recorded tabs still in it on chatgpt.com stay in the pool. After a browser start (a restored window has new ids) it asks, once, the tabs of the normal windows whose tabs are all on chatgpt.com, and takes back the one window whose tabs carry the mark only when no tab of another window carries it and every loaded tab asked answered (a discarded or not yet loaded tab outside that window is not asked): a duplicated or reopened tab carries the mark too. Only tabs carrying the mark join the pool; the window's unloaded tabs are loaded in the background once and join if they carry it. Each page gets 1.5 s to answer. A missing or different mark, or any doubt, leaves every tab ordinary and drops the record, and the next gateway ask opens a new window; so does an own tab found closed or off chatgpt.com. While the recorded tab does not answer, a CLI ask does not use it either. CLI asks never write the mark. Limits: a copy of the gateway's tab (Duplicate, a reopened tab) that outlives the original, with no bridge request between the close and a browser restart, is taken back after that restart; and chatgpt.com's own scripts (or another extension on that site) can read, copy or remove the mark, so after a browser restart they could have a user's lone ChatGPT tab taken for the own tab. They already see every prompt the gateway sends, so that gives them little; after a reload the ids are checked as well.)
  - `scripting` (to put the extension's own `content.js` and `pageShim.js` into ChatGPT tabs that were open before the extension was installed, updated or reloaded; Chrome injects the manifest's content scripts only into pages loaded later. Used on install and update for every open `https://chatgpt.com/*` tab, and for a tab whose content script does not answer a request, once, before that request is sent again; that includes the `own_mark` question to the lone ChatGPT tabs after a browser start (see `host_permissions`). Only those two packaged files are injected, never code built at run time, and only into a tab that is on `https://chatgpt.com/` when checked (the host permission limits it there as well). When a tab still does not answer, the error names it by its title, cut to 60 characters, and its address without query or fragment: a chat title is the user's text, and it appears in the CLI's error and the gateway's error response and log.)
- `host_permissions`:
  - Exactly `"https://chatgpt.com/*"`. Nothing else. It is all the tab work needs, without the `tabs` permission: the extension lists only `https://chatgpt.com/*` tabs (to find the ChatGPT tab for a CLI ask; once after a browser start, while the own tab is not known again yet, to ask each ChatGPT tab that is alone in a normal window for the own window's mark, a read-only `own_mark` message; see `storage`), opens and drives the gateway's own tab, and messages them. Chrome shows it the address and title of chatgpt.com tabs only: a tab that left chatgpt.com has no address it can read, so it counts as gone (the next gateway ask opens a new window) and an error names it "(no address)". The toolbar test button works on the tab it is clicked in the same way, and sends the test prompt to ChatGPT only when the developer flag `devTestAsk` is `true` in `chrome.storage.local`, otherwise it only logs. Checked live without `tabs` and `activeTab` on 28.10.2026 (TASK-055).
- `content_security_policy.extension_pages`:
  - `script-src 'self'; object-src 'self'`. No `unsafe-eval`, no `unsafe-inline`, no remote `script-src`.
- `background.service_worker`: a single bundled file. No remote imports.
- No `web_accessible_resources` unless required.
- No `externally_connectable` to web pages.
- No analytics, telemetry, or remote code.

The content script:

- Runs at `document_idle` and only on `https://chatgpt.com/*` (or later, when the service worker injects it into a tab open from before the extension was installed, updated or reloaded). Running it again in a page that has it adds nothing: it listens once per page.
- Does no work until a message arrives from the service worker.
- Does not read across origins.

The page shim (`pageShim.ts`, MAIN world, `document_start`, `chatgpt.com` only) tees the answer stream to the content script and hides the tab's visibility state. It is dormant outside a bridge request, so the user's own ChatGPT use behaves normally. It wraps `fetch` once per page: injected into a page that already has one (an earlier version's included), it does nothing. The shim marks the page and its acknowledgements with its protocol version; the content script uses only a shim of its own version, and with any other one (kept by a tab across an extension update) answers from the page alone and logs one line.

Which tabs the extension touches:

- **CLI asks** run in the user's ChatGPT tab: the tab that answered the previous CLI ask, while it still shows the same chat; otherwise the active ChatGPT tab. The tab is never loaded or reloaded. A tab that shows a chat stays where it is: the ask continues that chat. A tab that shows a new chat (ChatGPT's start page, or a project's page with no message yet) is moved in the page when the CLI sends a project name (see the next item): the content script clicks that project's row in the user's sidebar (first pressing the list's "show more" control when the row is not in view) or, when the user has no project by that name, creates one in the user's ChatGPT account through ChatGPT's own dialog; the tab then shows the project's page (`/g/<id>/project`) and the new chat starts inside the project. This remembered tab applies to every CLI ask, a fresh `cgpt ask` or a new `cgpt agent` session included, not only to continuations; limiting it to continuations needs a protocol field and is deferred.
- **The project folder's name is sent to ChatGPT.** A new chat of the CLI or the gateway goes into a ChatGPT project named after the project folder: for the CLI the folder name of the repository root above the folder it runs in (else that folder), for the gateway that of OpenCode's workspace root (else its working directory). The name becomes a project in the user's ChatGPT account, visible in its sidebar and kept by ChatGPT, and the slug of every chat address in it. It is checked against the redaction rules (§7) first: a name the rules would change is not sent, and the chat goes to a plain new chat with a warning. In the home folder and at a disk root no name is sent. There is no option to leave the project out.
- **A `cgpt agent` session and its chat.** The CLI records the chat address of the first reply that has one in `.cgpt-bridge/runs/<session-id>/session.json` (and in `.cgpt-bridge/session.json` while the session is the latest one), and checks every later reply against it, by the chat id in the address as the gateway and the extension do (the same chat under a renamed project, or with a trailing slash or a query, is the same chat). A reply from another chat (or from a new chat without an address) is dropped before it is parsed: nothing is shown or run, the session is marked `chat_changed`, and `cgpt agent` exits with code 5 and a message that names both chats. This is a check after the fact: by the time the reply arrives, that turn's prompt (the task, or the previous command's redacted output) has already been sent to the other chat. Preventing it would need a protocol field that tells the extension which chat to expect (`expect_conversation`, not implemented). If the first reply had no address, a switch before turn 2 is not noticed: the chat of the first reply that has an address becomes the session's chat.
- **`--continue` and `--resume`** use the chat recorded for the session and stop on a change in the same way. The chat is stored per session, so `--resume <id>` of an older session checks against that session's own chat. A session with no stored address (it ran before addresses were stored, or none of its replies had one) prints one message saying so; its first reply is not checked and the chat it comes from becomes the session's chat.
- **Gateway asks** run only in the extension's **own tabs**: the service worker opens one separate window without focus (`chrome.windows.create`) and in it a pool of up to 8 tabs (`chrome.tabs.create` with `active: false`), keeps them from being discarded, and loads a session's chat in a pool tab with `chrome.tabs.update` when an in-page switch is not enough. A pool tab leaves the pool only when it is closed or moved out of that window; while it is off chatgpt.com (a sign-in page) it gets no ask. No tab of the own window is ever used for a CLI ask. In that tab the content script clicks *New chat* and the sidebar link of the session's chat, and, for a new chat, opens (or creates) the project named after the workspace folder, as for a CLI ask. The user's other tabs and windows are never navigated, reloaded or typed into, and the own tab is never used for a CLI ask. If the own window reads as signed out, a gateway turn is refused before anything is typed (so the prompt never goes to ChatGPT outside your account), and OpenCode gets HTTP 503 with a message to sign in in that window.

---

## 12. Clipboard policy

The extension does not use the clipboard: the content script writes the composer text via DOM property + input event dispatch, and reads the answer from the page. This avoids:

- Overwriting the user's clipboard contents.
- Side-channel leakage to clipboard watchers.
- Spurious paste UI in some apps.

The CLI touches the clipboard only when asked: `--buffer` reads it as the prompt (`pbpaste` on macOS; `wl-paste`, `xclip` or `xsel` on Linux; the text is redacted like piped stdin, §7), and `--copy` writes the final answer to it, as received and not redacted (§7). The gateway does not use the clipboard.

---

## 13. Future sandboxing ideas (not in v0.1)

Listed for the roadmap, not implemented:

- Run confirmed commands inside a per-project sandbox (macOS `sandbox-exec` profile, Linux user namespaces or `bwrap`).
- An OpenCode agent for `cgpt/chatgpt-web`, set up by the installer, that asks before shell commands, edits and web fetches.
- Structured file edits (patch application) as a first-class command kind, so file changes can be reviewed as diffs instead of free-form shell.
- Per-project allowlists of approved command shapes for repeated tasks.
- A second pair of eyes mode where a second model reviews the proposed command before the user sees it.

---

## 14. What v0.1 intentionally does not support

- **No silent auto-run by default on the CLI path.** A keypress is required unless the user
  explicitly passes `--auto-readonly` or `--yolo` (see §3). On the gateway path OpenCode
  runs tool calls without asking unless its `permission` settings say `ask` (§15).
- **No silent retries** of denied commands.
- **No remote control** of `cgpt`. The CLI has no network listener; the only one is the optional OpenCode gateway, on `127.0.0.1` with a bearer token (§15).
- **No batch mode.** One project, one session, one user at the keyboard.
- **No reading or exporting of past ChatGPT conversations.**
- **No background polling** of the ChatGPT page.
- **No multi-tab orchestration.** The gateway uses one tab of its own (§11).
- **No installation of additional Chrome permissions** beyond `nativeMessaging`, `storage`, `scripting` and the single host permission.

The system is intentionally narrow. The narrower it is, the easier the safety story is to audit.

---

## 15. OpenCode gateway

`cgpt-gateway` is an OpenAI-compatible server for OpenCode. It turns OpenCode's requests into ChatGPT messages, sends them through the native host and the extension, and turns the answers back into text or tool calls. OpenCode runs the tools; the gateway never runs a command itself.

**Without `"bash": "ask"` (and `"edit": "ask"`) in OpenCode's `permission` settings, the denylist is the only protection, and it is a heuristic, not a sandbox.** The installer does not set `permission`, and by default OpenCode runs shell commands and edits files without asking. With the defaults, a prompt injection that reaches ChatGPT can make OpenCode change files, run commands the denylist does not block, and fetch from the network, with no user step.

The installer adds an OpenCode agent `cgpt` (`agents/cgpt.md` next to `opencode.json`) whose first rule is `"*": ask`: every permission OpenCode checks asks unless a later rule names it. `bash`, `edit`, `webfetch`, `skill` and `grep` are set to `ask` by name too. `grep` asks because OpenCode checks it against the search pattern, not the path (`tool/plugin/grep.ts`; ripgrep runs with `--hidden`), so an allowed `grep` with `path: ".env"` would print the file. `read` (except `.env` files, which ask), `glob` (names only), `todowrite` and `question` are allowed. `execute` is denied (Code Mode off, below). So web searches, every MCP tool and every path outside the project (`external_directory`) ask as well.

What the catch-all does not cover:

- Tools that never ask OpenCode for permission. In 2.2.5 several of OpenCode's own `opencode_*` tools are such (renaming the session, moving it to another folder, creating a worktree, listing models); once Code Mode is off they would be offered directly and run without asking, so the agent hides all `opencode_*` tools (`"opencode_*": deny`, which removes a tool whose own rule is a plain deny) and sets the two that read MCP resources back to `ask`. Removing a worktree is checked as `edit` and stays (it asks). A custom tool (`.opencode/tool`) or a plugin's tool that does not call OpenCode's permission check runs without asking in this agent too.
- Case. OpenCode matches rules case-sensitively except on Windows. The agent asks for `.env`, `.env.*`, `.ENV` and `.ENV.*`; on a case-insensitive macOS volume a file named `.Env` (mixed case) is read without asking.

A catch-all is the only rule that reaches all MCP tools: OpenCode 2.1.4 and 2.2.5 check an MCP call under the action `<server>_<tool>` (`packages/core/src/tool/mcp.ts`), with no common prefix, and a skill under `skill` with the skill id as resource (`tool/plugin/skill.ts`); `mcp.use` and `skill.use` exist in 2.2 only as policy entries that remove a server or skill for everyone (`config/plugin/policy.ts`), not as per-agent permissions. Of all subagents it may start only `cgpt-sub` (`agents/cgpt-sub.md`, added by the installer too): the subagent rule is written under both names OpenCode has used, `task` and `subagent`, as "deny `*`, then allow `cgpt-sub`" (OpenCode matches the last rule that fits). OpenCode's own subagents, such as `general`, run with the user's usual permissions, which by default ask nothing, so `cgpt` may not start them. `cgpt-sub` uses `cgpt/chatgpt-web`, has the same rules (with `shell` named besides `bash`) and starts no subagents (`task` and `subagent` set to `deny`). OpenCode 2.1 and later read an agent file with the key `permission` as the old format and map `task`, `bash` and `write`/`patch` to `subagent`, `shell` and `edit` (`v1/config/migrate.ts`, `normalizeAction`; checked with OpenCode 2.1.4 and 2.2.5, which resolve both files to the same rules). A `cgpt-sub.md` the user wrote is not replaced, and the installer warns that `cgpt` may start it with its own permissions. It protects only while that agent is selected: the installer does not make it the default, the user switches to it with Tab, and the model `cgpt/chatgpt-web` picked in any other agent asks nothing. Because its rules come last, they also replace the user's own `permission` rules in that agent: a tool or command the user denies there is asked about, and a file read the user denies there (other than `.env` files) is allowed. File reads inside the project (except `.env` files) and `glob` do not ask in it. It is a convenience, not a lock.

Network surface:

- It binds `127.0.0.1:<port>` (default 8765) and never another interface: other machines cannot reach it.
- Every request needs `Authorization: Bearer <token>`; anything else gets 401. The token is 64 random hex characters made on first start and compared in constant time.
- Any local process of any user can connect to the port, so the token is the only gate. It lives in two files: the gateway config (`~/Library/Application Support/cgpt-gateway/config.json` on macOS, mode 600; `%APPDATA%\cgpt-gateway\config.json` on Windows) and OpenCode's `opencode.json` (`provider.cgpt.options.apiKey`). The macOS installer makes `opencode.json` and its `.bak` owner-only (600) as well. On Windows both files keep the ACL of the user profile, which other non-admin users cannot read by default.
- Browser pages cannot use it: they do not know the token, and the gateway sends no CORS headers.

What the gateway applies to OpenCode traffic (the same `core/` rules as the CLI):

- **Denylist on tool calls** (heuristic). Every `command` string in the arguments of every tool call ChatGPT makes goes through the §6 denylist before the call is handed to OpenCode, whatever the tool is called. If any call of an answer hits it, none of that answer's calls is handed over; ChatGPT gets the rule names and is asked for another way. If its next answer is refused too, OpenCode gets ChatGPT's answer as text and no calls. Only block rules count here: a command the CLI would merely warn about or classify as `unknown` is handed over.
- **What it does not check.** Only `command` strings are checked. File reads, writes and edits, web fetches and other tools are handed over as ChatGPT wrote them. The denylist cannot see what a file written by one call does when a later call runs it.
- **No `execute` tool by default** (construction). It runs code the denylist cannot see; ChatGPT is shown it only if `allow_tools` in the gateway config names it. OpenCode 2.1 and 2.2 (Code Mode) offer MCP tools and their own `opencode_*` tools only inside `execute`, so with `execute` hidden ChatGPT cannot reach them, while the plain tools (`shell`, `read`, `patch`, `webfetch` and the rest) still come directly (checked with 2.1.4 and 2.2.5). The agent `cgpt` denies `execute`: OpenCode 2.2.5 then sends those tools directly (each call asks in that agent), 2.1.4 leaves them out.
- **Redaction of all text sent to ChatGPT** (heuristic; images are the exception, below). User messages, tool results and file contents are redacted with the §7 patterns before they are measured, cut or split into parts. Redaction protects only what goes to ChatGPT; it does not limit what OpenCode does on your machine.
- **Redaction marker** (construction). A tool call whose arguments contain the redaction marker is refused, so ChatGPT cannot write the marker over a real secret.
- **Call-shaped text is never run** (construction). Calls come only from a valid `tool_calls` block. An answer without one that still looks like a call (another block label, bare JSON, other keys, a tool name followed by its arguments) only earns the one repair request; if the repair gives no block either, OpenCode gets the answer as text.
- **Images go to ChatGPT as they are** (no protection). Images in OpenCode's messages (a pasted screenshot, an image a tool read) are sent as files with the turn's message and are **not redacted**: redaction works on text and cannot see a secret in a picture. Do not give OpenCode screenshots of secrets. What limits them (construction): only PNG, JPEG, GIF and WebP from `data:` URLs, the type taken from the bytes; an http(s) URL is never fetched (the gateway connects to nothing but the bridge) and goes into the text as a note; other files (PDF and the like) become a note too. At most 700 KB and 10 images per message (Chrome's 1 MB limit for a native message): the newest go while they fit, every other one is replaced by a text note with its size. The log has one line per image sent, with its type and size, never its content. The extension uploads the files into the composer before it types the text, so a file reaches ChatGPT's servers even when the ask then fails before Send (the page then removes it from the composer). An extension that does not report `attachments: true` gets the text note `[изображение не отправлено: расширение устарело]` instead of the image.

What the gateway keeps on disk from ChatGPT's answers (in its data folder, next to `config.json` and `sessions/`: `~/Library/Application Support/cgpt-gateway/` on macOS, `%APPDATA%\cgpt-gateway\` on Windows):

- **`unparsed.jsonl`**, always on: one JSON line per answer the gateway could not read as meant, to find out why a call was missed — a failed repair round (with the answer the repair followed), final text that still mentions calls (the `tool_calls` label, an `"arguments"` key, an offered tool's name next to `"name"`), an answer that came twice over, and an answer whose stream and page text differ (at most 20 such records an hour, each with at most 1 Ki characters of the answer; the next one written says how many were dropped). Each line holds the time, the OpenCode session key, the bridge's ask id, the reason, the parser's problem, and the answer text **after redaction** (the §7 patterns, the same as everything sent to ChatGPT, applied before cutting) cut to 16 Ki characters (start and end kept). That is still answer text: file names, code and whatever ChatGPT wrote. At 2 MiB the file is renamed to `unparsed.jsonl.1` (an older `.1` is dropped), so the two take at most 4 MiB; when the rename fails (Windows: the file is open elsewhere) records go on into the file and the next record tries again.
- **`captures/<time>-<ask id>.sse`**, a debug aid, **off by default**: `debug_capture_turns: N` in the gateway config (at most 100; the gateway reads it at start) has the next N turns that send anything ask the extension for the event stream of every answer, and writes each one, redacted and whole, to that folder (the extension keeps at most 512 Ki characters of a stream; a capture is never cut, and one that would not fit the bridge's frame or the gateway's 2 MiB is not kept at all, with a log line). ChatGPT streams the answer in small pieces, so a secret may be split over several events, where no pattern would match it. A capture therefore holds every event with each text piece replaced by a reference (`«text OFF+LEN»`), and all the pieces joined once at the end, so the redaction sees the text whole; every other string of an event (ids, paths, roles, titles, ChatGPT's metadata) is kept as one whole value and redacted as such. Offsets count the joined text before redaction. A capture is never cut in the middle of the joined text: at the shim's cap the last, incomplete word of it is dropped as well, and a capture too big for the bridge's frame is left out whole (the gateway logs "capture dropped"). Pieces of two messages streamed at the same time would be joined interleaved, which could again split a secret; ChatGPT has not been seen doing that. It is the whole answer text and the stream's metadata. Only the config file turns it on: no request body, header or ChatGPT answer can, and the page shim keeps a stream only for an ask that asked for it. A page script from before captures (a tab that kept it across an update) does not acknowledge the request: the ask then gets no capture, and the extension logs that the page script is too old. The log says so at start; set the key back to 0 (or remove it) and restart when done.
- Files are created owner-only (mode 600, `captures/` 700) on macOS and Linux, like the config; on Windows they keep the ACL of the user profile. Delete them at any time (the gateway may run: it creates them again when needed); the uninstaller removes them with the config and sessions.

Local socket between the clients (CLI, gateway) and the native host:

- macOS and Linux: a Unix domain socket with mode 0600 in `$XDG_RUNTIME_DIR` or `$TMPDIR` (on macOS a per-user folder under `/var/folders` that other users cannot enter). Without either it goes to `/tmp/cgpt-bridge-<uid>/`, a private folder the host creates with mode 0700; the host and the clients refuse it if another user owns it or it is open to group or others. Clients connect only to a socket (not a symlink) owned by the current user, so they never talk to a socket planted by another user, and the host replaces a leftover socket only if it is its own.
- Windows: a named pipe whose DACL allows only the owner (`OW`) and SYSTEM. This is listener-side protection only. Another local user could create a pipe with the same name before the host does, and the client does not check who owns the server end, so it would talk to that impostor. That needs a second interactive user on the same machine.
- `OW` is the account that created the pipe. Run Chrome as administrator and that becomes the elevated token's owner (the Administrators group): the pipe is then open to every elevated process, and the non-elevated gateway cannot connect. Do not run Chrome as administrator.
