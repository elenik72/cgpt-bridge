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

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use cgpt_bridge_protocol::agent::{
    format_command_result, parse_agent_response_with_cap, Status, AGENT_PROMPT_CONTRACT,
    DEFAULT_TIMEOUT_CAP_MS, REPAIR_PROMPT,
};
use cgpt_bridge_protocol::chat_url::chat_key;
use cgpt_bridge_protocol::{AskRequest, BridgeResponse};

use crate::args::AgentArgs;
use crate::clipboard;
use crate::exit::{
    command_result_too_large_message, interrupted_message, no_answer_message, too_large_message,
    ExitKind,
};
use crate::input;
use crate::plan::{new_session_id, PlanStore};
use crate::render;
use crate::runner::{escape_control, prompt_and_run, safe_text, RunConfig, RunOutcome};
use crate::spinner::Phase;
use crate::transport::{
    ask_once, new_request_id, project_for, resolve_socket_path, show_warning, AskOutcome,
};

pub fn run(args: AgentArgs, socket_override: Option<PathBuf>) -> u8 {
    let project_root = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cgpt: cannot get current dir: {}", e);
            return ExitKind::Generic.code();
        }
    };

    let task = match input::collect(
        &args.task,
        args.buffer,
        args.editor,
        !args.no_redact,
        TASK_USAGE,
    ) {
        Ok(t) => t,
        Err(code) => return code.code(),
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
                return ExitKind::Setup.code();
            }
            Err(e) => {
                eprintln!("cgpt: --continue: cannot read sessions: {}", e);
                return ExitKind::Setup.code();
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
                return ExitKind::Setup.code();
            }
        },
        None => match PlanStore::open(&project_root, new_session_id(), &task) {
            Ok(p) => (p, false),
            Err(e) => {
                eprintln!("cgpt: cannot initialise .cgpt-bridge/: {}", e);
                return ExitKind::Setup.code();
            }
        },
    };
    eprintln!(
        "cgpt: agent session {} (project {}){}",
        plan.session_id(),
        project_root.display(),
        if resuming { " — resuming" } else { "" }
    );
    if let Some(banner) = mode_banner(args.yolo, args.auto_readonly) {
        eprintln!("{}", escape_control(&banner));
    }

    let socket_path = match resolve_socket_path(socket_override.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cgpt: cannot resolve socket path: {}", e);
            return ExitKind::Setup.code();
        }
    };
    // A turn that opens a new chat puts it into this project; the turns
    // after it continue that chat, and the extension ignores the name there.
    let project = project_for(&project_root);

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
    // Commands auto-approved in a row under --auto-readonly; the runner
    // asks after `MAX_AUTO_APPROVALS_IN_A_ROW` and resets it on a manual
    // confirmation (AUD-012).
    let mut auto_streak = 0u32;
    // The chat the session is bound to: recorded in session.json from the
    // first reply that has an address, and checked on every later reply, so
    // a changed active tab cannot take the agent into someone else's chat
    // (AUD-009).
    let mut bound_chat: Option<String> = plan.conversation_url();
    if resuming && bound_chat.is_none() {
        eprintln!(
            "cgpt: no chat address is saved for this session (it ran before addresses were \
             stored, or none of its replies had one). The follow-up goes to the active \
             ChatGPT tab, that reply is not checked against a chat, and the chat it \
             comes from becomes this session's chat."
        );
    }

    loop {
        turn_index += 1;
        // Blank line between turns keeps the scrollback readable.
        if turn_index > 1 {
            eprintln!();
        }
        let assistant_text = match send_and_collect(
            &plan,
            &socket_path,
            &next_prompt,
            args.timeout_ms,
            turn_index,
            turn_purpose,
            project.as_deref(),
        )
        .and_then(|reply| check_chat(&plan, &mut bound_chat, reply))
        {
            Ok(t) => t,
            Err(code) => return code.code(),
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
                eprintln!(
                    "cgpt: protocol violation: {}",
                    escape_control(&first_err.to_string())
                );
                // One repair attempt per §6. It carries the contract again,
                // first, as on a first turn (the contract ends by announcing
                // what follows it): seen live, a --continue turn (sent without
                // it) and its repair both came back as prose.
                let repair_prompt = format!("{}\n\n{}", AGENT_PROMPT_CONTRACT, REPAIR_PROMPT);
                let repair_text = match send_and_collect(
                    &plan,
                    &socket_path,
                    &repair_prompt,
                    args.timeout_ms,
                    turn_index,
                    "repair prompt",
                    project.as_deref(),
                )
                .and_then(|reply| check_chat(&plan, &mut bound_chat, reply))
                {
                    Ok(t) => t,
                    Err(code) => return code.code(),
                };
                match parse_agent_response_with_cap(&repair_text, DEFAULT_TIMEOUT_CAP_MS) {
                    Ok(p) => p,
                    Err(second_err) => {
                        eprintln!(
                            "cgpt: repair attempt also failed: {}. Aborting.",
                            escape_control(&second_err.to_string())
                        );
                        let _ = plan.update_session_status("protocol_violation");
                        return ExitKind::Protocol.code();
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
            // Escape before highlighting: the model controls this text and
            // must not be able to drive the terminal with it.
            let shown = safe_text(&response.user_message);
            if is_final && !args.no_pretty {
                render::print_markdown(&shown);
            } else {
                println!("{}", shown);
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
                return ExitKind::Ok.code();
            }
            (Status::Blocked, _) => {
                eprintln!("cgpt: assistant reports `blocked`. Stopping.");
                let _ = plan.update_session_status("blocked");
                return ExitKind::Generic.code();
            }
            (Status::NeedsUserInput, _) => {
                eprintln!(
                    "cgpt: assistant needs more information. Answer the question in the chat tab, then re-run `cgpt agent <follow-up>`."
                );
                let _ = plan.update_session_status("needs_user_input");
                return ExitKind::Ok.code();
            }
            (Status::Continue, None) => {
                // The contract recommends pairing continue with a command;
                // if there's no command and no final, we have nothing to do
                // locally. Exit cleanly so the user can re-engage in browser.
                eprintln!(
                    "cgpt: assistant returned status=continue with no command. Nothing to do locally."
                );
                let _ = plan.update_session_status("idle");
                return ExitKind::Ok.code();
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
                let outcome = match prompt_and_run(&cmd, &cfg, &mut auto_streak) {
                    Ok(o) => o,
                    Err(e) => {
                        eprintln!("cgpt: runner error: {}", e);
                        return ExitKind::Generic.code();
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
                        return ExitKind::UserCancelled.code();
                    }
                };
                if let Err(e) = plan.record_command_result(&result_envelope) {
                    eprintln!("cgpt: cannot persist command result: {}", e);
                }
                next_prompt = build_next_user_message(&result_envelope);
                turn_purpose = COMMAND_RESULT;
            }
        }
    }
}

/// The purpose of a turn that sends a command's result (shown in its phase
/// line, and choosing the too-large message).
const COMMAND_RESULT: &str = "command result";

/// The banner of an auto-approving mode, `None` without one. Both modes run
/// commands unasked, and that output goes to ChatGPT (AUD-012).
fn mode_banner(yolo: bool, auto_readonly: bool) -> Option<String> {
    const OUTPUT_LINE: &str =
        "Output of auto-run commands is sent to ChatGPT after pattern-based redaction.";
    let head = if yolo {
        "⚠  --yolo enabled: every non-blocked command will run without a keypress.\n\
         Denylist hits are still blocked. Use only in trusted sandboxes."
    } else if auto_readonly {
        "ℹ  --auto-readonly enabled: read-only commands on the narrow auto-run list will auto-run."
    } else {
        return None;
    };
    Some(format!("{}\n{}", head, OUTPUT_LINE))
}

fn build_next_user_message(result: &cgpt_bridge_protocol::agent::CommandResultV1) -> String {
    let mut s = String::new();
    s.push_str("Command result follows. Continue per the contract.\n\n");
    s.push_str(&format_command_result(result));
    s
}

/// A reply: its text and the chat it came from.
struct Reply {
    text: String,
    conversation_url: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum ChatCheck {
    /// Nothing known yet, or the same chat as before.
    Fine,
    /// First address of this session: remember it.
    Bind(String),
    /// Another chat than the one the session is bound to.
    Changed,
}

/// Compare a reply's chat with the one the session is bound to, by chat id
/// as the gateway and the extension do: the same chat in another address
/// form (a renamed project's slug, a trailing slash, a query) is the same
/// chat. An address that is gone after one was known means the tab went back
/// to a new chat.
fn compare_chat(bound: Option<&str>, reply: Option<&str>) -> ChatCheck {
    match (bound, reply) {
        (Some(b), Some(r)) if chat_key(b) == chat_key(r) => ChatCheck::Fine,
        (Some(_), _) => ChatCheck::Changed,
        (None, Some(r)) => ChatCheck::Bind(r.to_string()),
        (None, None) => ChatCheck::Fine,
    }
}

/// Run before the reply is parsed: a reply from another chat must not reach
/// the plan, the screen or the command runner.
fn check_chat(
    plan: &PlanStore,
    bound: &mut Option<String>,
    reply: Reply,
) -> Result<String, ExitKind> {
    match compare_chat(bound.as_deref(), reply.conversation_url.as_deref()) {
        ChatCheck::Fine => {}
        ChatCheck::Bind(url) => {
            if let Err(e) = plan.set_conversation_url(&url) {
                eprintln!("cgpt: cannot record the chat address: {}", e);
            }
            *bound = Some(url);
        }
        ChatCheck::Changed => {
            let show = |u: Option<&str>| match u {
                Some(u) => escape_control(u).into_owned(),
                None => "(a new chat without an address)".to_string(),
            };
            eprintln!(
                "cgpt: the chat changed: this session runs in {}, but the reply came from {}.\n\
                 The reply was dropped and nothing was run. Open the first chat again and \
                 re-run with `cgpt agent --resume {} <follow-up>`, or start a new session \
                 with `cgpt agent <task>`.",
                show(bound.as_deref()),
                show(reply.conversation_url.as_deref()),
                plan.session_id(),
            );
            let _ = plan.update_session_status("chat_changed");
            return Err(ExitKind::Tab);
        }
    }
    Ok(reply.text)
}

fn send_and_collect(
    plan: &PlanStore,
    socket_path: &str,
    prompt: &str,
    timeout_ms: u64,
    turn_index: u32,
    purpose: &str,
    project: Option<&str>,
) -> Result<Reply, ExitKind> {
    let req = AskRequest {
        id: new_request_id("agent"),
        text: prompt.to_string(),
        timeout_ms,
        target: None,
        project: project.map(str::to_string),
        capture_stream: false,
        attachments: Vec::new(),
    };
    let phase = Phase::start(format!(
        "turn {} — sending {} to ChatGPT…",
        turn_index, purpose
    ));
    let outcome = ask_once(socket_path, &req, timeout_ms);
    let kind = ExitKind::for_outcome(&outcome);

    match outcome {
        AskOutcome::Ok(BridgeResponse::AskResult {
            text,
            conversation_url,
            warning,
            ..
        }) => {
            phase.done(format!(
                "turn {} — reply received ({} chars)",
                turn_index,
                text.chars().count()
            ));
            show_warning(warning.as_deref());
            Ok(Reply {
                text,
                conversation_url,
            })
        }
        AskOutcome::Ok(
            BridgeResponse::Pong { .. }
            | BridgeResponse::Stopped { .. }
            | BridgeResponse::StatusResult { .. },
        ) => {
            phase.fail("host returned a non-ask response (protocol bug)");
            Err(kind)
        }
        AskOutcome::Ok(BridgeResponse::Error { code, message, .. }) => {
            phase.fail(format!("host error: {:?}", code));
            eprintln!(
                "cgpt: host error ({:?}): {}",
                code,
                escape_control(&message)
            );
            Err(kind)
        }
        AskOutcome::SocketMissing => {
            phase.fail("native host socket missing");
            eprintln!(
                "cgpt: native host socket not found at {}.\n\
                 Start Chrome with the cgpt-bridge extension installed and reload it.",
                socket_path
            );
            Err(kind)
        }
        AskOutcome::SocketIo(e) => {
            phase.fail(format!("transport error: {}", e));
            Err(kind)
        }
        AskOutcome::Timeout => {
            phase.fail(no_answer_message(timeout_ms));
            Err(kind)
        }
        AskOutcome::TooLarge { len, cap } => {
            phase.fail(if purpose == COMMAND_RESULT {
                command_result_too_large_message(len, cap)
            } else {
                too_large_message(len, cap)
            });
            Err(kind)
        }
        AskOutcome::BadResponse(msg) => {
            phase.fail(format!("bad host response: {}", msg));
            Err(kind)
        }
        AskOutcome::Interrupted(stop) => {
            phase.fail(interrupted_message(&stop));
            let _ = plan.record_user_cancelled("ctrl_c");
            let _ = plan.update_session_status("user_cancelled");
            crate::interrupt::die()
        }
    }
}

const TASK_USAGE: &str = "cgpt: no task given.\n\
     Usage: cgpt agent \"<task description>\"\n\
     Or pipe a task on stdin, or read from the OS clipboard, or compose in $EDITOR:\n\
       cgpt agent --buffer\n\
       cgpt agent --editor\n\
       cgpt agent --continue \"follow-up question\"";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // AUD-012: the mode banner says where auto-run output goes.
    #[test]
    fn auto_readonly_banner_says_output_goes_to_chatgpt() {
        let banner = mode_banner(false, true).expect("banner");
        assert!(banner.contains("--auto-readonly enabled"), "{}", banner);
        assert!(
            banner.lines().any(|l| l.contains(
                "Output of auto-run commands is sent to ChatGPT after pattern-based redaction."
            )),
            "{}",
            banner
        );
    }

    #[test]
    fn yolo_banner_also_says_output_goes_to_chatgpt() {
        // --yolo implies --auto-readonly; its banner wins.
        let banner = mode_banner(true, true).expect("banner");
        assert!(banner.contains("--yolo enabled"), "{}", banner);
        assert!(!banner.contains("--auto-readonly enabled"), "{}", banner);
        assert!(banner.contains("sent to ChatGPT"), "{}", banner);
    }

    // AUD-009: the chat check.
    #[test]
    fn compare_chat_binds_the_first_address_and_then_holds_it() {
        let a = "https://chatgpt.com/c/a";
        let b = "https://chatgpt.com/c/b";
        assert_eq!(compare_chat(None, Some(a)), ChatCheck::Bind(a.into()));
        assert_eq!(compare_chat(None, None), ChatCheck::Fine);
        assert_eq!(compare_chat(Some(a), Some(a)), ChatCheck::Fine);
        assert_eq!(compare_chat(Some(a), Some(b)), ChatCheck::Changed);
        assert_eq!(compare_chat(Some(a), None), ChatCheck::Changed);
    }

    // M4: by chat id, as the gateway's `chat_key` and the extension compare.
    #[test]
    fn compare_chat_holds_the_same_chat_in_another_address_form() {
        let bound = "https://chatgpt.com/g/g-p-0123abcd-app/c/6720aa";
        for same in [
            // The project renamed: its slug changed.
            "https://chatgpt.com/g/g-p-0123abcd-renamed-app/c/6720aa",
            "https://chatgpt.com/c/6720aa",
            "https://chatgpt.com/g/g-p-0123abcd-app/c/6720aa/",
            "https://chatgpt.com/g/g-p-0123abcd-app/c/6720aa?model=x#end",
        ] {
            assert_eq!(
                compare_chat(Some(bound), Some(same)),
                ChatCheck::Fine,
                "{same}"
            );
        }
        for other in [
            "https://chatgpt.com/g/g-p-0123abcd-app/c/6720bb",
            "https://chatgpt.com/c/local-chatgpt:7a1d",
            "https://chatgpt.com/g/g-p-0123abcd-app/project",
            "https://chatgpt.com/",
        ] {
            assert_eq!(
                compare_chat(Some(bound), Some(other)),
                ChatCheck::Changed,
                "{other}"
            );
        }
    }

    #[test]
    fn no_banner_without_an_auto_mode() {
        assert_eq!(mode_banner(false, false), None);
    }

    #[test]
    fn banner_is_terminal_safe() {
        for banner in [mode_banner(false, true), mode_banner(true, false)] {
            let banner = banner.unwrap();
            assert_eq!(escape_control(&banner), banner.as_str());
        }
    }
}
