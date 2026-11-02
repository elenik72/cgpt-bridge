//! AUD-009: `cgpt agent` stops when a reply comes from another chat than the
//! one the session started in, before it parses the reply or runs its
//! command. A mock host on a temp socket serves the replies in turn.

use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;

mod common;

use cgpt_bridge_protocol::agent::AGENT_FENCE_INFO;
use cgpt_bridge_protocol::{accept, listen, read_frame, release, write_frame, BridgeResponse};

const CHAT_A: &str = "https://chatgpt.com/c/aaaaaaaa-0000";
const CHAT_B: &str = "https://chatgpt.com/c/bbbbbbbb-0000";

fn scratch_dir(label: &str) -> PathBuf {
    let p = PathBuf::from(format!("/tmp/cgb-ac-{}-{}", label, std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn endpoint(label: &str) -> String {
    format!("/tmp/cgb-ac-{}-{}.sock", label, std::process::id())
}

/// An agent reply that proposes a command (`continue`) or ends the run.
fn reply_text(status: &str, user_message: &str, command_id: Option<&str>) -> String {
    let command = match command_id {
        Some(id) => serde_json::json!({
            "id": id,
            "kind": "shell",
            "description": "list",
            "cwd": ".",
            "command": "ls",
            "expected_effect": "none",
            "risk": "read_only",
            "timeout_ms": 5000,
            "send_output": "summary",
        }),
        None => serde_json::Value::Null,
    };
    let body = serde_json::json!({
        "version": 1,
        "status": status,
        "user_message": user_message,
        "plan_update": { "summary": "", "events": [] },
        "command": command,
    });
    format!("```{}\n{}\n```\n", AGENT_FENCE_INFO, body)
}

/// Mock host: one connection per reply, in order; the reply carries the
/// given conversation URL. Returns after the last reply was written.
fn spawn_host(ep: String, replies: Vec<(String, Option<&'static str>)>) -> common::Host<()> {
    let listener = listen(&ep).expect("listen");
    let wake = ep.clone();
    let handle = thread::spawn(move || {
        for (text, url) in replies {
            let stream = accept(&listener).expect("accept");
            let mut reader = BufReader::new(&stream);
            let mut writer = BufWriter::new(&stream);
            let req = read_frame(&mut reader).expect("read_frame");
            let id = serde_json::from_slice::<serde_json::Value>(&req).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_string();
            let reply = BridgeResponse::AskResult {
                id,
                text,
                conversation_url: url.map(str::to_string),
                warning: None,
            };
            write_frame(&mut writer, &serde_json::to_vec(&reply).unwrap()).unwrap();
            writer.flush().unwrap();
        }
        release(&ep);
    });
    common::Host::new(wake, handle)
}

fn run_agent(dir: &Path, ep: &str, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cgpt"))
        .args(["--socket", ep, "agent"])
        .args(extra)
        .arg("go")
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .expect("spawn cgpt")
}

fn plan_jsonl(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(".cgpt-bridge/plan.jsonl")).unwrap_or_default()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn second_reply_from_another_chat_stops_before_the_command() {
    let dir = scratch_dir("changed");
    let ep = endpoint("changed");
    let host = spawn_host(
        ep.clone(),
        vec![
            (reply_text("continue", "first", Some("c1")), Some(CHAT_A)),
            (reply_text("continue", "STRANGER", Some("c2")), Some(CHAT_B)),
        ],
    );
    let out = run_agent(&dir, &ep, &[]);
    host.join().expect("mock host panicked");

    assert_eq!(out.status.code(), Some(5), "{:?}", out);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(stderr.contains("chat changed"), "{}", stderr);
    assert!(
        stderr.contains(CHAT_A) && stderr.contains(CHAT_B),
        "{}",
        stderr
    );
    assert!(stderr.contains("--resume s"), "{}", stderr);
    // The stranger's reply was neither shown nor parsed into the plan, and
    // its command never got as far as a proposal.
    assert!(!stdout.contains("STRANGER"), "{}", stdout);
    let plan = plan_jsonl(&dir);
    assert!(plan.contains("\"c1\""), "{}", plan);
    assert!(!plan.contains("\"c2\""), "{}", plan);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn second_reply_from_the_same_chat_goes_on() {
    let dir = scratch_dir("same");
    let ep = endpoint("same");
    let host = spawn_host(
        ep.clone(),
        vec![
            (reply_text("continue", "first", Some("c1")), Some(CHAT_A)),
            (reply_text("final", "ALL DONE", None), Some(CHAT_A)),
        ],
    );
    let out = run_agent(&dir, &ep, &["--no-pretty"]);
    host.join().expect("mock host panicked");

    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    assert!(text(&out.stdout).contains("ALL DONE"), "{:?}", out);
    let session = std::fs::read_to_string(dir.join(".cgpt-bridge/session.json")).unwrap();
    assert!(session.contains(CHAT_A), "{}", session);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reply_without_an_address_after_a_known_chat_stops() {
    // The page went back to a new, unsaved chat: not the chat we started in.
    let dir = scratch_dir("lost");
    let ep = endpoint("lost");
    let host = spawn_host(
        ep.clone(),
        vec![
            (reply_text("continue", "first", Some("c1")), Some(CHAT_A)),
            (reply_text("final", "STRANGER", None), None),
        ],
    );
    let out = run_agent(&dir, &ep, &[]);
    host.join().expect("mock host panicked");
    assert_eq!(out.status.code(), Some(5), "{:?}", out);
    assert!(!text(&out.stdout).contains("STRANGER"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn continue_in_another_chat_stops_on_the_first_reply() {
    let dir = scratch_dir("resume");
    // First run: ends in CHAT_A and records it.
    let ep1 = endpoint("resume1");
    let host = spawn_host(
        ep1.clone(),
        vec![(reply_text("needs_user_input", "q?", None), Some(CHAT_A))],
    );
    let out = run_agent(&dir, &ep1, &[]);
    host.join().unwrap();
    assert_eq!(out.status.code(), Some(0), "{:?}", out);

    // Follow-up with --continue, but the reply comes from CHAT_B.
    let ep2 = endpoint("resume2");
    let host = spawn_host(
        ep2.clone(),
        vec![(reply_text("final", "STRANGER", None), Some(CHAT_B))],
    );
    let out = run_agent(&dir, &ep2, &["--continue"]);
    host.join().unwrap();
    assert_eq!(out.status.code(), Some(5), "{:?}", out);
    assert!(!text(&out.stdout).contains("STRANGER"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn old_session_json_without_the_field_continues_and_adopts_the_chat() {
    let dir = scratch_dir("old");
    let ep1 = endpoint("old1");
    let host = spawn_host(
        ep1.clone(),
        vec![(reply_text("needs_user_input", "q?", None), None)],
    );
    let out = run_agent(&dir, &ep1, &[]);
    host.join().unwrap();
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    // Nothing was recorded: the page never left the temporary URL.
    let session = std::fs::read_to_string(dir.join(".cgpt-bridge/session.json")).unwrap();
    assert!(!session.contains("conversation_url"), "{}", session);

    let ep2 = endpoint("old2");
    let host = spawn_host(
        ep2.clone(),
        vec![(reply_text("final", "ALL DONE", None), Some(CHAT_B))],
    );
    let out = run_agent(&dir, &ep2, &["--continue", "--no-pretty"]);
    host.join().unwrap();
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    assert!(text(&out.stdout).contains("ALL DONE"));
    let session = std::fs::read_to_string(dir.join(".cgpt-bridge/session.json")).unwrap();
    assert!(session.contains(CHAT_B), "{}", session);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hostile_address_is_escaped_in_the_stop_message() {
    let dir = scratch_dir("hostile");
    let ep = endpoint("hostile");
    let host = spawn_host(
        ep.clone(),
        vec![
            (reply_text("continue", "first", Some("c1")), Some(CHAT_A)),
            (
                reply_text("final", "x", None),
                Some("https://chatgpt.com/c/x\r\u{1b}]0;pwned\u{7}"),
            ),
        ],
    );
    let out = run_agent(&dir, &ep, &[]);
    host.join().unwrap();
    assert_eq!(out.status.code(), Some(5), "{:?}", out);
    assert!(
        !out.stderr.contains(&0x0d) && !out.stderr.contains(&0x1b),
        "{:?}",
        text(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The session id a run printed on stderr (`cgpt: agent session <id> (...)`).
fn session_id_of(out: &Output) -> String {
    let stderr = text(&out.stderr);
    let rest = stderr
        .split("agent session ")
        .nth(1)
        .unwrap_or_else(|| panic!("no session line: {}", stderr));
    rest.split_whitespace().next().unwrap().to_string()
}

/// Run a first turn that ends the run in `chat`; returns the session id.
fn first_run_in(dir: &Path, label: &str, chat: Option<&'static str>) -> String {
    let ep = endpoint(label);
    let host = spawn_host(
        ep.clone(),
        vec![(reply_text("needs_user_input", "q?", None), chat)],
    );
    let out = run_agent(dir, &ep, &[]);
    host.join().unwrap();
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    session_id_of(&out)
}

#[test]
fn resume_of_an_older_session_uses_that_sessions_own_chat() {
    let dir = scratch_dir("two");
    let a = first_run_in(&dir, "two-a", Some(CHAT_A));
    // B is the latest session now and lives in another chat.
    let _b = first_run_in(&dir, "two-b", Some(CHAT_B));

    // A reply from A's own chat goes on, with no warning about the address.
    let ep = endpoint("two-ok");
    let host = spawn_host(
        ep.clone(),
        vec![(reply_text("final", "ALL DONE", None), Some(CHAT_A))],
    );
    let out = run_agent(&dir, &ep, &["--resume", &a, "--no-pretty"]);
    host.join().unwrap();
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    assert!(text(&out.stdout).contains("ALL DONE"), "{:?}", out);
    let stderr = text(&out.stderr);
    assert!(!stderr.contains("cannot record"), "{}", stderr);
    assert!(!stderr.contains("no chat address"), "{}", stderr);

    // A reply from B's chat is not A's chat: the run stops.
    let ep = endpoint("two-bad");
    let host = spawn_host(
        ep.clone(),
        vec![(reply_text("final", "STRANGER", None), Some(CHAT_B))],
    );
    let out = run_agent(&dir, &ep, &["--resume", &a]);
    host.join().unwrap();
    assert_eq!(out.status.code(), Some(5), "{:?}", out);
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains(CHAT_A) && stderr.contains(CHAT_B),
        "{}",
        stderr
    );
    // The hint names this very session, not the latest one.
    assert!(
        stderr.contains(&format!("`cgpt agent --resume {} <follow-up>`", a)),
        "{}",
        stderr
    );
    assert!(!stderr.contains("--continue"), "{}", stderr);
    assert!(!text(&out.stdout).contains("STRANGER"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_resume_id_that_is_a_path_is_refused() {
    let dir = scratch_dir("badid");
    let _a = first_run_in(&dir, "badid-a", Some(CHAT_A));
    let before = std::fs::read_to_string(dir.join(".cgpt-bridge/session.json")).unwrap();
    // The socket is never reached: no mock host is listening.
    for bad in ["..", "../x", "a/b"] {
        let out = run_agent(&dir, &endpoint("badid-none"), &["--resume", bad]);
        assert_eq!(out.status.code(), Some(3), "{}: {:?}", bad, out);
        let stderr = text(&out.stderr);
        assert!(stderr.contains("invalid session id"), "{}: {}", bad, stderr);
    }
    let after = std::fs::read_to_string(dir.join(".cgpt-bridge/session.json")).unwrap();
    assert_eq!(before, after);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_of_an_older_session_without_an_address_says_so_once() {
    let dir = scratch_dir("noaddr");
    let a = first_run_in(&dir, "noaddr-a", None);
    let _b = first_run_in(&dir, "noaddr-b", Some(CHAT_B));

    let ep = endpoint("noaddr-r");
    let host = spawn_host(
        ep.clone(),
        vec![(reply_text("final", "ALL DONE", None), Some(CHAT_A))],
    );
    let out = run_agent(&dir, &ep, &["--resume", &a, "--no-pretty"]);
    host.join().unwrap();
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    let stderr = text(&out.stderr);
    assert_eq!(stderr.matches("no chat address").count(), 1, "{}", stderr);
    assert!(!stderr.contains("cannot record"), "{}", stderr);
    // The chat of the reply became A's chat; B's record is untouched.
    let again = stored_chat_of(&dir, &a);
    assert_eq!(again, Some(CHAT_A.to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The chat address stored for a session, read from its own run folder.
fn stored_chat_of(dir: &Path, id: &str) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join(".cgpt-bridge/runs").join(id).join("session.json"))
        .ok()?;
    serde_json::from_str::<serde_json::Value>(&raw).ok()?["conversation_url"]
        .as_str()
        .map(str::to_string)
}

#[test]
fn stopping_an_older_session_leaves_the_latest_sessions_status_alone() {
    let dir = scratch_dir("status");
    let a = first_run_in(&dir, "status-a", Some(CHAT_A));
    let b = first_run_in(&dir, "status-b", Some(CHAT_B));

    let ep = endpoint("status-r");
    let host = spawn_host(
        ep.clone(),
        vec![(reply_text("final", "ALL DONE", None), Some(CHAT_A))],
    );
    let out = run_agent(&dir, &ep, &["--resume", &a, "--no-pretty"]);
    host.join().unwrap();
    assert_eq!(out.status.code(), Some(0), "{:?}", out);

    let latest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join(".cgpt-bridge/session.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(latest["session_id"], b.as_str());
    assert_eq!(latest["status"], "needs_user_input");
    assert_eq!(latest["conversation_url"], CHAT_B);
    let _ = std::fs::remove_dir_all(&dir);
}
