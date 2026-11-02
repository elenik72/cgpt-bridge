//! PROJ: `cgpt ask` and `cgpt agent` name the ChatGPT project a new chat goes
//! into after the folder they run in (none in the home folder), and show the
//! extension's warning (say, the chat went to a plain new chat) on stderr. A
//! mock host on a temp socket records the frames.

mod common;

use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;

use cgpt_bridge_protocol::agent::AGENT_FENCE_INFO;
use cgpt_bridge_protocol::{accept, listen, read_frame, release, write_frame, BridgeResponse};

const CHAT: &str = "https://chatgpt.com/g/g-p-0123456789abcdef0123456789abcdef-proj/c/aaaa-0000";
const WARNING: &str = "ChatGPT project not found or created; the chat went to a plain new chat.";

fn endpoint(label: &str) -> String {
    format!("/tmp/cgb-pj-{}-{}.sock", label, std::process::id())
}

/// `/tmp/cgb-pj-<pid>/<name>`, empty.
fn work_dir(name: &str) -> PathBuf {
    let p = PathBuf::from(format!("/tmp/cgb-pj-{}", std::process::id())).join(name);
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Mock host: one connection per reply, in order, each answered with `text`
/// and `warning`; the request frames come back from `join`.
fn spawn_host(
    ep: String,
    replies: Vec<(String, Option<&'static str>)>,
) -> common::Host<Vec<serde_json::Value>> {
    let listener = listen(&ep).expect("listen");
    let wake = ep.clone();
    let handle = thread::spawn(move || {
        let mut frames = Vec::new();
        for (text, warning) in replies {
            let stream = accept(&listener).expect("accept");
            let mut reader = BufReader::new(&stream);
            let mut writer = BufWriter::new(&stream);
            let frame = read_frame(&mut reader).expect("read_frame");
            let req: serde_json::Value = serde_json::from_slice(&frame).unwrap();
            let reply = BridgeResponse::AskResult {
                id: req["id"].as_str().unwrap().to_string(),
                text,
                conversation_url: Some(CHAT.to_string()),
                warning: warning.map(str::to_string),
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

fn run_cgpt(dir: &Path, home: &Path, ep: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cgpt"))
        .args(["--socket", ep, "--no-spinner"])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run cgpt")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
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

#[test]
fn ask_names_the_project_after_its_folder() {
    let dir = work_dir("my-proj");
    let ep = endpoint("ask");
    let host = spawn_host(ep.clone(), vec![("an answer".into(), None)]);
    let out = run_cgpt(&dir, Path::new("/nonexistent-home"), &ep, &["ask", "hi"]);
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    let frames = host.join().expect("mock host panicked");
    assert_eq!(frames[0]["project"], "my-proj");
    // Still the user's active tab: no target.
    assert!(frames[0].get("target").is_none(), "{}", frames[0]);
    assert_eq!(text(&out.stdout), "an answer\n");
    assert!(
        !text(&out.stderr).contains("warning"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn ask_in_the_home_folder_names_no_project() {
    let dir = work_dir("home");
    let ep = endpoint("home");
    let host = spawn_host(ep.clone(), vec![("an answer".into(), None)]);
    let out = run_cgpt(&dir, &dir, &ep, &["ask", "hi"]);
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    let frames = host.join().expect("mock host panicked");
    assert!(frames[0].get("project").is_none(), "{}", frames[0]);
}

#[test]
fn ask_shows_the_warning_on_stderr_and_the_answer_alone_on_stdout() {
    let dir = work_dir("warned");
    let ep = endpoint("warn");
    let host = spawn_host(ep.clone(), vec![("an answer".into(), Some(WARNING))]);
    let out = run_cgpt(&dir, Path::new("/nonexistent-home"), &ep, &["ask", "hi"]);
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    host.join().expect("mock host panicked");
    assert_eq!(text(&out.stdout), "an answer\n");
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains(&format!("cgpt: warning: {WARNING}")),
        "{stderr}"
    );
}

#[test]
fn agent_names_the_project_on_every_turn_and_shows_the_warning() {
    let dir = work_dir("agent-proj");
    let ep = endpoint("agent");
    let host = spawn_host(ep.clone(), vec![(final_reply(), Some(WARNING))]);
    let out = run_cgpt(
        &dir,
        Path::new("/nonexistent-home"),
        &ep,
        &["agent", "do it"],
    );
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    let frames = host.join().expect("mock host panicked");
    assert!(!frames.is_empty());
    for f in &frames {
        assert_eq!(f["project"], "agent-proj", "{f}");
    }
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains(&format!("cgpt: warning: {WARNING}")),
        "{stderr}"
    );
    assert!(!text(&out.stdout).contains(WARNING));
}

#[test]
fn ask_in_a_subfolder_of_a_repository_names_the_repository() {
    let repo = work_dir("repo-root");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let sub = repo.join("src/inner");
    std::fs::create_dir_all(&sub).unwrap();
    let ep = endpoint("repo");
    let host = spawn_host(ep.clone(), vec![("an answer".into(), None)]);
    let out = run_cgpt(&sub, Path::new("/nonexistent-home"), &ep, &["ask", "hi"]);
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    let frames = host.join().expect("mock host panicked");
    assert_eq!(frames[0]["project"], "repo-root");
}

#[test]
fn a_folder_name_that_cannot_be_a_project_name_is_warned_about() {
    let long = "x".repeat(101);
    let dir = work_dir(&long);
    let ep = endpoint("long");
    let host = spawn_host(ep.clone(), vec![("an answer".into(), None)]);
    let out = run_cgpt(&dir, Path::new("/nonexistent-home"), &ep, &["ask", "hi"]);
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    let frames = host.join().expect("mock host panicked");
    assert!(frames[0].get("project").is_none(), "{}", frames[0]);
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("cgpt: warning: the folder name cannot be a ChatGPT project name"),
        "{stderr}"
    );
    assert!(
        stderr.contains("a new chat goes to a plain new chat"),
        "{stderr}"
    );
    assert_eq!(text(&out.stdout), "an answer\n");
}
