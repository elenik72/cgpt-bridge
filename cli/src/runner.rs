//! Shell command runner and confirmation UI.
//!
//! Wire-up between the agent loop and the local environment:
//!   1. Take a `CommandRequest` from the parsed agent response.
//!   2. Classify it locally (`denylist::classify`) — never trust the
//!      assistant-declared `risk`.
//!   3. Show the user a confirmation panel on stderr and read a single
//!      keystroke decision from stdin (TTY).
//!   4. On `run`, exec via `$SHELL -lc '...'` with a timeout, capture stdout
//!      and stderr, then redact + truncate per `send_output`.
//!   5. Build a `CommandResultV1` ready for `cgpt-command-result-v1`.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
use cgpt_bridge_protocol::agent::SendOutput;
use cgpt_bridge_protocol::agent::{CommandRequest, CommandResultV1};

use crate::denylist::{self, Classification, LocalRisk};
use crate::redact;
use crate::spinner::Phase;

/// What the user chose at the confirmation prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Run the command exactly as proposed.
    Run,
    /// Run the (user-edited) command.
    RunEdited { command: String },
    /// Skip this command; the loop will send back a `user_rejected` result.
    Skip,
    /// Quit the entire agent session.
    Quit,
}

/// Outcome of `prompt_and_run`. The agent loop converts this into a
/// `CommandResultV1` body and into a plan entry.
#[derive(Debug)]
pub enum RunOutcome {
    /// Command was executed (possibly with timeout). Result envelope is
    /// already redacted + truncated.
    Executed(CommandResultV1),
    /// User skipped; we still emit a result envelope so ChatGPT sees a
    /// uniform shape.
    UserRejected(CommandResultV1),
    /// User quit the session.
    Quit,
    /// Local policy blocked the command and the user did not edit into an
    /// allowed form.
    PolicyBlocked(CommandResultV1),
}

#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Project root used for cwd containment + the default cwd if the
    /// request specifies ".".
    pub project_root: PathBuf,
    /// Maximum timeout the user permits regardless of what the assistant
    /// requested. Should match the parsed `timeout_ms` (already clamped
    /// upstream).
    pub timeout_ms: u64,
    /// Force the confirmation UI to be non-interactive. Used by tests and
    /// when stdin is not a TTY (we conservatively refuse to run instead of
    /// silently auto-confirming).
    pub assume_no_tty: bool,
    /// Auto-approve commands whose **local** classification is `ReadOnly`
    /// and whose paths resolve inside the project root, at most
    /// `MAX_AUTO_APPROVALS_IN_A_ROW` in a row. Denylist hits remain blocked.
    /// Other risks still prompt.
    pub auto_readonly: bool,
    /// Auto-approve every non-blocked command without a keypress. The
    /// confirmation panel is still rendered so the run is auditable; the
    /// runner just records a synthetic `[yolo]` decision.
    pub yolo: bool,
}

/// Show the confirmation panel, read the user decision, and (if approved)
/// execute the command. The classifier output is computed inside so the UI
/// can show it next to the assistant-declared `risk`.
pub fn prompt_and_run(
    req: &CommandRequest,
    cfg: &RunConfig,
    auto_streak: &mut u32,
) -> io::Result<RunOutcome> {
    decide_and_run(req, cfg, auto_streak, &mut |blocked| {
        if cfg.assume_no_tty || !io::stdin().is_terminal() {
            eprintln!(
                "cgpt: stdin is not a TTY; refusing to run a proposed command without an interactive confirmation."
            );
            Ok(Decision::Skip)
        } else {
            read_decision(blocked)
        }
    })
}

/// `--auto-readonly` runs at most this many commands in a row without a
/// keypress; the next read-only command asks (AUD-012). Only a command the
/// user confirmed manually starts a new run.
pub const MAX_AUTO_APPROVALS_IN_A_ROW: u32 = 20;

/// The confirmation loop behind `prompt_and_run`. `ask` reads the user's
/// decision; tests pass a scripted one. `auto_streak` counts the commands
/// auto-approved in a row under `--auto-readonly`.
fn decide_and_run(
    req: &CommandRequest,
    cfg: &RunConfig,
    auto_streak: &mut u32,
    ask: &mut dyn FnMut(bool) -> io::Result<Decision>,
) -> io::Result<RunOutcome> {
    let classification = denylist::classify(&req.command);
    let cwd_resolved = match resolve_cwd(&cfg.project_root, &req.cwd) {
        Ok(p) => p,
        Err(e) => {
            // Out-of-root cwd is a hard refuse — treat as policy block.
            let result = build_policy_block_result(
                req,
                &cfg.project_root,
                format!("cwd containment: {}", e),
            );
            render_panel(req, &classification, &cfg.project_root, Some(&e));
            return Ok(RunOutcome::PolicyBlocked(result));
        }
    };

    render_panel(req, &classification, &cwd_resolved, None);

    let mut current_command = req.command.clone();
    let mut current_classification = classification;

    loop {
        let local_risk = current_classification.local_risk;
        let is_blocked = local_risk == LocalRisk::Blocked;
        let refusal = auto_refusal(
            cfg,
            &current_classification,
            &current_command,
            &cwd_resolved,
            *auto_streak,
        );
        if let Some(note) = &refusal {
            eprintln!("{}", note);
        }
        let auto_approve = auto_approves(cfg, local_risk, refusal.is_some());
        let decision = if auto_approve {
            if cfg.yolo {
                eprintln!("⚠  --yolo: auto-approving (local risk: {:?})", local_risk);
            } else {
                *auto_streak += 1;
                eprintln!("✓ --auto-readonly: auto-approving read-only command");
            }
            Decision::Run
        } else {
            ask(is_blocked)?
        };

        match decision {
            Decision::Run | Decision::RunEdited { .. } => {
                if let Decision::RunEdited { command } = &decision {
                    current_command = command.clone();
                    current_classification = denylist::classify(&current_command);
                    if current_classification.local_risk == LocalRisk::Blocked {
                        eprintln!("cgpt: edited command still hits a denylist rule; cannot run.");
                        render_panel_inline(&current_command, &current_classification);
                        continue;
                    }
                }
                if current_classification.local_risk == LocalRisk::Blocked {
                    let result = build_policy_block_result(
                        req,
                        &cwd_resolved,
                        format!(
                            "policy_blocked: {}",
                            current_classification
                                .blocks
                                .iter()
                                .map(|h| h.rule_name.to_string())
                                .collect::<Vec<_>>()
                                .join(",")
                        ),
                    );
                    return Ok(RunOutcome::PolicyBlocked(result));
                }
                if !auto_approve {
                    // The user confirmed this one by hand.
                    *auto_streak = 0;
                }
                let exec = execute_shell(&current_command, &cwd_resolved, cfg.timeout_ms)?;
                let result = build_executed_result(req, &cwd_resolved, &current_command, exec);
                return Ok(RunOutcome::Executed(result));
            }
            Decision::Skip => {
                let result = build_skip_result(req, &cwd_resolved, &current_command);
                return Ok(RunOutcome::UserRejected(result));
            }
            Decision::Quit => return Ok(RunOutcome::Quit),
        }
    }
}

// ---------------------------------------------------------------------------
// Confirmation UI
// ---------------------------------------------------------------------------

/// Make model-controlled text safe to print to a terminal: every C0 control
/// (except `\n` and `\t`), DEL and C1 control becomes a visible escape such
/// as `\x1b`, `\r` or `\u{9b}`, so what the panel shows is what is executed.
pub fn escape_control(s: &str) -> Cow<'_, str> {
    let needs = |c: char| match c {
        '\n' | '\t' => false,
        c => c.is_control(),
    };
    if !s.contains(needs) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if !needs(c) {
            out.push(c);
            continue;
        }
        match c {
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x80 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
        }
    }
    Cow::Owned(out)
}

/// Model text for the terminal, markdown printer included: repair double
/// escaping until nothing changes, and only then escape control characters.
/// `render::print_markdown` repairs once more; after a fixpoint that pass is a
/// no-op (escapes never add a real newline or a literal `\n`), so it cannot
/// turn a visible `\r` back into a CR. Escaping inside the printer instead
/// would mean touching render.rs, which is out of scope.
pub fn safe_text(raw: &str) -> String {
    let mut text = Cow::Borrowed(raw);
    while let Cow::Owned(next) = crate::render::repair_double_escaped(&text) {
        text = Cow::Owned(next);
    }
    escape_control(&text).into_owned()
}

/// Columns a character takes on the terminal, overestimated on purpose (a
/// row that wraps early is harmless, one that the terminal wraps is not): a
/// tab can jump up to 8, any non-ASCII character is counted as double width.
pub(crate) fn char_cols(c: char) -> usize {
    match c {
        '\t' => 8,
        c if c.is_ascii() => 1,
        _ => 2,
    }
}

/// Width the panel is wrapped to: the terminal's, or 80 when it is unknown.
fn panel_width() -> usize {
    termimad::crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
        .max(40)
}

/// `text` (one logical line) cut into rows that fit `avail` columns. Padding
/// a line with spaces could otherwise make the terminal start a new screen row
/// at a forged `command:` header, so no row may ever be wrapped by the terminal.
fn wrap_row(text: &str, avail: usize) -> Vec<String> {
    let mut rows = vec![String::new()];
    let mut used = 0;
    for c in text.chars() {
        let w = char_cols(c);
        if used + w > avail && used > 0 {
            rows.push(String::new());
            used = 0;
        }
        rows.last_mut().unwrap().push(c);
        used += w;
    }
    rows
}

/// A possibly multi-line model field as panel lines: the first line carries
/// the label, every other row (an injected newline or a wrapped long line) is
/// indented under a `┆` gutter so model text cannot pass for panel structure.
fn field_lines(prefix: &str, value: &str, width: usize) -> Vec<String> {
    let value = escape_control(value);
    let pad = " ".repeat(prefix.chars().count());
    let avail = width.saturating_sub(pad.len() + 3).max(16);
    let mut out = Vec::new();
    for l in value.split('\n') {
        for row in wrap_row(l, avail) {
            out.push(if out.is_empty() {
                format!("{}{}", prefix, row)
            } else {
                format!("{}┆ {}", pad, row)
            });
        }
    }
    out
}

/// Lines of a command for the panel: indented as before when it is a single
/// line, numbered when it spans several (so a hidden extra line is obvious).
/// A line too long for the terminal continues under a `┆` gutter.
fn command_lines(command: &str, width: usize) -> Vec<String> {
    let command = escape_control(command);
    let lines: Vec<&str> = command.split('\n').collect();
    let num_w = lines.len().to_string().len();
    let (indent, gutter) = if lines.len() == 1 {
        (6, 6)
    } else {
        (num_w + 7, num_w + 4)
    };
    let avail = width.saturating_sub(indent.max(gutter + 2) + 1).max(16);
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        for (j, row) in wrap_row(l, avail).into_iter().enumerate() {
            out.push(match (j, lines.len()) {
                (0, 1) => format!("      {}", row),
                (0, _) => format!("    {:>w$} | {}", i + 1, row, w = num_w),
                _ => format!("{}┆ {}", " ".repeat(gutter), row),
            });
        }
    }
    out
}

fn format_panel(
    req: &CommandRequest,
    c: &Classification,
    cwd: &Path,
    extra_err: Option<&str>,
) -> String {
    format_panel_with_width(req, c, cwd, extra_err, panel_width())
}

