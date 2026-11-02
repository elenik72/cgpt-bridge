//! AUD-015: the exit-code table of `cgpt ask` and `cgpt agent`. Both commands
//! run the real binary against a mock host on a temp socket and must end with
//! the code `docs/requirements.md` §9 lists for the failure.

mod common;

use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;

use cgpt_bridge_protocol::{
    accept, listen, read_frame, release, write_frame, BridgeResponse, ErrorCode,
};

fn scratch_dir(label: &str) -> PathBuf {
    let p = PathBuf::from(format!("/tmp/cgb-ec-{}-{}", label, std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn endpoint(label: &str) -> String {
    format!("/tmp/cgb-ec-{}-{}.sock", label, std::process::id())
}

/// What the mock host answers with.
enum Reply {
    Error(ErrorCode),
    Pong,
    Garbage,
}

/// One connection: read the request, answer once.
fn spawn_host(ep: String, reply: Reply) -> common::Host<()> {
    let listener = listen(&ep).expect("listen");
    let wake = ep.clone();
    let handle = thread::spawn(move || {
        let stream = accept(&listener).expect("accept");
        let mut reader = BufReader::new(&stream);
        let mut writer = BufWriter::new(&stream);
        let req = read_frame(&mut reader).expect("read_frame");
        let id = serde_json::from_slice::<serde_json::Value>(&req).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let body = match reply {
            Reply::Error(code) => serde_json::to_vec(&BridgeResponse::Error {
                id: Some(id),
                code,
                message: "boom".into(),
            })
            .unwrap(),
            Reply::Pong => serde_json::to_vec(&BridgeResponse::Pong {
                id: Some(id),
                host_version: "x".into(),
                build: None,
                echo: None,
                ts_unix_ms: 0,
            })
            .unwrap(),
            Reply::Garbage => b"not json".to_vec(),
        };
        write_frame(&mut writer, &body).unwrap();
        writer.flush().unwrap();
        release(&ep);
    });
    common::Host::new(wake, handle)
}

/// `cgpt ask hi` or `cgpt agent hi` (or with no words), stdin closed.
fn run_cmd(cmd: &str, ep: &str, dir: &PathBuf, words: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cgpt"))
        .args(["--socket", ep, cmd])
        .args(words)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .expect("spawn cgpt")
}

fn code_of(cmd: &str, label: &str, reply: Reply) -> i32 {
    let ep = endpoint(label);
    let dir = scratch_dir(label);
    let host = spawn_host(ep.clone(), reply);
    let out = run_cmd(cmd, &ep, &dir, &["hi"]);
    host.join().unwrap();
    out.status.code().expect("exit code")
}

/// The wire error → exit code table, one row each. Same for both commands.
const HOST_ERRORS: [(ErrorCode, i32); 10] = [
    (ErrorCode::BadRequest, 2),
    (ErrorCode::OversizeFrame, 4),
    (ErrorCode::ExtensionUnavailable, 3),
    (ErrorCode::TabUnavailable, 5),
    (ErrorCode::DomFailure, 6),
    (ErrorCode::Timeout, 5),
    (ErrorCode::RateLimited, 1),
    (ErrorCode::Cancelled, 10),
    (ErrorCode::TabBusy, 8),
    (ErrorCode::Internal, 11),
];

#[test]
fn ask_host_errors_follow_the_table() {
    for (i, (code, want)) in HOST_ERRORS.into_iter().enumerate() {
        let got = code_of("ask", &format!("a{}", i), Reply::Error(code));
        assert_eq!(got, want, "ask, host error {:?}", code);
    }
}

#[test]
fn agent_host_errors_follow_the_table() {
    for (i, (code, want)) in HOST_ERRORS.into_iter().enumerate() {
        let got = code_of("agent", &format!("g{}", i), Reply::Error(code));
        assert_eq!(got, want, "agent, host error {:?}", code);
    }
}

#[test]
fn non_ask_answer_and_garbage_are_protocol_errors_for_both() {
    for cmd in ["ask", "agent"] {
        assert_eq!(
            code_of(cmd, &format!("{}p", cmd), Reply::Pong),
            7,
            "{} pong",
            cmd
        );
        assert_eq!(
            code_of(cmd, &format!("{}x", cmd), Reply::Garbage),
            7,
            "{} garbage",
            cmd
        );
    }
}

#[test]
fn missing_socket_is_a_setup_error_for_both() {
    for cmd in ["ask", "agent"] {
        let dir = scratch_dir(&format!("{}m", cmd));
        let out = run_cmd(cmd, "/tmp/cgb-ec-nope.sock", &dir, &["hi"]);
        assert_eq!(out.status.code(), Some(3), "{}", cmd);
    }
}

#[test]
fn a_prompt_over_the_frame_cap_is_a_usage_error_before_connecting() {
    // Over 1 MiB once serialized: refused before the socket is tried (the
    // socket does not exist, which would be 3), with the size and the limit.
    let prompt = "a".repeat(cgpt_bridge_protocol::MAX_FRAME_BYTES as usize + 1);
    for cmd in ["ask", "agent"] {
        let dir = scratch_dir(&format!("{}big", cmd));
        let mut child = Command::new(env!("CARGO_BIN_EXE_cgpt"))
            .args(["--socket", "/tmp/cgb-ec-nope.sock", cmd])
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn cgpt");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(prompt.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{cmd}: {err}");
        assert!(err.contains("too large"), "{cmd}: {err}");
        assert!(
            err.contains(&format!(
                "limit is {} bytes",
                cgpt_bridge_protocol::MAX_FRAME_BYTES
            )),
            "{cmd}: {err}"
        );
        assert!(err.contains("nothing was sent"), "{cmd}: {err}");
        assert!(!err.contains("socket not found"), "{cmd}: {err}");
    }
}

#[test]
fn empty_input_is_a_usage_error_for_both() {
    for cmd in ["ask", "agent"] {
        let dir = scratch_dir(&format!("{}e", cmd));
        let out = run_cmd(cmd, "/tmp/cgb-ec-nope.sock", &dir, &[]);
        assert_eq!(out.status.code(), Some(2), "{}", cmd);
        // Nothing reaches stdout; the hint goes to stderr.
        assert!(out.stdout.is_empty(), "{}", cmd);
        assert!(!out.stderr.is_empty(), "{}", cmd);
    }
}

#[test]
fn unknown_option_is_a_usage_error() {
    let dir = scratch_dir("opt");
    let out = Command::new(env!("CARGO_BIN_EXE_cgpt"))
        .args(["ask", "--no-such-flag", "hi"])
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}
