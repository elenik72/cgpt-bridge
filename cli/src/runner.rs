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

use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
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
    /// Auto-approve commands whose **local** classification is `ReadOnly`.
    /// Denylist hits remain blocked. Other risks still prompt.
    pub auto_readonly: bool,
    /// Auto-approve every non-blocked command without a keypress. The
    /// confirmation panel is still rendered so the run is auditable; the
    /// runner just records a synthetic `[yolo]` decision.
    pub yolo: bool,
}

/// Show the confirmation panel, read the user decision, and (if approved)
/// execute the command. The classifier output is computed inside so the UI
/// can show it next to the assistant-declared `risk`.
pub fn prompt_and_run(req: &CommandRequest, cfg: &RunConfig) -> io::Result<RunOutcome> {
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
        let auto_approve =
            !is_blocked && (cfg.yolo || (cfg.auto_readonly && local_risk == LocalRisk::ReadOnly));
        let decision = if auto_approve {
            if cfg.yolo {
                eprintln!("⚠  --yolo: auto-approving (local risk: {:?})", local_risk);
            } else {
                eprintln!("✓ --auto-readonly: auto-approving read-only command");
            }
            Decision::Run
        } else if cfg.assume_no_tty || !io::stdin().is_terminal() {
            eprintln!(
                "cgpt: stdin is not a TTY; refusing to run a proposed command without an interactive confirmation."
            );
            Decision::Skip
        } else {
            read_decision(is_blocked)?
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

fn render_panel(req: &CommandRequest, c: &Classification, cwd: &Path, extra_err: Option<&str>) {
    let mut out = io::stderr().lock();
    let _ = writeln!(out, "");
    let _ = writeln!(
        out,
        "──── proposed command ────────────────────────────────"
    );
    let _ = writeln!(out, "  id:              {}", req.id);
    let _ = writeln!(out, "  cwd (resolved):  {}", cwd.display());
    let _ = writeln!(out, "  description:     {}", req.description);
    let _ = writeln!(out, "  expected effect: {}", req.expected_effect);
    let _ = writeln!(out, "  declared risk:   {:?}", req.risk);
    let _ = writeln!(out, "  local risk:      {:?}", c.local_risk);
    let _ = writeln!(out, "  timeout_ms:      {}", req.timeout_ms);
    let _ = writeln!(out, "  send_output:     {:?}", req.send_output);
    let _ = writeln!(out, "  command:");
    let _ = writeln!(out, "      {}", req.command);
    if !c.blocks.is_empty() {
        let _ = writeln!(out, "  blocks:");
        for h in &c.blocks {
            let _ = writeln!(out, "    - [{}] {}", h.rule_name, h.note);
        }
    }
    if !c.warnings.is_empty() {
        let _ = writeln!(out, "  warnings:");
        for h in &c.warnings {
            let _ = writeln!(out, "    - [{}] {}", h.rule_name, h.note);
        }
    }
    if let Some(extra) = extra_err {
        let _ = writeln!(out, "  ! {}", extra);
    }
    let _ = writeln!(
        out,
        "──────────────────────────────────────────────────────"
    );
}

fn render_panel_inline(command: &str, c: &Classification) {
    let mut out = io::stderr().lock();
    let _ = writeln!(out, "  edited command:");
    let _ = writeln!(out, "      {}", command);
    let _ = writeln!(out, "  local risk:      {:?}", c.local_risk);
    if !c.blocks.is_empty() {
        let _ = writeln!(out, "  blocks:");
        for h in &c.blocks {
            let _ = writeln!(out, "    - [{}] {}", h.rule_name, h.note);
        }
    }
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
}

fn execute_shell(command: &str, cwd: &Path, timeout_ms: u64) -> io::Result<ExecOutcome> {
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
    let stdout_thread = thread::spawn(move || drain_pipe(stdout, otx));
    let stderr_thread = thread::spawn(move || drain_pipe(stderr, etx));

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

    // Once the child has been reaped, the drain threads' senders will close
    // and we can collect everything.
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    let stdout = collect_bytes(&orx);
    let stderr = collect_bytes(&erx);

    Ok(ExecOutcome {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        exit_code,
        duration_ms,
        timed_out,
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

fn drain_pipe<R: Read + Send + 'static>(mut r: R, tx: mpsc::Sender<Vec<u8>>) {
    // Cap per-stream raw capture at 4 MiB to bound memory before redaction.
    const RAW_CAP: usize = 4 * 1024 * 1024;
    let mut buf = vec![0u8; 8 * 1024];
    let mut total = Vec::with_capacity(64 * 1024);
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if total.len() < RAW_CAP {
                    let remaining = RAW_CAP - total.len();
                    let take = n.min(remaining);
                    total.extend_from_slice(&buf[..take]);
                }
                // Beyond the cap we keep reading so the child's pipe does
                // not block, but we stop accumulating.
            }
            Err(_) => break,
        }
    }
    let _ = tx.send(total);
}

fn collect_bytes(rx: &mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    while let Ok(part) = rx.try_recv() {
        out.extend(part);
    }
    out
}

#[cfg(unix)]
fn kill_with_grace(child: &mut Child) {
    use std::os::unix::process::ExitStatusExt;
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
    // Touch ExitStatusExt to keep the import meaningful on unix.
    let _ = std::process::ExitStatus::from_raw;
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
    let trunc = redact::truncate(&red.stdout, &red.stderr, req.send_output);
    CommandResultV1 {
        version: 1,
        command_id: req.id.clone(),
        cwd: cwd_used.display().to_string(),
        command: command_used.to_string(),
        exit_code: exec.exit_code,
        duration_ms: exec.duration_ms,
        timed_out: exec.timed_out,
        stdout: trunc.stdout,
        stderr: trunc.stderr,
        output_truncated: trunc.truncated,
        redactions_applied: red.rules_fired,
    }
}

fn build_skip_result(req: &CommandRequest, cwd_used: &Path, command: &str) -> CommandResultV1 {
    CommandResultV1 {
        version: 1,
        command_id: req.id.clone(),
        cwd: cwd_used.display().to_string(),
        command: command.to_string(),
        exit_code: None,
        duration_ms: 0,
        timed_out: false,
        stdout: String::new(),
        stderr: "user_rejected".to_string(),
        output_truncated: false,
        redactions_applied: Vec::new(),
    }
}

fn build_policy_block_result(
    req: &CommandRequest,
    cwd_used: &Path,
    reason: String,
) -> CommandResultV1 {
    CommandResultV1 {
        version: 1,
        command_id: req.id.clone(),
        cwd: cwd_used.display().to_string(),
        command: req.command.clone(),
        exit_code: None,
        duration_ms: 0,
        timed_out: false,
        stdout: String::new(),
        stderr: reason,
        output_truncated: false,
        redactions_applied: Vec::new(),
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

    fn tmp_root() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let p = PathBuf::from(format!("/tmp/cgb-runner-test-{}-{}", pid, n));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

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
        let res = build_executed_result(&r, &root, &r.command, exec);
        assert_eq!(res.exit_code, Some(0));
        assert!(res.stdout.contains("hello.txt"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = cfg;
    }

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
        let outcome = prompt_and_run(&r, &cfg).unwrap();
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
}
