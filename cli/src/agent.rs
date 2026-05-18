//! `cgpt agent` interactive loop.
//!
//! Glues together the agent protocol parser, plan storage, and command
//! runner. The transport (UDS → host → extension → tab) is the same one
//! `cgpt ask` uses; the agent loop just invokes it repeatedly with a
//! schema-enforcing prompt contract on the first turn and a
//! `cgpt-command-result-v1` body on subsequent turns.
//!
//! High-level flow per turn:
//!
//! ```text
//! turn 0: prompt = AGENT_PROMPT_CONTRACT + "\n\n" + user task
//! loop:
//!   send prompt → assistant text
//!   parse → ParsedAgentResponse | repair-and-retry-once | abort
//!   apply plan_update
//!   print user_message
//!   if command is None:
//!     status=final     -> exit 0
//!     status=blocked   -> exit (non-zero)
//!     status=needs_user_input -> pause, ask the user to answer in browser
//!     status=continue  -> wait for next user turn? (we exit; CLI is one-shot)
//!   else:
//!     confirm + run via runner → CommandResultV1
//!     prompt = format_command_result(result)
//! ```

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use cgpt_bridge_protocol::agent::{
    format_command_result, parse_agent_response_with_cap, Status, AGENT_PROMPT_CONTRACT,
    DEFAULT_TIMEOUT_CAP_MS, REPAIR_PROMPT,
};
use cgpt_bridge_protocol::{AskRequest, BridgeResponse};

use crate::args::AgentArgs;
use crate::clipboard;
use crate::editor;
use crate::plan::{new_session_id, PlanStore};
use crate::render;
use crate::runner::{prompt_and_run, RunConfig, RunOutcome};
use crate::spinner::Phase;
use crate::transport::{ask_once, new_request_id, resolve_socket_path, AskOutcome};

#[repr(u8)]
pub enum AgentExitKind {
    Ok = 0,
    Generic = 1,
    Setup = 3,
    Transport = 4,
    Tab = 5,
    Dom = 6,
    Protocol = 7,
    Policy = 8,
    UserCancelled = 10,
}

