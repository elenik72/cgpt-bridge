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

use std::process::ExitCode;

use cgpt_bridge_cli::exit::{interrupted_message, no_answer_message, too_large_message, ExitKind};
use cgpt_bridge_cli::{args, clipboard, history, input, spinner::Phase, transport};
use cgpt_bridge_protocol::{AskRequest, BridgeResponse};

use args::{Cli, Command};
use transport::AskOutcome;

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
                _ => ExitKind::Usage.code(),
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
    ExitCode::from(code.code())
}

fn run(cli: Cli) -> ExitKind {
    match cli.command {
        Command::Ask(ask) => run_ask(ask, &cli.socket_override),
        Command::Agent(agent_args) => {
            let code = cgpt_bridge_cli::agent::run(agent_args, cli.socket_override.clone());
            std::process::exit(code as i32);
        }
        Command::History(h) => std::process::exit(history::run_history(h) as i32),
        Command::Replay(r) => std::process::exit(history::run_replay(r) as i32),
        Command::Last(l) => std::process::exit(history::run_last(l) as i32),
    }
}

const ASK_USAGE: &str = "cgpt: no prompt given.\n\
     Usage: cgpt ask \"<prompt>\"\n\
     Or pipe text on stdin, e.g.:\n\
       echo \"hello\" | cgpt ask\n\
       cargo test 2>&1 | cgpt ask \"explain this failure\"\n\
     Or read from the OS clipboard / open editor:\n\
       cgpt ask --buffer\n\
       cgpt ask --editor";

fn run_ask(cmd: args::AskArgs, socket_override: &Option<std::path::PathBuf>) -> ExitKind {
    let prompt = match input::collect(
        &cmd.prompt,
        cmd.buffer,
        cmd.editor,
        !cmd.no_redact,
        ASK_USAGE,
    ) {
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

    let project = std::env::current_dir()
        .ok()
        .and_then(|dir| transport::project_for(&dir));
    let request = AskRequest {
        id: transport::new_request_id("ask"),
        text: prompt,
        timeout_ms: cmd.timeout_ms,
        target: None,
        project,
        capture_stream: false,
        attachments: Vec::new(),
    };

    let phase = Phase::start("asking ChatGPT…");
    let outcome = transport::ask_once(&socket_path, &request, cmd.timeout_ms);
    let kind = ExitKind::for_outcome(&outcome);

    match outcome {
        AskOutcome::Ok(BridgeResponse::AskResult { text, warning, .. }) => {
            phase.done(format!("reply received ({} chars)", text.chars().count()));
            transport::show_warning(warning.as_deref());
            // Stdout = assistant text only. Single trailing newline so
            // `cgpt ask "x" | wc -l` behaves like other CLIs.
            // Model text, so control characters are escaped the same way as
            // in `cgpt agent`; the clipboard keeps the answer as received.
            let shown = cgpt_bridge_cli::runner::safe_text(&text);
            print!("{}", shown);
            if !shown.ends_with('\n') {
                println!();
            }
            if cmd.copy {
                if let Err(e) = clipboard::write(&text) {
                    eprintln!("cgpt: --copy: clipboard write failed: {}", e);
                }
            }
            kind
        }
        AskOutcome::Ok(
            BridgeResponse::Pong { .. }
            | BridgeResponse::Stopped { .. }
            | BridgeResponse::StatusResult { .. },
        ) => {
            phase.fail("host returned a non-ask response (protocol bug)");
            eprintln!("cgpt: host returned a non-ask response for an ask request (protocol bug)");
            kind
        }
        AskOutcome::Ok(BridgeResponse::Error { code, message, .. }) => {
            phase.fail(format!("host error: {:?}", code));
            eprintln!(
                "cgpt: host error ({:?}): {}",
                code,
                cgpt_bridge_cli::runner::escape_control(&message)
            );
            kind
        }
        AskOutcome::SocketMissing => {
            phase.fail("native host socket missing");
            eprintln!(
                "cgpt: native host socket not found at {}.\n\
                 The native host is not running. Verify that:\n\
                 - The cgpt-bridge Chrome extension is installed and enabled.\n\
                 - Chrome is open with the extension active.\n\
                 - The native messaging manifest has been installed (macOS: install.sh\n\
                 or install/macos/install-host.sh; Linux: install/linux/install-host.sh).",
                socket_path,
            );
            kind
        }
        AskOutcome::SocketIo(e) => {
            phase.fail(format!("transport error: {}", e));
            kind
        }
        AskOutcome::Timeout => {
            phase.fail(no_answer_message(cmd.timeout_ms));
            kind
        }
        AskOutcome::TooLarge { len, cap } => {
            phase.fail(too_large_message(len, cap));
            kind
        }
        AskOutcome::BadResponse(msg) => {
            phase.fail(format!("bad host response: {}", msg));
            kind
        }
        AskOutcome::Interrupted(stop) => {
            phase.fail(interrupted_message(&stop));
            cgpt_bridge_cli::interrupt::die()
        }
    }
}
