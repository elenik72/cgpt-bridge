//! TASK-033a: what a person gives `cgpt ask` and `cgpt agent` (words, stdin)
//! is redacted once, before it reaches the socket and before it is written
//! under `.cgpt-bridge/`. A mock host on a temp socket records the frames.
//! The secrets below are made-up placeholders.

mod common;

use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;

use cgpt_bridge_protocol::agent::AGENT_FENCE_INFO;
use cgpt_bridge_protocol::{accept, listen, read_frame, release, write_frame, BridgeResponse};

const FAKE_KEY: &str = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
const FAKE_PASSWORD: &str = "hunter2-very-secret";
const CHAT: &str = "https://chatgpt.com/c/aaaaaaaa-0000";

fn endpoint(label: &str) -> String {
    format!("/tmp/cgb-ir-{}-{}.sock", label, std::process::id())
}

fn scratch_dir(label: &str) -> PathBuf {
    let p = PathBuf::from(format!("/tmp/cgb-ir-{}-{}", label, std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Mock host: one connection per reply, in order; every request frame is
/// kept and returned when the host is joined. `join` wakes a host that is
/// still waiting for a connection, so a CLI that never connects fails the
/// test instead of hanging it.
fn spawn_host(ep: String, replies: Vec<String>) -> common::Host<Vec<serde_json::Value>> {
    let listener = listen(&ep).expect("listen");
    let wake = ep.clone();
    let handle = thread::spawn(move || {
        let mut frames = Vec::new();
        for text in replies {
            let stream = accept(&listener).expect("accept");
            let mut reader = BufReader::new(&stream);
            let mut writer = BufWriter::new(&stream);
            let frame = read_frame(&mut reader).expect("read_frame");
            let req: serde_json::Value = serde_json::from_slice(&frame).unwrap();
            let reply = BridgeResponse::AskResult {
                id: req["id"].as_str().unwrap().to_string(),
                text,
                conversation_url: Some(CHAT.to_string()),
                warning: None,
            };
            write_frame(&mut writer, &serde_json::to_vec(&reply).unwrap()).unwrap();
            writer.flush().unwrap();
            frames.push(req);
        }
        release(&ep);
        frames
    });
    common::Host::new(wake, handle)
}

fn final_reply() -> String {
    let body = serde_json::json!({
        "version": 1,
        "status": "final",
        "user_message": "ALL DONE",
        "plan_update": { "summary": "", "events": [] },
        "command": serde_json::Value::Null,
    });
    format!("```{}\n{}\n```\n", AGENT_FENCE_INFO, body)
}

fn run_cgpt(dir: &Path, ep: &str, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cgpt"))
        .args(["--socket", ep, "--no-spinner"])
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cgpt");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .expect("write the input to the cgpt child (it exited early?)");
    child.wait_with_output().expect("wait cgpt")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn piped_input() -> String {
    format!("API_KEY={FAKE_PASSWORD}\nkey in a note: {FAKE_KEY}\n")
}

/// Every file below `dir`, read as text.
fn files_under(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(files_under(&path));
        } else {
            let body = std::fs::read_to_string(&path).unwrap_or_default();
            out.push((path, body));
        }
    }
    out
}

#[test]
fn ask_sends_redacted_stdin_and_names_the_rules_on_stderr() {
    let dir = scratch_dir("ask");
    let ep = endpoint("ask");
    let host = spawn_host(ep.clone(), vec!["an answer".into()]);
    let out = run_cgpt(&dir, &ep, &["ask", "explain"], &piped_input());
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    let frames = host.join().expect("mock host panicked");
    let sent = frames[0]["text"].as_str().unwrap();
    assert!(sent.starts_with("explain\n\n"), "{}", sent);
    assert!(sent.contains("«redacted:"), "{}", sent);
    assert!(!sent.contains(FAKE_PASSWORD), "{}", sent);
    assert!(!sent.contains(FAKE_KEY), "{}", sent);

    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("cgpt: input redacted before sending (rules: "),
        "{}",
        stderr
    );
    assert!(stderr.contains("openai_api_key"), "{}", stderr);
    assert!(stderr.contains("--no-redact"), "{}", stderr);
    assert!(!stderr.contains(FAKE_PASSWORD), "{}", stderr);
    assert!(!stderr.contains(FAKE_KEY), "{}", stderr);
    // stdout is still the answer and nothing else.
    assert_eq!(text(&out.stdout), "an answer\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ask_without_secrets_sends_the_text_as_is_and_prints_no_notice() {
    let dir = scratch_dir("clean");
    let ep = endpoint("clean");
    let host = spawn_host(ep.clone(), vec!["ok".into()]);
    let plain = "let password = read_password()?;\n";
    let out = run_cgpt(&dir, &ep, &["ask", "review"], plain);
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    let frames = host.join().expect("mock host panicked");
    assert_eq!(
        frames[0]["text"].as_str().unwrap(),
        format!("review\n\n{plain}")
    );
    assert!(!text(&out.stderr).contains("redacted"), "{:?}", out);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ask_no_redact_sends_the_text_as_is_and_prints_no_notice() {
    let dir = scratch_dir("noredact");
    let ep = endpoint("noredact");
    let host = spawn_host(ep.clone(), vec!["ok".into()]);
    let out = run_cgpt(
        &dir,
        &ep,
        &["ask", "--no-redact", "explain"],
        &piped_input(),
    );
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    let frames = host.join().expect("mock host panicked");
    let sent = frames[0]["text"].as_str().unwrap();
    assert!(sent.contains(FAKE_PASSWORD), "{}", sent);
    assert!(sent.contains(FAKE_KEY), "{}", sent);
    assert!(!text(&out.stderr).contains("redacted"), "{:?}", out);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agent_task_secret_reaches_neither_the_frame_nor_any_file_in_cgpt_bridge() {
    let dir = scratch_dir("agent");
    let ep = endpoint("agent");
    let host = spawn_host(ep.clone(), vec![final_reply()]);
    let out = run_cgpt(
        &dir,
        &ep,
        &["agent", "--no-pretty", "fix it"],
        &piped_input(),
    );
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    let frames = host.join().expect("mock host panicked");
    let sent = frames[0]["text"].as_str().unwrap();
    assert!(sent.contains("«redacted:"), "{}", sent);
    assert!(!sent.contains(FAKE_PASSWORD), "{}", sent);
    assert!(!sent.contains(FAKE_KEY), "{}", sent);
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("input redacted before sending"),
        "{}",
        stderr
    );
    assert!(!stderr.contains(FAKE_KEY), "{}", stderr);

    let files = files_under(&dir.join(".cgpt-bridge"));
    let names: Vec<String> = files
        .iter()
        .map(|(p, _)| {
            // Forward slashes on every OS, so the expected names below match on Windows.
            p.strip_prefix(&dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    // The files the card names, plus the per-session one: all of them exist.
    for want in [
        ".cgpt-bridge/plan.jsonl",
        ".cgpt-bridge/plan.md",
        ".cgpt-bridge/session.json",
    ] {
        assert!(names.iter().any(|n| n == want), "{} in {:?}", want, names);
    }
    for want in ["meta.json", "session.json"] {
        assert!(
            names
                .iter()
                .any(|n| n.starts_with(".cgpt-bridge/runs/") && n.ends_with(want)),
            "runs/<id>/{} in {:?}",
            want,
            names
        );
    }
    for (path, body) in &files {
        assert!(
            !body.contains(FAKE_PASSWORD),
            "{}: {}",
            path.display(),
            body
        );
        assert!(!body.contains(FAKE_KEY), "{}: {}", path.display(), body);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agent_resume_text_is_redacted_too() {
    let dir = scratch_dir("resume");
    let ep = endpoint("resume");
    let host = spawn_host(ep.clone(), vec![final_reply(), final_reply()]);
    let first = run_cgpt(&dir, &ep, &["agent", "--no-pretty", "start"], "");
    assert_eq!(first.status.code(), Some(0), "{:?}", first);
    let session = std::fs::read_dir(dir.join(".cgpt-bridge/runs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .into_string()
        .unwrap();
    let second = run_cgpt(
        &dir,
        &ep,
        &["agent", "--no-pretty", "--resume", &session],
        &piped_input(),
    );
    assert_eq!(second.status.code(), Some(0), "{:?}", second);
    let frames = host.join().expect("mock host panicked");
    let sent = frames[1]["text"].as_str().unwrap();
    assert!(sent.contains("«redacted:"), "{}", sent);
    assert!(!sent.contains(FAKE_PASSWORD), "{}", sent);
    assert!(!sent.contains(FAKE_KEY), "{}", sent);
    for (path, body) in files_under(&dir.join(".cgpt-bridge")) {
        assert!(!body.contains(FAKE_KEY), "{}: {}", path.display(), body);
        assert!(
            !body.contains(FAKE_PASSWORD),
            "{}: {}",
            path.display(),
            body
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agent_continue_text_is_redacted_too() {
    let dir = scratch_dir("continue");
    let ep = endpoint("continue");
    let host = spawn_host(ep.clone(), vec![final_reply(), final_reply()]);
    let first = run_cgpt(&dir, &ep, &["agent", "--no-pretty", "start"], "");
    assert_eq!(first.status.code(), Some(0), "{:?}", first);
    let second = run_cgpt(
        &dir,
        &ep,
        &["agent", "--no-pretty", "--continue"],
        &piped_input(),
    );
    assert_eq!(second.status.code(), Some(0), "{:?}", second);
    let frames = host.join().expect("mock host panicked");

    let sent = frames[1]["text"].as_str().unwrap();
    assert!(sent.contains("«redacted:"), "{}", sent);
    assert!(!sent.contains(FAKE_PASSWORD), "{}", sent);
    assert!(!sent.contains(FAKE_KEY), "{}", sent);
    assert!(text(&second.stderr).contains("input redacted before sending"));
    for (path, body) in files_under(&dir.join(".cgpt-bridge")) {
        assert!(!body.contains(FAKE_KEY), "{}: {}", path.display(), body);
        assert!(
            !body.contains(FAKE_PASSWORD),
            "{}: {}",
            path.display(),
            body
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agent_no_redact_sends_and_stores_the_task_as_typed_without_a_notice() {
    let dir = scratch_dir("agentraw");
    let ep = endpoint("agentraw");
    let host = spawn_host(ep.clone(), vec![final_reply()]);
    let out = run_cgpt(
        &dir,
        &ep,
        &["agent", "--no-pretty", "--no-redact", "fix it"],
        &piped_input(),
    );
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    let frames = host.join().expect("mock host panicked");

    let sent = frames[0]["text"].as_str().unwrap();
    assert!(sent.contains(FAKE_PASSWORD), "{}", sent);
    assert!(sent.contains(FAKE_KEY), "{}", sent);
    assert!(!text(&out.stderr).contains("input redacted"), "{:?}", out);
    let plan = std::fs::read_to_string(dir.join(".cgpt-bridge/plan.jsonl")).unwrap();
    assert!(plan.contains(FAKE_KEY), "{}", plan);
    let _ = std::fs::remove_dir_all(&dir);
}