pub fn run(args: AgentArgs, socket_override: Option<PathBuf>) -> u8 {
    let project_root = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cgpt: cannot get current dir: {}", e);
            return AgentExitKind::Generic as u8;
        }
    };

    let task = match collect_task(&args.task, args.buffer, args.editor) {
        Ok(t) => t,
        Err(code) => return code as u8,
    };

    // Resolve session id and decide whether to skip the prompt contract.
    // Priority: --resume <id>  > --continue  > fresh session.
    let resume_id: Option<String> = if let Some(id) = args.resume.clone() {
        Some(id)
    } else if args.continue_session {
        match PlanStore::latest_session_id(&project_root) {
            Ok(Some(id)) => Some(id),
            Ok(None) => {
                eprintln!(
                    "cgpt: --continue: no prior session found in {}",
                    project_root.display()
                );
                return AgentExitKind::Setup as u8;
            }
            Err(e) => {
                eprintln!("cgpt: --continue: cannot read sessions: {}", e);
                return AgentExitKind::Setup as u8;
            }
        }
    } else {
        None
    };

    let (plan, resuming) = match &resume_id {
        Some(id) => match PlanStore::open_existing(&project_root, id.clone()) {
            Ok(p) => (p, true),
            Err(e) => {
                eprintln!("cgpt: cannot resume session `{}`: {}", id, e);
                return AgentExitKind::Setup as u8;
            }
        },
        None => match PlanStore::open(&project_root, new_session_id(), &task) {
            Ok(p) => (p, false),
            Err(e) => {
                eprintln!("cgpt: cannot initialise .cgpt-bridge/: {}", e);
                return AgentExitKind::Setup as u8;
            }
        },
    };
    eprintln!(
        "cgpt: agent session {} (project {}){}",
        plan.session_id(),
        project_root.display(),
        if resuming { " — resuming" } else { "" }
    );
    if args.yolo {
        eprintln!(
            "⚠  --yolo enabled: every non-blocked command will run without a keypress.\n\
             Denylist hits are still blocked. Use only in trusted sandboxes."
        );
    } else if args.auto_readonly {
        eprintln!(
            "ℹ  --auto-readonly enabled: commands the local classifier marks read-only will auto-run."
        );
    }

    let socket_path = match resolve_socket_path(socket_override.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cgpt: cannot resolve socket path: {}", e);
            return AgentExitKind::Setup as u8;
        }
    };

    // First turn: prepend the prompt contract on fresh sessions only. On
    // --continue / --resume the active ChatGPT tab still has the contract
    // in its conversation context, so repeating it would just burn tokens
    // and confuse the model.
    let mut next_prompt = if resuming {
        task.clone()
    } else {
        format!("{}\n\n{}", AGENT_PROMPT_CONTRACT, task)
    };
    let mut turn_index = 0u32;
    // What we are sending this turn — used purely for the user-visible
    // progress phase label. "initial task" on turn 1; flipped to
    // "command result" by the runner branch below; "repair prompt" when the
    // parser asks for a retry.
    let mut turn_purpose: &str = "initial task";

    loop {
        turn_index += 1;
        // Blank line between turns keeps the scrollback readable.
        if turn_index > 1 {
            eprintln!();
        }
        let assistant_text = match send_and_collect(
            &socket_path,
            &next_prompt,
            args.timeout_ms,
            turn_index,
            turn_purpose,
        ) {
            Ok(t) => t,
            Err(code) => return code as u8,
        };

        let _ = plan.transcript_append(&serde_json::json!({
            "turn": turn_index,
            "kind": "assistant_message",
            "text_len": assistant_text.len(),
            "at_unix_ms": now_ms(),
        }));

        let parsed = match parse_agent_response_with_cap(&assistant_text, DEFAULT_TIMEOUT_CAP_MS) {
            Ok(p) => p,
            Err(first_err) => {
                eprintln!("cgpt: protocol violation: {}", first_err);
                // One repair attempt per §6.
                let repair_text = match send_and_collect(
                    &socket_path,
                    REPAIR_PROMPT,
                    args.timeout_ms,
                    turn_index,
                    "repair prompt",
                ) {
                    Ok(t) => t,
                    Err(code) => return code as u8,
                };
                match parse_agent_response_with_cap(&repair_text, DEFAULT_TIMEOUT_CAP_MS) {
                    Ok(p) => p,
                    Err(second_err) => {
                        eprintln!(
                            "cgpt: repair attempt also failed: {}. Aborting.",
                            second_err
                        );
                        let _ = plan.update_session_status("protocol_violation");
                        return AgentExitKind::Protocol as u8;
                    }
                }
            }
        };

        if let Some(clamp) = parsed.timeout_clamped {
            eprintln!(
                "cgpt: clamped command timeout {} ms -> {} ms",
                clamp.requested_ms, clamp.clamped_to_ms
            );
        }
        if parsed.unknown_event_count > 0 {
            eprintln!(
                "cgpt: dropped {} unknown plan event(s) (forward-compat)",
                parsed.unknown_event_count
            );
        }

        let mut response = parsed.response;
        // Defensive: some assistant responses double-escape newlines in
        // user_message (`\\n` instead of `\n`), which breaks rendering and
        // makes plan.md/final.md unreadable. Repair once here so every
        // downstream consumer sees real newlines.
        if let std::borrow::Cow::Owned(fixed) =
            render::repair_double_escaped(&response.user_message)
        {
            response.user_message = fixed;
        }
        if let Err(e) = plan.apply_plan_update(&response.plan_update) {
            eprintln!("cgpt: cannot write plan update: {}", e);
        }
        let is_final = matches!(response.status, Status::Final);
        if !response.user_message.is_empty() {
            if is_final && !args.no_pretty {
                render::print_markdown(&response.user_message);
            } else {
                println!("{}", response.user_message);
            }
        }

        match (response.status, response.command.clone()) {
            (Status::Final, _) => {
                let _ = plan.record_final(&response);
                let _ = plan.update_session_status("final");
                if args.copy && !response.user_message.is_empty() {
                    if let Err(e) = clipboard::write(&response.user_message) {
                        eprintln!("cgpt: --copy: clipboard write failed: {}", e);
                    }
                }
                return AgentExitKind::Ok as u8;
            }
            (Status::Blocked, _) => {
                eprintln!("cgpt: assistant reports `blocked`. Stopping.");
                let _ = plan.update_session_status("blocked");
                return AgentExitKind::Generic as u8;
            }
            (Status::NeedsUserInput, _) => {
                eprintln!(
                    "cgpt: assistant needs more information. Answer the question in the chat tab, then re-run `cgpt agent <follow-up>`."
                );
                let _ = plan.update_session_status("needs_user_input");
                return AgentExitKind::Ok as u8;
            }
            (Status::Continue, None) => {
                // The contract recommends pairing continue with a command;
                // if there's no command and no final, we have nothing to do
                // locally. Exit cleanly so the user can re-engage in browser.
                eprintln!(
                    "cgpt: assistant returned status=continue with no command. Nothing to do locally."
                );
                let _ = plan.update_session_status("idle");
                return AgentExitKind::Ok as u8;
            }
            (Status::Continue, Some(cmd)) => {
                if let Err(e) = plan.record_command_proposed(&cmd) {
                    eprintln!("cgpt: cannot record command_proposed: {}", e);
                }
                let cfg = RunConfig {
                    project_root: project_root.clone(),
                    timeout_ms: cmd.timeout_ms,
                    assume_no_tty: false,
                    auto_readonly: args.auto_readonly || args.yolo,
                    yolo: args.yolo,
                };
                let outcome = match prompt_and_run(&cmd, &cfg) {
                    Ok(o) => o,
                    Err(e) => {
                        eprintln!("cgpt: runner error: {}", e);
                        return AgentExitKind::Generic as u8;
                    }
                };

                let result_envelope = match outcome {
                    RunOutcome::Executed(r) => r,
                    RunOutcome::UserRejected(r) => r,
                    RunOutcome::PolicyBlocked(r) => {
                        eprintln!(
                            "cgpt: command policy-blocked; sending result back to assistant."
                        );
                        r
                    }
                    RunOutcome::Quit => {
                        eprintln!("cgpt: user quit.");
                        let _ = plan.record_user_cancelled("user_quit");
                        let _ = plan.update_session_status("user_cancelled");
                        return AgentExitKind::UserCancelled as u8;
                    }
                };
                if let Err(e) = plan.record_command_result(&result_envelope) {
                    eprintln!("cgpt: cannot persist command result: {}", e);
                }
                next_prompt = build_next_user_message(&result_envelope);
                turn_purpose = "command result";
            }
        }
    }
}