fn format_panel_with_width(
    req: &CommandRequest,
    c: &Classification,
    cwd: &Path,
    extra_err: Option<&str>,
    width: usize,
) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    line(String::new());
    line("──── proposed command ────────────────────────────────".into());
    for (prefix, value) in [
        ("  id:              ", req.id.as_str()),
        ("  cwd (resolved):  ", &cwd.display().to_string()),
        ("  description:     ", req.description.as_str()),
        ("  expected effect: ", req.expected_effect.as_str()),
    ] {
        for l in field_lines(prefix, value, width) {
            line(l);
        }
    }
    line(format!("  declared risk:   {:?}", req.risk));
    line(format!("  local risk:      {:?}", c.local_risk));
    line(format!("  timeout_ms:      {}", req.timeout_ms));
    line(format!("  send_output:     {:?}", req.send_output));
    line("  command:".into());
    for l in command_lines(&req.command, width) {
        line(l);
    }
    if !c.blocks.is_empty() {
        line("  blocks:".into());
        for h in &c.blocks {
            line(format!("    - [{}] {}", h.rule_name, h.note));
        }
    }
    if !c.warnings.is_empty() {
        line("  warnings:".into());
        for h in &c.warnings {
            line(format!("    - [{}] {}", h.rule_name, h.note));
        }
    }
    if let Some(extra) = extra_err {
        for l in field_lines("  ! ", extra, width) {
            line(l);
        }
    }
    line("──────────────────────────────────────────────────────".into());
    out
}

fn render_panel(req: &CommandRequest, c: &Classification, cwd: &Path, extra_err: Option<&str>) {
    let _ = write!(
        io::stderr().lock(),
        "{}",
        format_panel(req, c, cwd, extra_err)
    );
}

fn format_panel_inline(command: &str, c: &Classification) -> String {
    let mut out = String::from("  edited command:\n");
    for l in command_lines(command, panel_width()) {
        out.push_str(&l);
        out.push('\n');
    }
    out.push_str(&format!("  local risk:      {:?}\n", c.local_risk));
    if !c.blocks.is_empty() {
        out.push_str("  blocks:\n");
        for h in &c.blocks {
            out.push_str(&format!("    - [{}] {}\n", h.rule_name, h.note));
        }
    }
    out
}

fn render_panel_inline(command: &str, c: &Classification) {
    let _ = write!(io::stderr().lock(), "{}", format_panel_inline(command, c));
}

fn read_decision(blocked: bool) -> io::Result<Decision> {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    loop {
        if blocked {
            eprint!("[BLOCKED — choose one] [e]dit  [s]kip  [q]uit > ");
        } else {
            eprint!("[r]un  [e]dit  [s]kip  [q]uit > ");
        }
        io::stderr().flush().ok();
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            // EOF on stdin while interactive → treat as quit so we don't hang.
            eprintln!("(stdin closed; treating as quit)");
            return Ok(Decision::Quit);
        }
        let key = line.trim().to_lowercase();
        match key.as_str() {
            "r" if !blocked => return Ok(Decision::Run),
            "e" => {
                eprint!("edited command (one line, empty to cancel) > ");
                io::stderr().flush().ok();
                let mut edited = String::new();
                if reader.read_line(&mut edited)? == 0 {
                    return Ok(Decision::Quit);
                }
                let edited = edited.trim_end_matches(['\n', '\r']).to_string();
                if edited.trim().is_empty() {
                    continue;
                }
                return Ok(Decision::RunEdited { command: edited });
            }
            "s" => return Ok(Decision::Skip),
            "q" => return Ok(Decision::Quit),
            "" => {
                // Pressing Enter should NOT default to run. Re-prompt.
                eprintln!("(no default; please press r / e / s / q)");
            }
            other => {
                eprintln!(
                    "(unrecognized choice {:?}; please press r / e / s / q)",
                    other
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ExecOutcome {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    duration_ms: u64,
    timed_out: bool,
    /// A descendant still held the output pipes after the grace period, so
    /// the captured output may be incomplete.
    pipes_left_open: bool,
}

/// How long to keep collecting output after the shell has exited or been
/// killed. A descendant that outlives it can hold the pipes open for as long
/// as it likes; we stop waiting and say so (AUD-010).
const OUTPUT_GRACE: Duration = Duration::from_millis(1_500);

/// Appended to the result's stderr, after truncation, when the pipes were
/// still open after the grace period.
const BACKGROUND_NOTE: &str =
    "cgpt: background processes still hold the output pipes; output may be incomplete";

/// Shown to the user in the same case. Once the turn returns, the reader
/// threads stop at their next read, so a background process that keeps
/// writing to the pipe is killed by SIGPIPE on its next write.
const BACKGROUND_WARNING: &str = "background processes still hold the output pipes; \
output may be incomplete, and processes that keep writing will be stopped";

/// Above this many live reader threads the warning says so (see `LIVE_READERS`).
const READER_WARN_THRESHOLD: usize = 32;

/// Reader threads still alive in this process. A reader ends at EOF or at its
/// first read after the turn returned; a silent background process that
/// keeps the pipes open keeps its two readers (and fds) until it exits.
static LIVE_READERS: AtomicUsize = AtomicUsize::new(0);

/// Decrements `LIVE_READERS` when a reader thread ends, however it ends.
struct ReaderGuard;

impl ReaderGuard {
    fn new() -> Self {
        LIVE_READERS.fetch_add(1, Ordering::Relaxed);
        ReaderGuard
    }
}

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        LIVE_READERS.fetch_sub(1, Ordering::Relaxed);
    }
}

fn reader_leak_warning(live: usize) -> Option<String> {
    (live > READER_WARN_THRESHOLD).then(|| format!("{} background readers still open", live))
}

fn execute_shell(command: &str, cwd: &Path, timeout_ms: u64) -> io::Result<ExecOutcome> {
    execute_shell_with_grace(command, cwd, timeout_ms, OUTPUT_GRACE)
}

fn execute_shell_with_grace(
    command: &str,
    cwd: &Path,
    timeout_ms: u64,
    grace: Duration,
) -> io::Result<ExecOutcome> {
    // Prefer $SHELL with login flag so the user's PATH and env are loaded;
    // fall back to /bin/sh if $SHELL is unset or unusable.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let started = Instant::now();
    let mut child = match Command::new(&shell)
        .arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => Command::new("/bin/sh")
            .arg("-lc")
            .arg(command)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    };

    // Concurrent capture of stdout and stderr to avoid the pipe-buffer
    // deadlock that happens if the child fills one pipe while we are only
    // reading from the other.
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (otx, orx) = mpsc::channel::<Vec<u8>>();
    let (etx, erx) = mpsc::channel::<Vec<u8>>();
    let turn_over = Arc::new(AtomicBool::new(false));
    let _turn = TurnGuard(Arc::clone(&turn_over));
    spawn_reader(stdout, otx, Arc::clone(&turn_over));
    spawn_reader(stderr, etx, turn_over);

    let phase = Phase::start(format!("executing: {}", short_command(command)));

    let deadline = started + Duration::from_millis(timeout_ms);
    let mut timed_out = false;
    let exit_code = loop {
        match child.try_wait()? {
            Some(status) => break status.code(),
            None => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    kill_with_grace(&mut child);
                    break None;
                }
                thread::sleep(Duration::from_millis(40));
            }
        }
    };

    let duration_ms = started.elapsed().as_millis() as u64;
    let summary = if timed_out {
        format!("timed out after {}ms", duration_ms)
    } else {
        match exit_code {
            Some(0) => format!("exit 0 in {}ms", duration_ms),
            Some(code) => format!("exit {} in {}ms", code, duration_ms),
            None => format!("killed (no exit code) after {}ms", duration_ms),
        }
    };
    if timed_out || matches!(exit_code, Some(c) if c != 0) {
        phase.fail(summary);
    } else {
        phase.done(summary);
    }

    // Normally the pipes close with the shell and both channels disconnect
    // at once. If a descendant still holds them, wait only `grace` and keep
    // what was read. The receivers are dropped on return, so each detached
    // reader ends at its next read (see `drain_pipe`).
    let grace_deadline = Instant::now() + grace;
    let (stdout, stdout_done) = collect_until(&orx, grace_deadline);
    let (stderr, stderr_done) = collect_until(&erx, grace_deadline);
    let pipes_left_open = !(stdout_done && stderr_done);
    if pipes_left_open {
        eprintln!("⚠  {}", BACKGROUND_WARNING);
        if let Some(w) = reader_leak_warning(LIVE_READERS.load(Ordering::Relaxed)) {
            eprintln!("⚠  {}", w);
        }
    }

    Ok(ExecOutcome {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        exit_code,
        duration_ms,
        timed_out,
        pipes_left_open,
    })
}

fn short_command(command: &str) -> String {
    const MAX_CHARS: usize = 80;
    let one_line = command.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MAX_CHARS {
        one_line
    } else {
        let head: String = one_line.chars().take(MAX_CHARS - 1).collect();
        format!("{}…", head)
    }
}

/// Sets the "turn is over" flag when `execute_shell` returns, on any path.
/// The readers check it after each read; it costs no channel traffic.
struct TurnGuard(Arc<AtomicBool>);

impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    r: R,
    tx: mpsc::Sender<Vec<u8>>,
    turn_over: Arc<AtomicBool>,
) {
    let guard = ReaderGuard::new();
    thread::spawn(move || {
        let _guard = guard;
        drain_pipe(r, tx, &turn_over);
    });
}

/// Forward everything the child writes, chunk by chunk, so that output read
/// before a stuck pipe is not lost. The channel disconnects at EOF.
///
/// The thread also ends once the turn is over (`turn_over`, or the receiver
/// is gone): at its next read it drops the pipe, and a background process
/// that writes again gets SIGPIPE. That bounds the threads; the agent is not
/// meant to host long-lived processes.
fn drain_pipe<R: Read + Send + 'static>(
    mut r: R,
    tx: mpsc::Sender<Vec<u8>>,
    turn_over: &AtomicBool,
) {
    // Cap per-stream raw capture at 4 MiB to bound memory before redaction.
    const RAW_CAP: usize = 4 * 1024 * 1024;
    let mut buf = vec![0u8; 8 * 1024];
    let mut sent = 0usize;
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if turn_over.load(Ordering::Relaxed) {
                    break;
                }
                // Beyond the cap we keep reading so the child's pipe does
                // not block, but we stop accumulating and queue nothing: the
                // channel is only drained when the shell is done.
                if sent < RAW_CAP {
                    let take = n.min(RAW_CAP - sent);
                    sent += take;
                    if tx.send(buf[..take].to_vec()).is_err() {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
}

/// Gather chunks until the reader finishes (channel disconnects) or the
/// deadline passes. The flag says whether the stream was read to EOF.
fn collect_until(rx: &mpsc::Receiver<Vec<u8>>, deadline: Instant) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(part) => out.extend(part),
            Err(mpsc::RecvTimeoutError::Disconnected) => return (out, true),
            Err(mpsc::RecvTimeoutError::Timeout) => return (out, false),
        }
    }
}

#[cfg(unix)]
fn kill_with_grace(child: &mut Child) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let pid = child.id() as i32;
    unsafe { kill(pid, 15) }; // SIGTERM
                              // Give 500ms to clean up before SIGKILL.
    let kill_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < kill_deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = unsafe { kill(pid, 9) }; // SIGKILL
    let _ = child.wait();
}

