//! `cgpt` — terminal-side CLI.
//!
//! M5 phase 1 scope: argument parsing, stdin handling, UDS client, single
//! `cgpt ask` round trip. Phase 2 wires this to a real router inside the
//! native host and through the extension to the active ChatGPT tab.
//!
//! Stdout discipline (per docs/requirements.md §5.1 and §9):
//!   - `cgpt ask` stdout = the visible assistant response text, nothing else.
//!   - All progress, warnings, and errors go to stderr.
//!   - Exit codes are categorized.

use std::io::{self, IsTerminal, Read};
use std::process::ExitCode;

use cgpt_bridge_cli::{args, clipboard, editor, history, spinner::Phase, transport};
use cgpt_bridge_protocol::{AskRequest, BridgeResponse, ErrorCode};

use args::{Cli, Command};
use transport::AskOutcome;

/// CLI exit codes (subset of docs/requirements.md §9 that M5-phase-1 can
/// produce). Stable, do not reorder.
#[repr(u8)]
enum ExitKind {
    Ok = 0,
    Generic = 1,
    Usage = 2,
    Setup = 3,
    Transport = 4,
    Tab = 5,
    Dom = 6,
    Protocol = 7,
    Internal = 11,
}

fn main() -> ExitCode {
    let cli = match args::parse() {
        Ok(c) => c,
        Err(e) => {
            // For --help and --version clap returns Err with exit_code() == 0
            // and the formatted message ready to print to stdout. For real
            // parse errors exit_code() is non-zero and the message belongs on
            // stderr. `Error::print()` picks the correct stream.
            let _ = e.print();
            return ExitCode::from(match e.exit_code() {
                0 => 0,
                _ => ExitKind::Usage as u8,
            });
        }
    };

    if cli.no_spinner {
        // Spinner module reads this env var on each Spinner::start. Setting
        // it here means the flag works across the binary's whole lifetime
        // without threading another parameter through every call site.
        std::env::set_var("CGPT_SPINNER", "off");
    }

    let code = run(cli);
    ExitCode::from(code as u8)
}

fn run(cli: Cli) -> ExitKind {
    match cli.command {
        Command::Ask(ask) => run_ask(ask, &cli.socket_override),
        Command::Agent(agent_args) => {
            let code = cgpt_bridge_cli::agent::run(agent_args, cli.socket_override.clone());
            // Bypass the local ExitKind enum and exit directly with the
            // agent loop's chosen code; ExitKind only models the ask path.
            std::process::exit(code as i32);
        }
        Command::History(h) => std::process::exit(history::run_history(h) as i32),
        Command::Replay(r) => std::process::exit(history::run_replay(r) as i32),
        Command::Last(l) => std::process::exit(history::run_last(l) as i32),
    }
}

fn run_ask(cmd: args::AskArgs, socket_override: &Option<std::path::PathBuf>) -> ExitKind {
    let prompt = match collect_prompt(&cmd.prompt, cmd.buffer, cmd.editor) {
        Ok(p) => p,
        Err(code) => return code,
    };

    let socket_path = match transport::resolve_socket_path(socket_override.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cgpt: cannot resolve socket path: {}", e);
            return ExitKind::Setup;
        }
    };

    let request = AskRequest {
        id: transport::new_request_id("ask"),
        text: prompt,
        timeout_ms: cmd.timeout_ms,
    };

    let phase = Phase::start("asking ChatGPT…");
    let outcome = transport::ask_once(&socket_path, &request, cmd.timeout_ms);

    match outcome {
        AskOutcome::Ok(BridgeResponse::AskResult { text, .. }) => {
            phase.done(format!("reply received ({} chars)", text.chars().count()));
            // Stdout = assistant text only. Single trailing newline so
            // `cgpt ask "x" | wc -l` behaves like other CLIs.
            print!("{}", text);
            if !text.ends_with('\n') {
                println!();
            }
            if cmd.copy {
                if let Err(e) = clipboard::write(&text) {
                    eprintln!("cgpt: --copy: clipboard write failed: {}", e);
                }
            }
            ExitKind::Ok
        }
        AskOutcome::Ok(BridgeResponse::Pong { .. }) => {
            phase.fail("host returned pong (protocol bug)");
            eprintln!("cgpt: host returned pong for an ask request (protocol bug)");
            ExitKind::Protocol
        }
        AskOutcome::Ok(BridgeResponse::Error { code, message, .. }) => {
            phase.fail(format!("host error: {:?}", code));
            eprintln!("cgpt: host error ({:?}): {}", code, message);
            map_error_code(code)
        }
        AskOutcome::SocketMissing => {
            phase.fail("native host socket missing");
            eprintln!(
                "cgpt: native host socket not found at {}.\n\
                 The native host is not running. Verify that:\n\
                 - The cgpt-bridge Chrome extension is installed and enabled.\n\
                 - Chrome is open with the extension active.\n\
                 - The native messaging manifest has been installed via\n\
                 install/macos/install-host.sh (M5 phase 2 wires this up).",
                socket_path.display(),
            );
            ExitKind::Setup
        }
        AskOutcome::SocketIo(e) => {
            phase.fail(format!("transport error: {}", e));
            ExitKind::Transport
        }
        AskOutcome::Timeout => {
            phase.fail(format!("timed out after {}ms", cmd.timeout_ms));
            ExitKind::Tab
        }
        AskOutcome::BadResponse(msg) => {
            phase.fail(format!("bad host response: {}", msg));
            ExitKind::Protocol
        }
    }
}