fn build_next_user_message(result: &cgpt_bridge_protocol::agent::CommandResultV1) -> String {
    let mut s = String::new();
    s.push_str("Command result follows. Continue per the contract.\n\n");
    s.push_str(&format_command_result(result));
    s
}

fn send_and_collect(
    socket_path: &std::path::Path,
    prompt: &str,
    timeout_ms: u64,
    turn_index: u32,
    purpose: &str,
) -> Result<String, AgentExitKind> {
    let req = AskRequest {
        id: new_request_id("agent"),
        text: prompt.to_string(),
        timeout_ms,
    };
    let phase = Phase::start(format!(
        "turn {} — sending {} to ChatGPT…",
        turn_index, purpose
    ));
    let outcome = ask_once(socket_path, &req, timeout_ms);

    match outcome {
        AskOutcome::Ok(BridgeResponse::AskResult { text, .. }) => {
            phase.done(format!(
                "turn {} — reply received ({} chars)",
                turn_index,
                text.chars().count()
            ));
            Ok(text)
        }
        AskOutcome::Ok(BridgeResponse::Pong { .. }) => {
            phase.fail("host returned pong (protocol bug)");
            Err(AgentExitKind::Protocol)
        }
        AskOutcome::Ok(BridgeResponse::Error { code, message, .. }) => {
            phase.fail(format!("host error: {:?}", code));
            eprintln!("cgpt: host error ({:?}): {}", code, message);
            Err(map_host_error(code))
        }
        AskOutcome::SocketMissing => {
            phase.fail("native host socket missing");
            eprintln!(
                "cgpt: native host socket not found at {}.\n\
                 Start Chrome with the cgpt-bridge extension installed and reload it.",
                socket_path.display()
            );
            Err(AgentExitKind::Setup)
        }
        AskOutcome::SocketIo(e) => {
            phase.fail(format!("transport error: {}", e));
            Err(AgentExitKind::Transport)
        }
        AskOutcome::Timeout => {
            phase.fail(format!("timed out after {}ms", timeout_ms));
            Err(AgentExitKind::Tab)
        }
        AskOutcome::BadResponse(msg) => {
            phase.fail(format!("bad host response: {}", msg));
            Err(AgentExitKind::Protocol)
        }
    }
}

fn map_host_error(code: cgpt_bridge_protocol::ErrorCode) -> AgentExitKind {
    use cgpt_bridge_protocol::ErrorCode;
    match code {
        ErrorCode::BadRequest => AgentExitKind::Generic,
        ErrorCode::OversizeFrame => AgentExitKind::Transport,
        ErrorCode::ExtensionUnavailable => AgentExitKind::Setup,
        ErrorCode::TabUnavailable => AgentExitKind::Tab,
        ErrorCode::DomFailure => AgentExitKind::Dom,
        ErrorCode::Timeout => AgentExitKind::Tab,
        ErrorCode::Internal => AgentExitKind::Generic,
    }
}

fn collect_task(
    positional: &[String],
    buffer: bool,
    editor_flag: bool,
) -> Result<String, AgentExitKind> {
    use std::io::IsTerminal;
    let from_args = if positional.is_empty() {
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
                return Err(AgentExitKind::Generic);
            }
        }
    } else if io::stdin().is_terminal() {
        None
    } else {
        let mut buf = String::new();
        if let Err(e) = io::stdin().lock().read_to_string(&mut buf) {
            eprintln!("cgpt: failed to read stdin: {}", e);
            return Err(AgentExitKind::Generic);
        }
        if buf.trim().is_empty() {
            None
        } else {
            Some(buf)
        }
    };
    let mut combined = match (from_args, secondary) {
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
                return Err(AgentExitKind::Generic);
            }
        };
    }
    if combined.trim().is_empty() {
        eprintln!(
            "cgpt: no task given.\n\
             Usage: cgpt agent \"<task description>\"\n\
             Or pipe a task on stdin, or read from the OS clipboard, or compose in $EDITOR:\n\
               cgpt agent --buffer\n\
               cgpt agent --editor\n\
               cgpt agent --continue \"follow-up question\""
        );
        return Err(AgentExitKind::Generic);
    }
    Ok(combined)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// Touch Write to keep the import non-dead when stdout/stderr writes are
// confined to println!/eprintln! macros.
#[allow(dead_code)]
fn _force_write_import(mut w: impl Write) {
    let _ = w.flush();
}