/// Windows has no SIGTERM to ask politely with: terminate right away.
#[cfg(not(unix))]
fn kill_with_grace(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// --auto-readonly: where the paths really point (AUD-012)
// ---------------------------------------------------------------------------

/// Whether the runner runs the command without a keypress: never a blocked
/// one; any other under `--yolo`; under `--auto-readonly` a read-only one
/// that `auto_refusal` did not refuse.
fn auto_approves(cfg: &RunConfig, risk: LocalRisk, refused: bool) -> bool {
    risk != LocalRisk::Blocked
        && (cfg.yolo || (cfg.auto_readonly && risk == LocalRisk::ReadOnly && !refused))
}

/// Why `--auto-readonly` must ask about a command the classifier calls
/// read-only, or `None` when it may run it unasked (and when the mode does
/// not apply). The classifier only judges the text; this also holds the
/// command to the narrow auto-run list, asks the filesystem where each
/// program and path argument leads, and stops a long unattended run.
fn auto_refusal(
    cfg: &RunConfig,
    c: &Classification,
    command: &str,
    cwd: &Path,
    streak: u32,
) -> Option<String> {
    auto_refusal_in(cfg, c, command, cwd, streak, &AutoEnv::current())
}

/// `auto_refusal` with the environment given.
fn auto_refusal_in(
    cfg: &RunConfig,
    c: &Classification,
    command: &str,
    cwd: &Path,
    streak: u32,
    env: &AutoEnv,
) -> Option<String> {
    if cfg.yolo || !cfg.auto_readonly || c.local_risk != LocalRisk::ReadOnly {
        return None;
    }
    if streak >= MAX_AUTO_APPROVALS_IN_A_ROW {
        return Some(format!(
            "cgpt: --auto-readonly: {} commands auto-approved in a row; confirm this one to go on.",
            streak
        ));
    }
    let why = match denylist::auto_run_programs(command) {
        Err(why) => why,
        Ok(programs) => match cfg.project_root.canonicalize() {
            Ok(root) => program_refusal(&programs, env, cwd, &root)
                .or_else(|| path_outside_root(command, cwd, &root))?,
            Err(e) => format!("cannot canonicalize the project root: {}", e),
        },
    };
    Some(format!(
        "cgpt: --auto-readonly: {}; confirm it to run.",
        escape_control(&why)
    ))
}

/// What `--auto-readonly` reads from the runner's environment. The command
/// runs in `$SHELL -lc`, whose profile may still change `PATH`.
struct AutoEnv {
    /// `PATH`, where the shell looks up each program.
    path: Option<OsString>,
    /// `RIPGREP_CONFIG_PATH` is set: rg reads flags from that file.
    ripgrep_config: bool,
}

impl AutoEnv {
    fn current() -> Self {
        AutoEnv {
            path: std::env::var_os("PATH"),
            ripgrep_config: std::env::var_os("RIPGREP_CONFIG_PATH").is_some(),
        }
    }
}

/// Why a program of the list may not be what it seems: its file is in the
/// project, or not found at all, or rg would read flags from the user's
/// config file, which may turn on ones off the list.
fn program_refusal(programs: &[&str], env: &AutoEnv, cwd: &Path, root: &Path) -> Option<String> {
    if env.ripgrep_config && programs.contains(&"rg") {
        return Some(
            "RIPGREP_CONFIG_PATH is set, and its file may turn on rg flags off the list"
                .to_string(),
        );
    }
    for name in programs {
        let Some(file) = find_program(name, env.path.as_deref(), cwd) else {
            return Some(format!("`{}` is not found in PATH", name));
        };
        match file.canonicalize() {
            Ok(real) if !real.starts_with(root) => {}
            Ok(real) => {
                return Some(format!(
                    "`{}` resolves to {}, inside the project",
                    name,
                    real.display()
                ))
            }
            Err(e) => return Some(format!("`{}` cannot be resolved: {}", name, e)),
        }
    }
    None
}

/// The file the shell runs for `name`: the first file of that name in a
/// `PATH` entry that it can run (on Unix, one with an exec bit; the shell
/// skips the others). An empty or relative entry counts from `cwd`, as in
/// the shell.
fn find_program(name: &str, path: Option<&OsStr>, cwd: &Path) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|dir| cwd.join(dir).join(name))
        .find(|file| runnable(file))
}

fn runnable(file: &Path) -> bool {
    let Ok(meta) = file.metadata() else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// How deep the inner text of `bash -c '...'` and the like is split again
/// (the classifier unwraps as deep).
const MAX_WORD_DEPTH: usize = 8;

/// The first word of `command` that names, or may name, something outside
/// `root` once the shell has expanded it and every symlink is followed, or
/// a glob that expands to a flag-like or secrets file name; `None` if all
/// stay inside. Relative paths are taken from `cwd`. Every
/// word is checked, flags and patterns included: a word that names nothing
/// on disk resolves through its nearest existing ancestor, so it stays
/// inside, and one that happens to name a link out of the project only costs
/// a question.
fn path_outside_root(command: &str, cwd: &Path, root: &Path) -> Option<String> {
    let mut words = Vec::new();
    collect_words(command, 0, &mut words);
    for w in &words {
        if w.expands {
            return Some(format!(
                "`{}` uses a shell expansion that cannot be checked",
                w.text
            ));
        }
        for cand in path_candidates(&w.text) {
            if let Some(why) = candidate_outside(cand, w.globs, cwd, root) {
                return Some(why);
            }
        }
    }
    None
}

/// A word as a POSIX shell sees it, quotes removed, with whether an
/// unquoted part expands (`$`, backtick, `{`, zsh `=cmd`) or globs.
#[derive(Default)]
struct ShellWord {
    text: String,
    expands: bool,
    globs: bool,
}

/// Words of `text`, and again the words inside every word that holds more
/// than one (the script of `bash -c '...'`).
fn collect_words(text: &str, depth: usize, out: &mut Vec<ShellWord>) {
    for w in shell_words(text) {
        if depth < MAX_WORD_DEPTH
            && w.text
                .contains(|c: char| c.is_whitespace() || is_word_break(c) || "'\"\\".contains(c))
        {
            collect_words(&w.text, depth + 1, out);
        }
        out.push(w);
    }
}

fn is_word_break(c: char) -> bool {
    ";|&()<>".contains(c)
}

/// POSIX word splitting with quote removal. Unquoted operators end a word.
/// An unquoted `{` counts as an expansion: brace expansion joins the group
/// with the text around it (`o{u,}t`), which is not checked.
fn shell_words(s: &str) -> Vec<ShellWord> {
    let mut words = Vec::new();
    let mut cur = ShellWord::default();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('\''), '\'') => quote = None,
            (Some('\''), _) => cur.text.push(c),
            (Some(_), '"') => quote = None,
            (Some(_), '\\') => match chars.peek() {
                // A line continuation disappears.
                Some('\n') => {
                    chars.next();
                }
                Some(&n) if matches!(n, '$' | '`' | '"' | '\\') => {
                    cur.text.push(n);
                    chars.next();
                }
                _ => cur.text.push('\\'),
            },
            (Some(_), _) => {
                let expands = c == '`'
                    || (c == '$'
                        && chars
                            .peek()
                            .is_some_and(|&n| n.is_alphanumeric() || "_{(@*#?$!-".contains(n)));
                cur.expands |= expands;
                cur.text.push(c);
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                match chars.next() {
                    Some('\n') | None => {}
                    Some(n) => cur.text.push(n),
                }
                in_word = true;
            }
            (None, c) if c.is_whitespace() || is_word_break(c) => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, _) => {
                // `$'\x2e'` and `$"..."` expand too.
                cur.expands |= c == '`'
                    || (c == '$' && chars.peek().is_some_and(|n| !n.is_whitespace()))
                    || c == '{'
                    || (c == '=' && !in_word);
                cur.globs |= matches!(c, '*' | '?' | '[');
                cur.text.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

/// The path strings a word may stand for: itself, a flag's value
/// (`--file=x`, `-Path:x`) and the elements of a comma list.
fn path_candidates(word: &str) -> Vec<&str> {
    let mut whole = vec![word];
    if let Some((_, v)) = word.split_once('=') {
        whole.push(v);
    }
    if word.starts_with('-') {
        if let Some((_, v)) = word.split_once(':') {
            whole.push(v);
        }
    }
    let mut out = Vec::new();
    for w in whole {
        out.push(w);
        if w.contains(',') {
            out.extend(w.split(','));
        }
    }
    out
}

fn candidate_outside(cand: &str, globs: bool, cwd: &Path, root: &Path) -> Option<String> {
    if cand.is_empty() {
        return None;
    }
    let path = if cand == "~" || cand.starts_with("~/") {
        match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(cand[1..].trim_start_matches('/')),
            None => return Some(format!("`{}` needs $HOME, which is not set", cand)),
        }
    } else if cand.starts_with('~') {
        return Some(format!("`{}` names another user's home", cand));
    } else {
        cwd.join(cand)
    };
    let outside = |p: Option<PathBuf>| match p {
        Some(p) if p.starts_with(root) => None,
        Some(p) => Some(format!(
            "`{}` resolves to {}, outside the project root",
            cand,
            p.display()
        )),
        None => Some(format!("`{}` cannot be resolved", cand)),
    };
    let is_glob = |s: &str| s.contains(['*', '?', '[']);
    if !(globs && is_glob(cand)) {
        return outside(resolve_path(&path));
    }
    // A glob: the shell picks names in the parent directory, so every entry
    // it may pick must stay inside.
    let (dir_part, pattern) = cand.rsplit_once('/').unwrap_or(("", cand));
    if is_glob(dir_part) || pattern.is_empty() {
        return Some(format!("`{}` globs in a directory part", cand));
    }
    let parent = match path.parent() {
        Some(p) => resolve_path(p),
        None => None,
    };
    if let Some(why) = outside(parent.clone()) {
        return Some(why);
    }
    let Ok(entries) = std::fs::read_dir(parent.unwrap_or_default()) else {
        return None; // no directory, nothing matches
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if glob_matches(pattern, &name) {
            // The program gets the expanded name: one that starts with `-`
            // is read as a flag the auto-run list never saw. Behind a
            // directory part (`src/*`) the word starts with that part.
            if dir_part.is_empty() && name.starts_with('-') {
                return Some(format!(
                    "`{}` matches the file `{}`, which looks like a flag",
                    cand, name
                ));
            }
            // User decision 5 of 28.10 for the names a glob expands to. As
            // in the shell, a pattern matches a dot-name only when it starts
            // with `.` itself, so `cat *` does not ask for `.git`.
            let full = if dir_part.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", dir_part, name)
            };
            if (!name.starts_with('.') || pattern.starts_with('.'))
                && denylist::path_is_secret(&full)
            {
                return Some(format!(
                    "`{}` matches `{}`, a known secrets location",
                    cand, full
                ));
            }
            if let Some(why) = outside(resolve_path(&entry.path())) {
                return Some(why);
            }
        }
    }
    None
}