fn map_error_code(code: ErrorCode) -> ExitKind {
    match code {
        ErrorCode::BadRequest => ExitKind::Usage,
        ErrorCode::OversizeFrame => ExitKind::Transport,
        ErrorCode::ExtensionUnavailable => ExitKind::Setup,
        ErrorCode::TabUnavailable => ExitKind::Tab,
        ErrorCode::DomFailure => ExitKind::Dom,
        ErrorCode::Timeout => ExitKind::Tab,
        ErrorCode::Internal => ExitKind::Internal,
    }
}

/// Combine positional args, optional secondary input source, and optional
/// editor pass into the prompt text. Source resolution:
///   - `--buffer`  : secondary is the OS clipboard, stdin untouched.
///   - otherwise   : secondary is stdin when piped (non-TTY).
///   - `--editor`  : the combined prompt is fed to `$EDITOR` as an initial
///                   template; the user's final saved buffer wins.
///
/// If after all of the above the result is empty, exit code 2.
fn collect_prompt(
    positional: &[String],
    buffer: bool,
    editor_flag: bool,
) -> Result<String, ExitKind> {
    let positional_text = if positional.is_empty() {
        None
    } else {
        Some(positional.join(" "))
    };

    let secondary = if buffer {
        match clipboard::read() {
            Ok(s) if !s.trim().is_empty() => Some(s),
            Ok(_) => None,
            Err(e) => {
                eprintln!("cgpt: --buffer: failed to read clipboard: {}", e);
                return Err(ExitKind::Usage);
            }
        }
    } else if io::stdin().is_terminal() {
        None
    } else {
        let mut buf = String::new();
        io::stdin().lock().read_to_string(&mut buf).map_err(|e| {
            eprintln!("cgpt: failed to read stdin: {}", e);
            ExitKind::Generic
        })?;
        if buf.trim().is_empty() {
            None
        } else {
            Some(buf)
        }
    };

    let mut combined = match (positional_text, secondary) {
        (Some(a), Some(b)) => format!("{}\n\n{}", a, b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => String::new(),
    };

    if editor_flag {
        combined = match editor::capture(&combined) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cgpt: --editor: {}", e);
                return Err(ExitKind::Generic);
            }
        };
    }

    if combined.trim().is_empty() {
        eprintln!(
            "cgpt: no prompt given.\n\
             Usage: cgpt ask \"<prompt>\"\n\
             Or pipe text on stdin, e.g.:\n\
               echo \"hello\" | cgpt ask\n\
               cargo test 2>&1 | cgpt ask \"explain this failure\"\n\
             Or read from the OS clipboard / open editor:\n\
               cgpt ask --buffer\n\
               cgpt ask --editor"
        );
        return Err(ExitKind::Usage);
    }

    Ok(combined)
}
