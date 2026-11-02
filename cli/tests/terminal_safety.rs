//! End-to-end checks that model-controlled text reaches the terminal only as
//! visible escapes. Each test runs the real `cgpt` binary with piped output
//! and inspects the bytes it wrote: no CR (0x0D) and no ESC (0x1B).

use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;

use cgpt_bridge_protocol::agent::AGENT_FENCE_INFO;
use cgpt_bridge_protocol::{
    accept, listen, read_frame, release, write_frame, BridgeResponse, ErrorCode,
};

/// Two-level escaped newlines (`\\n` in the text, so each repair pass peels
/// one level) followed by a real CR and a title-setting ESC sequence.
const HOSTILE: &str = "a\\\\nb\\\\nc\\\\nd\rEVIL\u{1b}]0;pwned\u{7}";

fn scratch_dir(label: &str) -> PathBuf {
    let p = PathBuf::from(format!("/tmp/cgb-ts-{}-{}", label, std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn assert_clean(what: &str, bytes: &[u8]) {
    assert!(
        !bytes.contains(&0x0d) && !bytes.contains(&0x1b),
        "{} contains CR/ESC: {:?}",
        what,
        String::from_utf8_lossy(bytes)
    );
}

fn contains(hay: &[u8], needle: &str) -> bool {
    String::from_utf8_lossy(hay).contains(needle)
}

fn run_cgpt(dir: &PathBuf, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cgpt"))
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .expect("spawn cgpt")
}

/// One-shot mock host: accepts a single connection and answers the ask.
fn spawn_host(endpoint: String, reply: BridgeResponse) -> thread::JoinHandle<()> {
    let listener = listen(&endpoint).expect("listen");
    thread::spawn(move || {
        let stream = accept(&listener).expect("accept");
        let mut reader = BufReader::new(&stream);
        let mut writer = BufWriter::new(&stream);
        let req = read_frame(&mut reader).expect("read_frame");
        let v: serde_json::Value = serde_json::from_slice(&req).unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        let reply = match reply {
            BridgeResponse::AskResult {
                text,
                conversation_url,
                ..
            } => BridgeResponse::AskResult {
                id,
                text,
                conversation_url,
                warning: None,
            },
            BridgeResponse::Error { code, message, .. } => BridgeResponse::Error {
                id: Some(id),
                code,
                message,
            },
            other => other,
        };
        write_frame(&mut writer, &serde_json::to_vec(&reply).unwrap()).unwrap();
        writer.flush().unwrap();
        release(&endpoint);
    })
}

fn endpoint(label: &str) -> String {
    format!("/tmp/cgb-ts-{}-{}.sock", label, std::process::id())
}

#[test]
fn replay_never_emits_cr_or_esc_even_after_double_escaping() {
    let dir = scratch_dir("replay");
    let run = dir.join(".cgpt-bridge/runs/s1");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(run.join("final.md"), HOSTILE).unwrap();
    for extra in [None, Some("--no-pretty")] {
        let mut args = vec!["replay", "s1"];
        args.extend(extra);
        let out = run_cgpt(&dir, &args);
        assert!(out.status.success(), "{:?}", out);
        assert_clean(&format!("replay {:?}", extra), &out.stdout);
        assert!(
            contains(&out.stdout, "d\\rEVIL\\x1b]0;pwned\\x07"),
            "{:?}",
            extra
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agent_final_user_message_never_emits_cr_or_esc() {
    let dir = scratch_dir("agent");
    let body = serde_json::json!({
        "version": 1,
        "status": "final",
        "user_message": HOSTILE,
        "plan_update": { "summary": "", "events": [] },
        "command": null,
    });
    let text = format!("```{}\n{}\n```\n", AGENT_FENCE_INFO, body);
    for extra in [None, Some("--no-pretty")] {
        // The mock serves one connection, so each run gets its own host on a
        // fresh socket. `spawn_host` binds before it returns, so the CLI can
        // connect right away: no sleep, no restart on a reused path.
        let ep = endpoint(&format!("agent-{}", extra.is_some()));
        let host = spawn_host(
            ep.clone(),
            BridgeResponse::AskResult {
                id: String::new(),
                text: text.clone(),
                conversation_url: None,
                warning: None,
            },
        );
        let mut args = vec!["--socket", ep.as_str(), "agent"];
        args.extend(extra);
        args.push("go");
        let out = run_cgpt(&dir, &args);
        host.join().expect("mock host panicked");
        assert_clean(&format!("agent stdout {:?}", extra), &out.stdout);
        assert!(
            contains(&out.stdout, "d\\rEVIL\\x1b]0;pwned\\x07"),
            "{:?}",
            out
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn host_error_text_is_escaped_for_agent_and_ask() {
    let dir = scratch_dir("hosterr");
    for (label, args) in [("agent", vec!["agent", "go"]), ("ask", vec!["ask", "go"])] {
        let ep = endpoint(&format!("he-{}", label));
        let host = spawn_host(
            ep.clone(),
            BridgeResponse::Error {
                id: None,
                code: ErrorCode::Internal,
                message: "boom\r\u{1b}[2Kfake".into(),
            },
        );
        let mut full = vec!["--socket", ep.as_str()];
        full.extend(args);
        let out = run_cgpt(&dir, &full);
        let _ = host.join();
        assert_clean(&format!("{} stderr", label), &out.stderr);
        assert!(contains(&out.stderr, "boom\\r\\x1b[2Kfake"), "{}", label);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ask_answer_never_emits_cr_or_esc() {
    let dir = scratch_dir("ask");
    let ep = endpoint("ask-text");
    let host = spawn_host(
        ep.clone(),
        BridgeResponse::AskResult {
            id: String::new(),
            text: HOSTILE.into(),
            conversation_url: None,
            warning: None,
        },
    );
    let out = run_cgpt(&dir, &["--socket", ep.as_str(), "ask", "go"]);
    host.join().expect("mock host panicked");
    assert!(out.status.success(), "{:?}", out);
    assert_clean("ask stdout", &out.stdout);
    assert!(
        contains(&out.stdout, "d\\rEVIL\\x1b]0;pwned\\x07"),
        "{:?}",
        out
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ask_plain_text_is_printed_unchanged() {
    let dir = scratch_dir("ask-plain");
    let ep = endpoint("ask-plain");
    let host = spawn_host(
        ep.clone(),
        BridgeResponse::AskResult {
            id: String::new(),
            text: "line one\n\tindented `code`\n".into(),
            conversation_url: None,
            warning: None,
        },
    );
    let out = run_cgpt(&dir, &["--socket", ep.as_str(), "ask", "go"]);
    host.join().expect("mock host panicked");
    assert_eq!(out.stdout, b"line one\n\tindented `code`\n");
    let _ = std::fs::remove_dir_all(&dir);
}