/// `*` and `?`, case-insensitively; a pattern with `[` matches every name.
/// Matching more names than the shell would only costs a question.
fn glob_matches(pattern: &str, name: &str) -> bool {
    if pattern.contains('[') {
        return true;
    }
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Where `path` points once every symlink is followed. A path that does not
/// exist yet resolves through its nearest existing ancestor. `None` when
/// that cannot be done safely: a dangling link, or `..` after a missing part.
fn resolve_path(path: &Path) -> Option<PathBuf> {
    let comps: Vec<Component> = path.components().collect();
    for i in (1..=comps.len()).rev() {
        let prefix: PathBuf = comps[..i].iter().collect();
        match prefix.canonicalize() {
            Ok(mut out) => {
                for c in &comps[i..] {
                    match c {
                        Component::Normal(n) => out.push(n),
                        Component::CurDir => {}
                        _ => return None,
                    }
                }
                return Some(out);
            }
            // It exists but cannot be followed: a dangling link.
            Err(_) if prefix.symlink_metadata().is_ok() => return None,
            Err(_) => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// cwd containment + result building
// ---------------------------------------------------------------------------

fn resolve_cwd(project_root: &Path, requested: &str) -> Result<PathBuf, String> {
    let root = project_root.canonicalize().map_err(|e| {
        format!(
            "cannot canonicalize project root {}: {}",
            project_root.display(),
            e
        )
    })?;

    let requested_path = if requested.trim() == "." || requested.trim().is_empty() {
        root.clone()
    } else {
        let p = Path::new(requested);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            project_root.join(p)
        }
    };

    let canon = requested_path.canonicalize().map_err(|e| {
        format!(
            "cannot canonicalize requested cwd {}: {}",
            requested_path.display(),
            e
        )
    })?;

    if canon == root || canon.starts_with(&root) {
        Ok(canon)
    } else {
        Err(format!(
            "requested cwd {} resolves outside project root {}",
            canon.display(),
            root.display()
        ))
    }
}

fn build_executed_result(
    req: &CommandRequest,
    cwd_used: &Path,
    command_used: &str,
    exec: ExecOutcome,
) -> CommandResultV1 {
    let red = redact::redact(&exec.stdout, &exec.stderr);
    let command = redact_command(command_used);
    let mut trunc = redact::truncate(&red.stdout, &red.stderr, req.send_output);
    if exec.pipes_left_open {
        // After truncation, so a Summary cut cannot drop it.
        if !trunc.stderr.is_empty() && !trunc.stderr.ends_with('\n') {
            trunc.stderr.push('\n');
        }
        trunc.stderr.push_str(BACKGROUND_NOTE);
        trunc.stderr.push('\n');
    }
    CommandResultV1 {
        version: 1,
        command_id: req.id.clone(),
        cwd: cwd_used.display().to_string(),
        command: command.stdout,
        exit_code: exec.exit_code,
        duration_ms: exec.duration_ms,
        timed_out: exec.timed_out,
        stdout: trunc.stdout,
        stderr: trunc.stderr,
        output_truncated: trunc.truncated,
        redactions_applied: merge_rules(red.rules_fired, command.rules_fired),
    }
}

/// The command text as it is stored and sent back: redacted like output. What
/// actually runs is `command_used`, which is never rewritten.
fn redact_command(command: &str) -> redact::Redaction {
    redact::redact(command, "")
}

fn merge_rules(mut a: Vec<String>, b: Vec<String>) -> Vec<String> {
    a.extend(b);
    a.sort();
    a.dedup();
    a
}

fn build_skip_result(req: &CommandRequest, cwd_used: &Path, command: &str) -> CommandResultV1 {
    let command = redact_command(command);
    CommandResultV1 {
        version: 1,
        command_id: req.id.clone(),
        cwd: cwd_used.display().to_string(),
        command: command.stdout,
        exit_code: None,
        duration_ms: 0,
        timed_out: false,
        stdout: String::new(),
        stderr: "user_rejected".to_string(),
        output_truncated: false,
        redactions_applied: command.rules_fired,
    }
}

fn build_policy_block_result(
    req: &CommandRequest,
    cwd_used: &Path,
    reason: String,
) -> CommandResultV1 {
    let command = redact_command(&req.command);
    CommandResultV1 {
        version: 1,
        command_id: req.id.clone(),
        cwd: cwd_used.display().to_string(),
        command: command.stdout,
        exit_code: None,
        duration_ms: 0,
        timed_out: false,
        stdout: String::new(),
        stderr: reason,
        output_truncated: false,
        redactions_applied: command.rules_fired,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cgpt_bridge_protocol::agent::{CommandKind, Risk};

    fn req(command: &str, send_output: SendOutput) -> CommandRequest {
        CommandRequest {
            id: "cmd_test".into(),
            kind: CommandKind::Shell,
            description: "x".into(),
            cwd: ".".into(),
            command: command.into(),
            expected_effect: "x".into(),
            risk: Risk::ReadOnly,
            timeout_ms: 5_000,
            send_output,
        }
    }

    const HOSTILE: &str = "a\u{1b}[2K\u{1b}[1A\u{1b}]0;pwned\u{7}b\rc\u{85}d\u{9b}31me\u{7f}f";

    fn assert_no_control(out: &str) {
        let bad: Vec<char> = out
            .chars()
            .filter(|c| c.is_control() && !matches!(c, '\n' | '\t'))
            .collect();
        assert!(
            bad.is_empty(),
            "control chars leaked: {:?} in {:?}",
            bad,
            out
        );
    }

    #[test]
    fn escape_control_makes_controls_visible() {
        assert_eq!(
            escape_control(HOSTILE),
            "a\\x1b[2K\\x1b[1A\\x1b]0;pwned\\x07b\\rc\\u{85}d\\u{9b}31me\\x7ff"
        );
    }

    #[test]
    fn escape_control_keeps_newline_tab_and_plain_text() {
        let s = "line1\n\tline2 — ünïcode ✓";
        assert!(matches!(escape_control(s), Cow::Borrowed(_)));
        assert_eq!(escape_control(s), s);
    }

    #[test]
    fn panel_shows_escapes_not_control_bytes() {
        let mut r = req("echo ok", SendOutput::Truncated);
        r.id = format!("id{}", HOSTILE);
        r.description = format!("desc{}", HOSTILE);
        r.expected_effect = format!("eff{}", HOSTILE);
        r.command = format!("echo {}", HOSTILE);
        let c = denylist::classify("ls");
        let out = format_panel(&r, &c, Path::new("/tmp/x\u{1b}[2J"), Some("err\r\u{1b}[H"));
        assert_no_control(&out);
        assert!(out.contains("\\x1b[2K"), "{}", out);
        assert!(out.contains("\\r"), "{}", out);
        assert!(out.contains("\\u{9b}"), "{}", out);
        assert!(out.contains("/tmp/x\\x1b[2J"), "{}", out);
        assert!(out.contains("err\\r\\x1b[H"), "{}", out);
    }

    #[test]
    fn panel_single_line_command_keeps_plain_indent() {
        let r = req("ls -la", SendOutput::Truncated);
        let out = format_panel(&r, &denylist::classify("ls -la"), Path::new("/p"), None);
        assert!(out.contains("  command:\n      ls -la\n"), "{}", out);
    }

    #[test]
    fn panel_multiline_command_is_numbered_and_keeps_tabs() {
        let r = req("set -e\n\techo hi\nls", SendOutput::Truncated);
        let out = format_panel(&r, &denylist::classify("ls"), Path::new("/p"), None);
        assert!(
            out.contains("  command:\n    1 | set -e\n    2 | \techo hi\n    3 | ls\n"),
            "{}",
            out
        );
    }

    #[test]
    fn panel_multiline_numbers_are_right_aligned() {
        let cmd = (1..=10)
            .map(|i| format!("l{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let r = req(&cmd, SendOutput::Truncated);
        let out = format_panel(&r, &denylist::classify("ls"), Path::new("/p"), None);
        assert!(out.contains("     1 | l1\n"), "{}", out);
        assert!(out.contains("    10 | l10\n"), "{}", out);
    }

    #[test]
    fn inline_panel_escapes_edited_command() {
        let out = format_panel_inline(&format!("rm x{}", HOSTILE), &denylist::classify("ls"));
        assert_no_control(&out);
        assert!(out.contains("\\x1b[2K"), "{}", out);
    }

    #[test]
    fn panel_prose_fields_cannot_forge_panel_lines() {
        let mut r = req("echo real", SendOutput::Truncated);
        r.description = "first\n  local risk:      ReadOnly\n  command:\n      echo fake".into();
        r.expected_effect = "x\n──────────────────────────────────────────────────────".into();
        let out = format_panel(&r, &denylist::classify("ls"), Path::new("/p"), None);
        // Only the real header lines start at the panel's own indentation.
        let count = |s: &str| out.lines().filter(|l| l.starts_with(s)).count();
        assert_eq!(count("  command:"), 1, "{}", out);
        assert_eq!(count("  local risk:"), 1, "{}", out);
        assert_eq!(count("─────"), 1, "{}", out); // only the real bottom border
        assert!(out.contains("      echo real\n"), "{}", out);
        // Forged lines are still there, but marked as continuation of the field.
        assert!(out.contains("┆   command:"), "{}", out);
        assert!(out.contains("┆ ────"), "{}", out);
    }

    /// Display columns of a panel row.
    /// Independent of `char_cols`: tab 8, CJK 2, everything else (box drawing,
    /// the gutter) 1.
    fn cols(row: &str) -> usize {
        row.chars()
            .map(|c| match c {
                '\t' => 8,
                '\u{4e2d}' => 2,
                _ => 1,
            })
            .sum()
    }

    #[test]
    fn panel_padded_prose_cannot_wrap_a_fake_command_onto_its_own_row() {
        let width = 80;
        // One logical line, padded so a terminal of this width would start a
        // new screen row exactly at the forged header and its fake command.
        let mut r = req("echo real", SendOutput::Truncated);
        r.description = format!(
            "x{}  command:{}      echo fake",
            " ".repeat(120),
            " ".repeat(60)
        );
        r.expected_effect = format!("{}  command:", "y".repeat(width - 2));
        let out = format_panel_with_width(
            &r,
            &denylist::classify("ls"),
            Path::new("/p"),
            Some(&format!("e{}  command:", " ".repeat(200))),
            width,
        );
        let count = |s: &str| out.lines().filter(|l| l.starts_with(s)).count();
        assert_eq!(count("  command:"), 1, "{}", out);
        assert_eq!(count("      echo fake"), 0, "{}", out);
        // No row is wider than the terminal, so nothing is soft-wrapped by it.
        for l in out.lines() {
            assert!(cols(l) < width, "row too wide ({}): {:?}", cols(l), l);
        }
        // Every row of a prose field after the first is under the gutter.
        for l in out.lines().filter(|l| l.contains("fake")) {
            assert!(l.contains('\u{2506}'), "{:?}", l);
        }
        // Nothing is lost by wrapping.
        let joined: String = out.lines().filter(|l| l.contains('\u{2506}')).collect();
        assert!(joined.contains("echo fake"), "{}", out);
    }

    #[test]
    fn panel_long_command_line_wraps_with_a_marker_not_a_bare_row() {
        let width = 60;
        let cmd = format!("ls{}  warnings:", " ".repeat(100));
        let r = req(&cmd, SendOutput::Truncated);
        let out =
            format_panel_with_width(&r, &denylist::classify("ls"), Path::new("/p"), None, width);
        let count = |s: &str| out.lines().filter(|l| l.starts_with(s)).count();
        assert_eq!(count("  warnings:"), 0, "{}", out);
        for l in out.lines() {
            assert!(cols(l) < width, "row too wide ({}): {:?}", cols(l), l);
        }
    }

    #[test]
    fn panel_wide_chars_and_tabs_count_as_wide_when_wrapping() {
        let width = 60;
        let mut r = req("ls", SendOutput::Truncated);
        r.description = "\u{4e2d}".repeat(60);
        let out =
            format_panel_with_width(&r, &denylist::classify("ls"), Path::new("/p"), None, width);
        for l in out.lines() {
            assert!(cols(l) < width, "row too wide ({}): {:?}", cols(l), l);
        }
    }

    #[test]
    fn panel_emoji_presentation_symbols_wrap_before_the_terminal_width() {
        // U+2614 (umbrella with rain) and U+1F004 (mahjong red dragon) render
        // two columns wide; the exact ranges must not matter.
        let width = 60;
        let mut r = req("ls", SendOutput::Truncated);
        r.description = format!("{}{}", "\u{2614}".repeat(40), "\u{1f004}".repeat(40));
        let out =
            format_panel_with_width(&r, &denylist::classify("ls"), Path::new("/p"), None, width);
        let real_cols = |l: &str| -> usize {
            l.chars()
                .map(|c| {
                    if matches!(c, '\u{2614}' | '\u{1f004}') {
                        2
                    } else {
                        1
                    }
                })
                .sum()
        };
        let rows = out
            .lines()
            .filter(|l| l.contains(['\u{2614}', '\u{1f004}']))
            .count();
        assert!(rows >= 4, "expected wrapping, got {} rows\n{}", rows, out);
        for l in out.lines() {
            assert!(
                real_cols(l) < width,
                "row too wide ({}): {:?}",
                real_cols(l),
                l
            );
        }
    }

    #[test]
    fn safe_text_reaches_a_fixpoint_before_escaping() {
        // Two levels of `\\n`, then a real CR.
        let raw = "a\\\\nb\\\\nc\\\\nd\rEVIL\u{1b}[2J";
        let shown = safe_text(raw);
        assert_eq!(shown, "a\nb\nc\nd\\rEVIL\\x1b[2J");
        // The markdown printer repairs once more; that must be a no-op.
        assert!(matches!(
            crate::render::repair_double_escaped(&shown),
            Cow::Borrowed(_)
        ));
        assert!(!shown.contains(['\r', '\u{1b}']));
    }

    #[test]
    fn safe_text_leaves_plain_text_alone() {
        assert_eq!(safe_text("hello\n\tworld"), "hello\n\tworld");
    }

    fn tmp_root() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let p = PathBuf::from(format!("/tmp/cgb-runner-test-{}-{}", pid, n));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    // Runs a POSIX shell command; the runner is not ported to Windows.
    #[cfg(unix)]
    #[test]
    fn ls_runs_and_captures_stdout() {
        let root = tmp_root();
        // Drop a known file so `ls` has something to show.
        std::fs::write(root.join("hello.txt"), "x").unwrap();
        let cfg = RunConfig {
            project_root: root.clone(),
            timeout_ms: 5_000,
            assume_no_tty: false,
            auto_readonly: false,
            yolo: false,
        };
        let r = req("ls", SendOutput::Truncated);
        let exec = execute_shell(&r.command, &root, 5_000).unwrap();
        assert_eq!(exec.exit_code, Some(0));
        assert!(exec.stdout.contains("hello.txt"));
        assert!(!exec.timed_out);
        assert!(!exec.pipes_left_open);
        let res = build_executed_result(&r, &root, &r.command, exec);
        assert_eq!(res.exit_code, Some(0));
        assert!(res.stdout.contains("hello.txt"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = cfg;
    }

    // Runs a POSIX shell command; the runner is not ported to Windows.
    #[cfg(unix)]
    #[test]
    fn timeout_fires_and_marks_timed_out() {
        let root = tmp_root();
        let exec = execute_shell("sleep 5", &root, 200).unwrap();
        assert!(exec.timed_out);
        assert!(
            exec.duration_ms < 2_000,
            "duration {} should be small",
            exec.duration_ms
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Kills the background `sleep` a test leaves behind, even when an
    /// assertion fails first, so no stray process outlives the test run.
    #[cfg(unix)]
    struct BgPid(PathBuf);

    #[cfg(unix)]
    impl BgPid {
        fn pid(&self) -> Option<i32> {
            std::fs::read_to_string(&self.0).ok()?.trim().parse().ok()
        }
    }

    #[cfg(unix)]
    impl Drop for BgPid {
        fn drop(&mut self) {
            extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            if let Some(pid) = self.pid() {
                unsafe { kill(pid, 9) };
            }
        }
    }

    // AUD-010: a descendant that outlives the shell and keeps the pipes open
    // must not keep the turn hanging past the timeout plus a short grace.
    #[cfg(unix)]
    #[test]
    fn timeout_returns_even_if_background_child_holds_pipes() {
        let root = tmp_root();
        let bg = BgPid(root.join("bg.pid"));
        let started = Instant::now();
        let exec =
            execute_shell("sleep 30 & echo $! > bg.pid; echo hi; wait", &root, 2_000).unwrap();
        let elapsed = started.elapsed();
        assert!(exec.timed_out);
        assert!(
            elapsed < Duration::from_secs(5),
            "returned after {:?}, expected timeout + short grace",
            elapsed
        );
        assert!(exec.stdout.contains("hi"), "stdout: {:?}", exec.stdout);
        assert!(exec.pipes_left_open);
        assert!(bg.pid().is_some(), "background child pid was not recorded");
        drop(bg);
        let _ = std::fs::remove_dir_all(&root);
    }

    // AUD-010: the shell exits at once but its background child keeps the
    // pipes open; the runner reports it instead of waiting for the child.
    #[cfg(unix)]
    #[test]
    fn background_child_after_normal_exit_is_reported_not_awaited() {
        let root = tmp_root();
        let bg = BgPid(root.join("bg.pid"));
        let started = Instant::now();
        let exec =
            execute_shell("sleep 30 & echo $! > bg.pid; echo started", &root, 10_000).unwrap();
        let elapsed = started.elapsed();
        assert!(!exec.timed_out);
        assert_eq!(exec.exit_code, Some(0));
        assert!(
            elapsed < Duration::from_secs(3),
            "returned after {:?}",
            elapsed
        );
        assert!(exec.stdout.contains("started"), "stdout: {:?}", exec.stdout);
        assert!(exec.pipes_left_open);
        drop(bg);
        let _ = std::fs::remove_dir_all(&root);
    }

    // Review finding 1: Summary mode keeps only the first 2 KiB of stderr,
    // so the note must be added after truncation, not to the raw stderr.
    #[cfg(unix)]
    #[test]
    fn background_note_survives_summary_truncation() {
        let root = tmp_root();
        let bg = BgPid(root.join("bg.pid"));
        let r = req("x", SendOutput::Summary);
        let exec = execute_shell_with_grace(
            "sleep 30 & echo $! > bg.pid; head -c 5000 /dev/zero | tr '\\0' x >&2",
            &root,
            10_000,
            Duration::from_millis(300),
        )
        .unwrap();
        let res = build_executed_result(&r, &root, &r.command, exec);
        assert!(res.output_truncated);
        assert!(
            res.stderr.contains("background processes"),
            "stderr was cut: {:?}",
            res.stderr.len()
        );
        drop(bg);
        let _ = std::fs::remove_dir_all(&root);
    }

    // Round 2: a command that floods output for the whole timeout keeps the
    // capture at the raw cap per stream.
    #[cfg(unix)]
    #[test]
    fn flooding_command_stays_within_the_raw_cap() {
        let root = tmp_root();
        let exec = execute_shell("yes; echo done", &root, 2_000).unwrap();
        assert!(exec.timed_out);
        assert!(!exec.stdout.is_empty());
        assert!(
            exec.stdout.len() <= 4 * 1024 * 1024,
            "stdout {} bytes",
            exec.stdout.len()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Review finding 3: past the threshold the warning names the count.
    #[test]
    fn reader_leak_warning_only_above_threshold() {
        assert_eq!(reader_leak_warning(READER_WARN_THRESHOLD), None);
        let w = reader_leak_warning(READER_WARN_THRESHOLD + 1).unwrap();
        assert!(w.contains("33 background readers still open"), "{}", w);
    }

    struct Endless;
    impl Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            buf.fill(b'x');
            Ok(buf.len())
        }
    }

    // Round 2: past the 4 MiB cap nothing may be queued, or a flooding command
    // grows the channel for as long as the shell runs.
    #[test]
    fn reader_queues_nothing_past_the_cap() {
        const CAP: usize = 4 * 1024 * 1024;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let flag = Arc::new(AtomicBool::new(false));
        let reader_flag = Arc::clone(&flag);
        thread::spawn(move || {
            drain_pipe(Endless, tx, &reader_flag);
            let _ = done_tx.send(());
        });
        let mut got = 0usize;
        while got < CAP {
            got += rx.recv().unwrap().len();
        }
        assert_eq!(got, CAP);
        // Give the reader time to flood if it were going to.
        let until = Instant::now() + Duration::from_millis(300);
        let mut extra = 0usize;
        while rx
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .is_ok()
        {
            extra += 1;
        }
        // Stop the reader before asserting so it cannot spin on after a failure.
        flag.store(true, Ordering::Relaxed);
        assert!(done_rx.recv_timeout(Duration::from_secs(5)).is_ok());
        assert_eq!(extra, 0, "chunks queued past the cap");
    }

    // Review finding 2: past the cap the reader must still notice that the
    // turn is over instead of draining the pipe forever.
    #[test]
    fn reader_stops_after_cap_when_receiver_is_gone() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let flag = Arc::new(AtomicBool::new(false));
        let reader_flag = Arc::clone(&flag);
        thread::spawn(move || {
            drain_pipe(Endless, tx, &reader_flag);
            let _ = done_tx.send(());
        });
        let mut got = 0usize;
        while got < 4 * 1024 * 1024 {
            got += rx.recv().unwrap().len();
        }
        // The receiver stays alive: the flag alone must stop the reader.
        drop(TurnGuard(flag));
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "reader kept draining after the turn was over"
        );
        drop(rx);
    }

    // AUD-010 repro: `sleep 6; echo done` used to return after ~6.1 s
    // because the orphaned `sleep` held the pipes.
    #[cfg(unix)]
    #[test]
    fn timeout_does_not_wait_for_orphaned_foreground_child() {
        let root = tmp_root();
        let bg = BgPid(root.join("bg.pid"));
        // Long enough for a slow login-shell profile to reach `sleep 6 &`
        // before the timeout fires, yet well below the 6 s of the old code.
        const TIMEOUT_MS: u64 = 2_000;
        let started = Instant::now();
        // `wait` keeps the shell alive on the child like a foreground `sleep`
        // would, while recording the pid so the test can clean it up.
        let exec = execute_shell_with_grace(
            "sleep 6 & echo $! > bg.pid; wait; echo done",
            &root,
            TIMEOUT_MS,
            OUTPUT_GRACE,
        )
        .unwrap();
        let elapsed = started.elapsed();
        assert!(exec.timed_out);
        assert!(exec.pipes_left_open);
        // timeout + SIGKILL escalation (a shell that ignores SIGTERM) + grace,
        // plus slack for a loaded machine; far below the 6 s of the old code.
        let bound =
            Duration::from_millis(TIMEOUT_MS + 500) + OUTPUT_GRACE + Duration::from_millis(500);
        assert!(
            elapsed < bound,
            "returned after {:?}, bound {:?}",
            elapsed,
            bound
        );
        drop(bg);
        let _ = std::fs::remove_dir_all(&root);
    }

    // Runs a POSIX shell command; the runner is not ported to Windows.
    #[cfg(unix)]
    #[test]
    fn nonzero_exit_is_captured() {
        let root = tmp_root();
        let exec = execute_shell("false", &root, 3_000).unwrap();
        assert_eq!(exec.exit_code, Some(1));
        assert!(!exec.timed_out);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cwd_outside_root_is_rejected() {
        let root = tmp_root();
        let err = resolve_cwd(&root, "/").unwrap_err();
        assert!(err.contains("outside project root"));
        let _ = std::fs::remove_dir_all(&root);
    }

    // Runs a POSIX shell command; the runner is not ported to Windows.
    #[cfg(unix)]
    #[test]
    fn redaction_in_executed_result() {
        let root = tmp_root();
        let exec =
            execute_shell("printf 'sk-abcdefghijklmnopqrstuvwxyz123'", &root, 3_000).unwrap();
        let r = req("printf 'sk-...'", SendOutput::Truncated);
        let res = build_executed_result(&r, &root, &r.command, exec);
        assert!(res.stdout.contains("«redacted:openai_api_key»"));
        assert!(res.redactions_applied.iter().any(|n| n == "openai_api_key"));
        let _ = std::fs::remove_dir_all(&root);
    }

    const FAKE_KEY: &str = "sk-abcdefghijklmnopqrstuvwxyz123";

    #[cfg(unix)]
    #[test]
    fn executed_result_redacts_the_command_but_runs_it_unchanged() {
        let root = tmp_root();
        let command = format!("printf ran # {}", FAKE_KEY);
        let exec = execute_shell(&command, &root, 3_000).unwrap();
        assert_eq!(exec.stdout, "ran");
        let r = req(&command, SendOutput::Truncated);
        let res = build_executed_result(&r, &root, &command, exec);
        assert!(!res.command.contains(FAKE_KEY));
        assert!(res.command.contains("«redacted:openai_api_key»"));
        assert!(res.redactions_applied.iter().any(|n| n == "openai_api_key"));
        assert_eq!(r.command, command, "the request itself is not rewritten");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skip_and_policy_block_results_redact_the_command() {
        let command = format!("curl -H 'x: {}' example.test", FAKE_KEY);
        let r = req(&command, SendOutput::Summary);
        let cwd = std::env::current_dir().unwrap();
        let skipped = build_skip_result(&r, &cwd, &command);
        let blocked = build_policy_block_result(&r, &cwd, "policy_blocked: x".into());
        for res in [skipped, blocked] {
            assert!(!res.command.contains(FAKE_KEY));
            assert!(res.command.contains("«redacted:openai_api_key»"));
            assert!(res.redactions_applied.iter().any(|n| n == "openai_api_key"));
        }
    }

    #[test]
    fn merge_rules_sorts_and_drops_duplicates() {
        let merged = merge_rules(
            vec!["b_rule".into(), "openai_api_key".into()],
            vec!["openai_api_key".into(), "a_rule".into()],
        );
        assert_eq!(merged, vec!["a_rule", "b_rule", "openai_api_key"]);
    }

    #[cfg(unix)]
    #[test]
    fn edited_command_runs_as_typed_but_is_stored_redacted() {
        let root = tmp_root();
        let cfg = RunConfig {
            project_root: root.clone(),
            timeout_ms: 5_000,
            assume_no_tty: true,
            auto_readonly: false,
            yolo: false,
        };
        let marker = root.join("ran.txt");
        let edited = format!("echo {} > ran.txt", FAKE_KEY);
        let r = req("ls", SendOutput::Truncated);
        let out = decide_and_run(&r, &cfg, &mut 0, &mut |_| {
            Ok(Decision::RunEdited {
                command: edited.clone(),
            })
        })
        .unwrap();
        let res = match out {
            RunOutcome::Executed(res) => res,
            other => panic!("expected Executed, got {:?}", other),
        };
        // The real, edited command ran: it wrote the real value.
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), FAKE_KEY);
        // The result (stored and sent) carries the redacted copy.
        assert!(!res.command.contains(FAKE_KEY));
        assert!(res.command.contains("«redacted:openai_api_key»"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plain_command_is_left_alone_in_results() {
        let r = req("ls -la", SendOutput::Summary);
        let cwd = std::env::current_dir().unwrap();
        let res = build_skip_result(&r, &cwd, "ls -la");
        assert_eq!(res.command, "ls -la");
        assert!(res.redactions_applied.is_empty());
    }

    #[test]
    fn no_tty_falls_through_to_skip() {
        let root = tmp_root();
        let cfg = RunConfig {
            project_root: root.clone(),
            timeout_ms: 5_000,
            assume_no_tty: true,
            auto_readonly: false,
            yolo: false,
        };
        let r = req("ls", SendOutput::Summary);
        let outcome = prompt_and_run(&r, &cfg, &mut 0).unwrap();
        match outcome {
            RunOutcome::UserRejected(res) => {
                assert_eq!(res.stderr, "user_rejected");
                assert_eq!(res.command_id, r.id);
            }
            other => panic!("expected UserRejected, got {:?}", other),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn denylisted_command_becomes_policy_block() {
        // We need a non-blocked path through prompt_and_run that turns into
        // PolicyBlocked via the loop iterating with the run branch. Easier
        // to exercise build_policy_block_result directly here.
        let r = req("sudo rm -rf /", SendOutput::Summary);
        let cwd = std::env::current_dir().unwrap();
        let res = build_policy_block_result(&r, &cwd, "policy_blocked: sudo".into());
        assert_eq!(res.stderr, "policy_blocked: sudo");
        assert_eq!(res.exit_code, None);
    }

    // -----------------------------------------------------------------------
    // --auto-readonly: canonical paths and the in-a-row cap (AUD-012)
    // -----------------------------------------------------------------------

    /// A project root with `inside.txt`, and a separate directory outside it
    /// with `secret.txt`. Both canonical.
    fn project_and_outside() -> (PathBuf, PathBuf) {
        let root = tmp_root().canonicalize().unwrap();
        let outside = tmp_root().canonicalize().unwrap();
        std::fs::write(root.join("inside.txt"), "in").unwrap();
        std::fs::write(outside.join("secret.txt"), "SECRET").unwrap();
        (root, outside)
    }

    fn cleanup(dirs: &[&Path]) {
        for d in dirs {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    fn auto_cfg(root: &Path) -> RunConfig {
        RunConfig {
            project_root: root.to_path_buf(),
            timeout_ms: 5_000,
            assume_no_tty: true,
            auto_readonly: true,
            yolo: false,
        }
    }

    #[cfg(unix)]
    /// Runs `command` under `--auto-readonly`. If the runner asks, `answer`
    /// is the user's reply. Returns the outcome and whether it asked.
    fn run_auto(
        cfg: &RunConfig,
        command: &str,
        streak: &mut u32,
        answer: Decision,
    ) -> (RunOutcome, bool) {
        let mut asked = false;
        let outcome = decide_and_run(
            &req(command, SendOutput::Truncated),
            cfg,
            streak,
            &mut |_blocked| {
                asked = true;
                Ok(answer.clone())
            },
        )
        .unwrap();
        (outcome, asked)
    }

    #[cfg(unix)]
    fn assert_read_only(command: &str) {
        assert_eq!(
            denylist::classify(command).local_risk,
            LocalRisk::ReadOnly,
            "precondition: the classifier must call {:?} read-only, so only the path check can stop it",
            command
        );
    }

    #[cfg(unix)]
    #[test]
    fn auto_readonly_runs_an_in_project_read_only_command() {
        let (root, outside) = project_and_outside();
        let mut streak = 0;
        let (out, asked) = run_auto(
            &auto_cfg(&root),
            "cat inside.txt",
            &mut streak,
            Decision::Skip,
        );
        assert!(!asked);
        assert!(
            matches!(out, RunOutcome::Executed(ref r) if r.stdout == "in"),
            "{:?}",
            out
        );
        assert_eq!(streak, 1);
        cleanup(&[&root, &outside]);
    }

    // Acceptance: a symlink inside the root to a file outside is not
    // auto-approved.
    #[cfg(unix)]
    #[test]
    fn auto_readonly_asks_for_a_symlink_to_a_file_outside() {
        let (root, outside) = project_and_outside();
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link")).unwrap();
        assert_read_only("cat link");
        let mut streak = 0;
        let (out, asked) = run_auto(&auto_cfg(&root), "cat link", &mut streak, Decision::Skip);
        assert!(asked, "symlink out of the project was auto-approved");
        assert!(matches!(out, RunOutcome::UserRejected(_)), "{:?}", out);
        assert_eq!(streak, 0);
        // Quoting, escapes and a line continuation still name the link.
        for cmd in ["cat 'link'", "cat \"li\"nk", "cat l\\ink", "cat li\\\nnk"] {
            assert!(
                path_outside_root(cmd, &root, &root).is_some(),
                "{:?} passed the path check",
                cmd
            );
        }
        cleanup(&[&root, &outside]);
    }

    #[cfg(unix)]
    #[test]
    fn auto_readonly_asks_for_paths_through_a_symlinked_directory() {
        let (root, outside) = project_and_outside();
        std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
        let cfg = auto_cfg(&root);
        for cmd in [
            "cat out/secret.txt",
            // Does not exist yet: resolved through its parent.
            "cat out/new.txt",
            "cat out/new/deeper.txt",
            "ls out",
            "grep -n x out/secret.txt",
            // Flag values and comma lists are paths too.
            "grep --file=out/secret.txt inside.txt",
            "cat inside.txt,out/secret.txt",
            // A glob expands to the link.
            "ls o?t",
            // Wrapped commands are checked by their inner text.
            "bash -c 'cat out/secret.txt'",
            "sh -c \"cat inside.txt; cat out/secret.txt\"",
        ] {
            assert_read_only(cmd);
            let mut streak = 0;
            let (_, asked) = run_auto(&cfg, cmd, &mut streak, Decision::Skip);
            assert!(asked, "{:?} was auto-approved", cmd);
        }
        cleanup(&[&root, &outside]);
    }

    #[cfg(unix)]
    #[test]
    fn auto_readonly_glob_is_matched_against_the_directory() {
        let (root, outside) = project_and_outside();
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link")).unwrap();
        assert_read_only("cat lin*");
        let cfg = auto_cfg(&root);
        let mut streak = 0;
        assert!(run_auto(&cfg, "cat lin*", &mut streak, Decision::Skip).1);
        // `*.txt` does not match `link`, so it stays automatic.
        let (out, asked) = run_auto(&cfg, "cat *.txt", &mut streak, Decision::Skip);
        assert!(!asked);
        assert!(matches!(out, RunOutcome::Executed(_)), "{:?}", out);
        // A quoted star is not a glob: the literal name does not exist.
        assert!(!run_auto(&cfg, "ls 'lin*'", &mut streak, Decision::Skip).1);
        cleanup(&[&root, &outside]);
    }

    // On a case-insensitive filesystem (the macOS default) `OUT` opens the
    // link named `out`; the check must ask the filesystem, not compare text.
    #[cfg(unix)]
    #[test]
    fn auto_readonly_case_insensitive_name_still_resolves_the_link() {
        let (root, outside) = project_and_outside();
        std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
        let case_insensitive = root.join("OUT").exists();
        let mut streak = 0;
        let (_, asked) = run_auto(
            &auto_cfg(&root),
            "cat OUT/secret.txt",
            &mut streak,
            Decision::Skip,
        );
        if case_insensitive {
            assert!(
                asked,
                "OUT/secret.txt was auto-approved on a case-insensitive filesystem"
            );
        } else {
            // Case-sensitive: `OUT` names nothing, the shell cannot follow it.
            assert!(!asked);
        }
        // A differently cased in-project name stays inside either way.
        assert!(
            !run_auto(
                &auto_cfg(&root),
                "cat INSIDE.txt",
                &mut streak,
                Decision::Skip
            )
            .1
        );
        cleanup(&[&root, &outside]);
    }

    #[cfg(unix)]
    #[test]
    fn auto_readonly_asks_for_a_dangling_symlink() {
        let (root, outside) = project_and_outside();
        std::os::unix::fs::symlink(outside.join("later.txt"), root.join("link")).unwrap();
        let mut streak = 0;
        assert!(run_auto(&auto_cfg(&root), "cat link", &mut streak, Decision::Skip).1);
        cleanup(&[&root, &outside]);
    }

    // The runner's check does not rely on the classifier's text rules: each
    // of these is caught, or refused as uncheckable, by the path check alone.
    #[cfg(unix)]
    #[test]
    fn path_check_catches_every_way_out_of_the_root() {
        let (root, outside) = project_and_outside();
        std::fs::create_dir(root.join("sub")).unwrap();
        for cmd in [
            "cat ../secret.txt",
            "cat ./../secret.txt",
            "cat sub/../../secret.txt",
            "cat missing/../../secret.txt",
            "cat ~/x",
            "cat ~",
            "cat ~root/x",
            "cat $HOME/x",
            "cat \"$HOME\"/x",
            "cat ${HOME}/x",
            "cat $'\\x2e\\x2e/secret.txt'",
            "cat `echo ..`/secret.txt",
            "cat /etc/hosts",
            &format!("cat {}", outside.join("secret.txt").display()),
            "cat */../../secret.txt",
            "cat ou*/secret.txt",
            "cat =ls",
            // Brace expansion joins the group with the text around it
            // (`o{u,}t` is `out t`); it is refused, not split.
            "cat o{u,}t/secret.txt",
            "cat {inside.txt,../secret.txt}",
            "bash -c 'cat o{u,}t/secret.txt'",
        ] {
            assert!(
                path_outside_root(cmd, &root, &root).is_some(),
                "{:?} passed the path check",
                cmd
            );
        }
        for cmd in [
            "cat inside.txt",
            "ls",
            "ls sub",
            "cat sub/../inside.txt",
            "cat missing/new.txt",
            "grep 'end$' inside.txt",
            "grep -rn 'a b' .",
            &format!("cat {}", root.join("inside.txt").display()),
        ] {
            assert_eq!(path_outside_root(cmd, &root, &root), None, "{:?}", cmd);
        }
        cleanup(&[&root, &outside]);
    }

    // The root the agent gets from `current_dir` need not be canonical (a
    // symlinked checkout, `/tmp` on macOS): the runner canonicalises it, or
    // every in-project path would look outside.
    #[cfg(unix)]
    #[test]
    fn auto_readonly_canonicalises_a_symlinked_project_root() {
        let (root, outside) = project_and_outside();
        let links = tmp_root();
        let via_link = links.join("project");
        std::os::unix::fs::symlink(&root, &via_link).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link")).unwrap();
        let cfg = auto_cfg(&via_link);
        let mut streak = 0;
        let (out, asked) = run_auto(&cfg, "cat inside.txt", &mut streak, Decision::Skip);
        assert!(!asked, "in-project command asked under a symlinked root");
        assert!(
            matches!(out, RunOutcome::Executed(ref r) if r.stdout == "in"),
            "{:?}",
            out
        );
        assert!(run_auto(&cfg, "cat link", &mut streak, Decision::Skip).1);
        cleanup(&[&root, &outside, &links]);
    }

    // The same bypasses through the whole decision: none is auto-approved.
    #[cfg(unix)]
    #[test]
    fn auto_readonly_never_auto_approves_a_way_out() {
        let (root, outside) = project_and_outside();
        let cfg = auto_cfg(&root);
        for cmd in [
            "cat ../secret.txt",
            "cat ./../secret.txt",
            "cat ~/x",
            "cat $HOME/x",
            "cat $'\\x2e\\x2e/secret.txt'",
        ] {
            let mut streak = 0;
            let (out, asked) = run_auto(&cfg, cmd, &mut streak, Decision::Skip);
            assert!(asked, "{:?} was auto-approved", cmd);
            assert!(matches!(out, RunOutcome::UserRejected(_)), "{:?}", out);
        }
        cleanup(&[&root, &outside]);
    }

    // Acceptance: after 20 auto-approvals in a row the next read-only command
    // asks. Only a manual confirmation resets the count.
    #[cfg(unix)]
    #[test]
    fn auto_readonly_cap_asks_after_twenty_in_a_row() {
        assert_eq!(MAX_AUTO_APPROVALS_IN_A_ROW, 20);
        let (root, outside) = project_and_outside();
        let cfg = auto_cfg(&root);
        let mut streak = 0;
        for i in 0..20 {
            let (out, asked) = run_auto(&cfg, "cat inside.txt", &mut streak, Decision::Skip);
            assert!(!asked, "asked at auto-approval {}", i + 1);
            assert!(matches!(out, RunOutcome::Executed(_)), "{:?}", out);
        }
        assert_eq!(streak, 20);
        // 21st: asks. A skip is not a confirmation, the cap stays.
        let (out, asked) = run_auto(&cfg, "cat inside.txt", &mut streak, Decision::Skip);
        assert!(asked);
        assert!(matches!(out, RunOutcome::UserRejected(_)), "{:?}", out);
        assert_eq!(streak, 20);
        assert!(run_auto(&cfg, "cat inside.txt", &mut streak, Decision::Skip).1);
        // A manual run resets the count.
        let (out, asked) = run_auto(&cfg, "cat inside.txt", &mut streak, Decision::Run);
        assert!(asked);
        assert!(matches!(out, RunOutcome::Executed(_)), "{:?}", out);
        assert_eq!(streak, 0);
        let (_, asked) = run_auto(&cfg, "cat inside.txt", &mut streak, Decision::Skip);
        assert!(!asked);
        assert_eq!(streak, 1);
        cleanup(&[&root, &outside]);
    }

    #[cfg(unix)]
    #[test]
    fn auto_readonly_cap_resets_on_an_edited_run_but_not_on_a_policy_block() {
        let (root, outside) = project_and_outside();
        let cfg = auto_cfg(&root);
        let mut streak = MAX_AUTO_APPROVALS_IN_A_ROW;
        // A refused cwd never reaches the prompt and changes nothing.
        let mut r = req("cat inside.txt", SendOutput::Truncated);
        r.cwd = "/".into();
        let out = decide_and_run(&r, &cfg, &mut streak, &mut |_| Ok(Decision::Run)).unwrap();
        assert!(matches!(out, RunOutcome::PolicyBlocked(_)), "{:?}", out);
        assert_eq!(streak, MAX_AUTO_APPROVALS_IN_A_ROW);
        let edited = Decision::RunEdited {
            command: "cat inside.txt".into(),
        };
        let (out, asked) = run_auto(&cfg, "cat inside.txt", &mut streak, edited);
        assert!(asked);
        assert!(matches!(out, RunOutcome::Executed(_)), "{:?}", out);
        assert_eq!(streak, 0);
        cleanup(&[&root, &outside]);
    }

    // A manual confirmation of a command the path check stopped also resets
    // the count: the user has looked.
    #[cfg(unix)]
    #[test]
    fn manual_confirm_after_a_path_refusal_resets_the_cap() {
        let (root, outside) = project_and_outside();
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link")).unwrap();
        let mut streak = 7;
        let (out, asked) = run_auto(&auto_cfg(&root), "cat link", &mut streak, Decision::Run);
        assert!(asked);
        assert!(matches!(out, RunOutcome::Executed(_)), "{:?}", out);
        assert_eq!(streak, 0);
        cleanup(&[&root, &outside]);
    }

    // --yolo is out of scope: no cap, no path check.
    #[cfg(unix)]
    #[test]
    fn yolo_is_not_capped() {
        let (root, outside) = project_and_outside();
        let cfg = RunConfig {
            yolo: true,
            ..auto_cfg(&root)
        };
        let mut streak = MAX_AUTO_APPROVALS_IN_A_ROW;
        let (out, asked) = run_auto(&cfg, "cat inside.txt", &mut streak, Decision::Skip);
        assert!(!asked);
        assert!(matches!(out, RunOutcome::Executed(_)), "{:?}", out);
        cleanup(&[&root, &outside]);
    }

    // The cap and the path check apply only to --auto-readonly on a
    // read-only command; elsewhere the runner asks (or runs, under --yolo)
    // as before, without a note.
    #[test]
    fn auto_refusal_only_for_auto_readonly_read_only_commands() {
        let (root, outside) = project_and_outside();
        let cap = MAX_AUTO_APPROVALS_IN_A_ROW;
        let ro = "cat inside.txt";
        let c = denylist::classify(ro);
        let cfg = auto_cfg(&root);
        // Listed programs come from a fake bin outside the project, so the
        // decision does not depend on what the CI runner has in PATH.
        let bin = fake_bin();
        let env = env_with_path(&[&bin]);
        assert!(auto_refusal_in(&cfg, &c, ro, &root, cap, &env).is_some());
        assert!(auto_refusal_in(&cfg, &c, ro, &root, cap - 1, &env).is_none());
        let unknown = "cat ../x";
        assert!(auto_refusal_in(
            &cfg,
            &denylist::classify(unknown),
            unknown,
            &root,
            cap,
            &env
        )
        .is_none());
        let off = RunConfig {
            auto_readonly: false,
            ..auto_cfg(&root)
        };
        assert!(auto_refusal_in(&off, &c, ro, &root, cap, &env).is_none());
        let yolo = RunConfig {
            yolo: true,
            ..auto_cfg(&root)
        };
        assert!(auto_refusal_in(&yolo, &c, ro, &root, cap, &env).is_none());
        cleanup(&[&root, &outside, &bin]);
    }

    // The refusal note shows a model-chosen path: it must be escaped.
    #[cfg(unix)]
    #[test]
    fn auto_refusal_note_escapes_the_path() {
        let (root, outside) = project_and_outside();
        let name = "x\u{1b}[2Jy";
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join(name)).unwrap();
        let cmd = format!("cat '{}'", name);
        let note = auto_refusal(&auto_cfg(&root), &denylist::classify(&cmd), &cmd, &root, 0)
            .expect("refused");
        assert_no_control(&note);
        assert!(note.contains("x\\x1b[2Jy"), "{}", note);
        cleanup(&[&root, &outside]);
    }

    // -----------------------------------------------------------------------
    // --auto-readonly: the narrow auto-run list (P1)
    // -----------------------------------------------------------------------

    /// Programs the list may run, as empty files in a folder outside every
    /// project, so the decision does not depend on what this machine has
    /// installed.
    const LISTED: &[&str] = &[
        "ls", "cat", "head", "tail", "wc", "stat", "file", "grep", "rg", "find", "tree", "sort",
        "echo", "pwd", "which",
    ];

    /// An empty file the shell could run: executable on Unix.
    fn program_file(path: &Path) {
        std::fs::write(path, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// A `bin` folder with every listed program; returns the folder.
    fn fake_bin() -> PathBuf {
        let bin = tmp_root().canonicalize().unwrap();
        for name in LISTED {
            program_file(&bin.join(name));
        }
        bin
    }

    fn env_with_path(dirs: &[&Path]) -> AutoEnv {
        AutoEnv {
            path: Some(std::env::join_paths(dirs).unwrap()),
            ripgrep_config: false,
        }
    }

    /// The decision alone: whether the runner would run `command` without a
    /// keypress in `env`. Nothing is executed.
    fn auto_runs_in(cfg: &RunConfig, command: &str, cwd: &Path, env: &AutoEnv) -> bool {
        let c = denylist::classify(command);
        let refused = auto_refusal_in(cfg, &c, command, cwd, 0, env).is_some();
        auto_approves(cfg, c.local_risk, refused)
    }

    /// `auto_runs_in` with every listed program found outside the project.
    fn auto_runs(cfg: &RunConfig, command: &str, cwd: &Path) -> bool {
        let bin = fake_bin();
        let runs = auto_runs_in(cfg, command, cwd, &env_with_path(&[&bin]));
        cleanup(&[&bin]);
        runs
    }

    #[test]
    fn auto_readonly_asks_for_forms_that_run_code_or_read_outside() {
        let (root, outside) = project_and_outside();
        let cfg = auto_cfg(&root);
        let auto_approved = [
            // A positional `version` is a script, class, package or file.
            "node version",
            "npx version",
            "java version",
            "rustc version",
            "dotnet version",
            "yarn version",
            "pnpm version",
            // A version query may run what the project's config names.
            "yarn --version",
            "yarn -v",
            // `ps` in any form: `e` / `-E` print the environment.
            "ps",
            "ps e",
            "ps -E",
            "ps aux",
            "ls | ps e",
            "bash -c 'ps e'",
            "bash -c ps",
            "sh -c 'ps'",
            "bash -lc ps",
            // Paths read from a file are not checked; before or after the
            // operands, `=` or a separate value, or an abbreviation.
            "sort --files0-from=list",
            "sort --files0-from list",
            "sort a --files0-from=list",
            "sort a --files0-from list",
            "wc --files0-from=list",
            "wc a --files0-from list",
            "sort a --fil=list",
            "sort a --fil list",
            "sort --files0 list",
            "sort 'a b' --fil=list",
            "wc --f=list",
            "wc a --fi list",
            "bash -c 'sort a --fil=list'",
            "find . -files0-from list",
            "echo inside.txt | sort --files0-from=-",
            "echo inside.txt | file -f -",
            // git: always asks (user decision 1 of 28.10).
            "git status",
            "git diff",
            "git log --oneline",
        ]
        .into_iter()
        .filter(|cmd| auto_runs(&cfg, cmd, &root))
        .collect::<Vec<_>>();
        cleanup(&[&root, &outside]);
        assert!(
            auto_approved.is_empty(),
            "auto-approved: {:?}",
            auto_approved
        );
    }

    // The list refuses the read-only forms themselves, not only through the
    // classifier: each is read-only in the panel, and the note names it.
    #[test]
    fn auto_refusal_names_the_command_off_the_list() {
        let (root, outside) = project_and_outside();
        let cfg = auto_cfg(&root);
        let env = AutoEnv::current();
        for (cmd, shown) in [
            ("yarn --version", "`yarn`"),
            ("ps e", "`ps`"),
            ("git status", "`git`"),
            ("tree -l", "`-l`"),
        ] {
            let c = denylist::classify(cmd);
            assert_eq!(c.local_risk, LocalRisk::ReadOnly, "precondition: {:?}", cmd);
            let note = auto_refusal_in(&cfg, &c, cmd, &root, 0, &env).expect("refused");
            assert!(note.contains(shown), "{:?}: {}", cmd, note);
            assert!(note.contains("auto-run list"), "{:?}: {}", cmd, note);
        }
        // The classifier itself does not call `--files0-from` read-only (P2),
        // so these ask before the list is reached.
        for cmd in ["sort a --files0-from=list", "find . -files0-from list"] {
            assert_eq!(denylist::classify(cmd).local_risk, LocalRisk::Unknown);
            assert!(!auto_runs(&cfg, cmd, &root), "{:?} was auto-approved", cmd);
        }
        cleanup(&[&root, &outside]);
    }

    // The name is matched as the raw first word: an operand or a longer name
    // is not the command, and `ps.exe`, `PS` or `Yarn` are not on the list.
    #[test]
    fn auto_readonly_matches_the_command_name_exactly() {
        let (root, outside) = project_and_outside();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/x"), "x").unwrap();
        let cfg = auto_cfg(&root);
        for cmd in [
            "ls",
            "cat src/x",
            "rg foo src",
            "cat ps",
            "grep node src",
            "rg version src",
            "wc -l src/x",
            "sort src/x",
            "sort -t, -k2 src/x",
            "rg --files src",
        ] {
            assert!(auto_runs(&cfg, cmd, &root), "{:?} asked", cmd);
        }
        for cmd in ["PS e", "Yarn --version", "ps.exe", "Cat src/x"] {
            assert_eq!(denylist::classify(cmd).local_risk, LocalRisk::ReadOnly);
            assert!(!auto_runs(&cfg, cmd, &root), "{:?} was auto-approved", cmd);
        }
        cleanup(&[&root, &outside]);
    }

    // Only --auto-readonly changes: --yolo and the manual panel stay as they
    // were.
    #[test]
    fn list_refusals_unchanged_outside_auto_readonly() {
        let (root, outside) = project_and_outside();
        let yolo = RunConfig {
            yolo: true,
            ..auto_cfg(&root)
        };
        let manual = RunConfig {
            auto_readonly: false,
            ..auto_cfg(&root)
        };
        for (cmd, risk) in [
            ("ps e", LocalRisk::ReadOnly),
            ("yarn --version", LocalRisk::ReadOnly),
            ("git status", LocalRisk::ReadOnly),
            ("tree -l", LocalRisk::ReadOnly),
            // Not read-only in the classifier since P2.
            ("sort a --files0-from=list", LocalRisk::Unknown),
        ] {
            assert!(auto_runs(&yolo, cmd, &root), "{:?} asked under --yolo", cmd);
            assert!(!auto_runs(&manual, cmd, &root), "{:?}", cmd);
            assert_eq!(denylist::classify(cmd).local_risk, risk, "{:?}", cmd);
        }
        cleanup(&[&root, &outside]);
    }

    // Card: `git status` in a real repository asks; the panel still calls it
    // read-only. A refusal by the list does not count toward the cap.
    #[cfg(unix)]
    #[test]
    fn auto_readonly_asks_for_git_status_in_a_repository() {
        let (root, outside) = project_and_outside();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status();
        if !init.is_ok_and(|s| s.success()) {
            eprintln!("git is not available; skipping");
            cleanup(&[&root, &outside]);
            return;
        }
        assert_read_only("git status");
        let mut streak = 5;
        let (out, asked) = run_auto(&auto_cfg(&root), "git status", &mut streak, Decision::Skip);
        assert!(asked, "git status was auto-approved");
        assert!(matches!(out, RunOutcome::UserRejected(_)), "{:?}", out);
        assert_eq!(streak, 5, "a refused command changed the count");
        cleanup(&[&root, &outside]);
    }

    #[cfg(unix)]
    #[test]
    fn auto_readonly_runs_ls_and_cat_in_the_project() {
        let (root, outside) = project_and_outside();
        let cfg = auto_cfg(&root);
        let mut streak = 0;
        let (out, asked) = run_auto(&cfg, "ls", &mut streak, Decision::Skip);
        assert!(!asked);
        assert!(
            matches!(out, RunOutcome::Executed(ref r) if r.stdout.contains("inside.txt")),
            "{:?}",
            out
        );
        let (out, asked) = run_auto(&cfg, "cat -n inside.txt", &mut streak, Decision::Skip);
        assert!(!asked);
        assert!(matches!(out, RunOutcome::Executed(_)), "{:?}", out);
        assert_eq!(streak, 2);
        cleanup(&[&root, &outside]);
    }

    // User decision 5 of 28.10: a secrets file named as an argument asks.
    #[cfg(unix)]
    #[test]
    fn auto_readonly_asks_for_a_secrets_file_name() {
        let (root, outside) = project_and_outside();
        std::fs::write(root.join(".npmrc"), "x").unwrap();
        let mut streak = 0;
        let (out, asked) = run_auto(&auto_cfg(&root), "cat .npmrc", &mut streak, Decision::Skip);
        assert!(asked, "cat .npmrc was auto-approved");
        assert!(matches!(out, RunOutcome::UserRejected(_)), "{:?}", out);
        // Since P2 the classifier already does not call it read-only; with a
        // read-only class forced, the runner's list still refuses it.
        let forced = Classification {
            local_risk: LocalRisk::ReadOnly,
            blocks: Vec::new(),
            warnings: Vec::new(),
        };
        let note = auto_refusal(&auto_cfg(&root), &forced, "cat .npmrc", &root, 0)
            .expect("the list let cat .npmrc through");
        assert!(note.contains("secrets location"), "{}", note);
        cleanup(&[&root, &outside]);
    }

    // User decision of 28.10: the program found by name must lie outside the
    // project. `PATH` with `.` or another entry inside the project asks once
    // that entry holds the program; a program found nowhere asks too.
    #[cfg(unix)]
    #[test]
    fn auto_readonly_asks_for_a_program_found_inside_the_project() {
        let (root, outside) = project_and_outside();
        let bin = fake_bin();
        let cfg = auto_cfg(&root);
        let cmd = "cat inside.txt";
        let dot = Path::new(".");
        // `.` first, but no `cat` in the project: the outside one runs.
        assert!(auto_runs_in(&cfg, cmd, &root, &env_with_path(&[dot, &bin])));
        program_file(&root.join("cat"));
        assert!(!auto_runs_in(
            &cfg,
            cmd,
            &root,
            &env_with_path(&[dot, &bin])
        ));
        // After the outside entry, the project's file is never reached.
        assert!(auto_runs_in(&cfg, cmd, &root, &env_with_path(&[&bin, dot])));
        // An empty entry is the current folder too.
        let empty = AutoEnv {
            path: Some(OsString::from(format!(":{}", bin.display()))),
            ripgrep_config: false,
        };
        assert!(!auto_runs_in(&cfg, cmd, &root, &empty));
        // A relative or an absolute entry inside the project.
        std::fs::create_dir(root.join("tools")).unwrap();
        std::fs::rename(root.join("cat"), root.join("tools/cat")).unwrap();
        for dir in [Path::new("tools"), &root.join("tools")] {
            assert!(!auto_runs_in(
                &cfg,
                cmd,
                &root,
                &env_with_path(&[dir, &bin])
            ));
        }
        let note = auto_refusal_in(
            &cfg,
            &denylist::classify(cmd),
            cmd,
            &root,
            0,
            &env_with_path(&[Path::new("tools"), &bin]),
        )
        .expect("refused");
        assert!(note.contains("inside the project"), "{}", note);
        // Not found in PATH.
        let empty_bin = tmp_root();
        assert!(!auto_runs_in(
            &cfg,
            cmd,
            &root,
            &env_with_path(&[&empty_bin])
        ));
        let no_path = AutoEnv {
            path: None,
            ripgrep_config: false,
        };
        assert!(!auto_runs_in(&cfg, cmd, &root, &no_path));
        // Every stage's program is looked up.
        program_file(&root.join("sort"));
        assert!(!auto_runs_in(
            &cfg,
            "cat inside.txt | sort",
            &root,
            &env_with_path(&[dot, &bin])
        ));
        // The shell skips a file it cannot run, so a stray non-executable
        // `cat` earlier in PATH does not hide the project's one.
        std::fs::write(empty_bin.join("cat"), "").unwrap();
        assert!(!auto_runs_in(
            &cfg,
            cmd,
            &root,
            &env_with_path(&[&empty_bin, Path::new("tools"), &bin])
        ));
        cleanup(&[&root, &outside, &bin, &empty_bin]);
    }

    // User decision of 28.10: rg asks while RIPGREP_CONFIG_PATH is set; other
    // programs do not care.
    #[test]
    fn auto_readonly_asks_for_rg_with_a_config_file() {
        let (root, outside) = project_and_outside();
        let bin = fake_bin();
        let cfg = auto_cfg(&root);
        let with_config = AutoEnv {
            ripgrep_config: true,
            ..env_with_path(&[&bin])
        };
        assert!(auto_runs_in(&cfg, "rg foo", &root, &env_with_path(&[&bin])));
        assert!(!auto_runs_in(&cfg, "rg foo", &root, &with_config));
        assert!(!auto_runs_in(
            &cfg,
            "cat inside.txt | rg foo",
            &root,
            &with_config
        ));
        assert!(auto_runs_in(&cfg, "grep -r foo .", &root, &with_config));
        cleanup(&[&root, &outside, &bin]);
    }

    // Ruling 48: the shell expands a glob before the program runs, so a
    // matching file named like a flag reaches it as one, past the list.
    #[cfg(unix)]
    #[test]
    fn auto_readonly_asks_when_a_glob_matches_a_flag_like_name() {
        let (root, outside) = project_and_outside();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::fs::write(root.join("-l"), "").unwrap();
        let cfg = auto_cfg(&root);
        let note = auto_refusal(&cfg, &denylist::classify("cat *"), "cat *", &root, 0)
            .expect("cat * was auto-approved");
        assert!(note.contains("`-l`"), "{}", note);
        let mut streak = 0;
        let (out, asked) = run_auto(&cfg, "cat *.txt", &mut streak, Decision::Skip);
        assert!(!asked, "cat *.txt asked");
        assert!(matches!(out, RunOutcome::Executed(_)), "{:?}", out);
        cleanup(&[&root, &outside]);
    }

    // User decision 5 of 28.10, through a glob: a match that is a secrets
    // file asks. As in the shell, `*` does not match a dot-name unless the
    // pattern starts with `.`.
    #[cfg(unix)]
    #[test]
    fn auto_readonly_asks_when_a_glob_matches_a_secrets_file() {
        let (root, outside) = project_and_outside();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::fs::write(root.join(".env"), "x").unwrap();
        std::fs::create_dir(root.join("secrets")).unwrap();
        std::fs::write(root.join("secrets/credentials.json"), "x").unwrap();
        let cfg = auto_cfg(&root);
        let mut streak = 0;
        let (out, asked) = run_auto(&cfg, "cat *", &mut streak, Decision::Skip);
        assert!(!asked, "cat * asked");
        assert!(matches!(out, RunOutcome::Executed(_)), "{:?}", out);
        for cmd in ["cat .e*", "cat secrets/*"] {
            let note = auto_refusal(&cfg, &denylist::classify(cmd), cmd, &root, 0)
                .unwrap_or_else(|| panic!("{:?} was auto-approved", cmd));
            assert!(note.contains("secrets"), "{}", note);
        }
        cleanup(&[&root, &outside]);
    }
}
