//! `ask_once` puts the typed `BridgeRequest::Ask` on the wire and waits past
//! the extension's own deadline, so the host's reply reaches the caller.
//! A mock host on a temp socket stands in for the real one.

use std::io::{BufReader, BufWriter, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use cgpt_bridge_cli::transport::{ask_once, AskOutcome};
use cgpt_bridge_protocol::{
    accept, listen, read_frame, release, write_frame, AskRequest, BridgeResponse, ChatTarget,
    ErrorCode, HOST_TIMEOUT_GRACE_MS,
};

fn endpoint(label: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    #[cfg(unix)]
    {
        format!("/tmp/cgb-aw-{}-{}-{}.sock", label, pid, ts)
    }
    #[cfg(windows)]
    {
        format!("cgb-aw-{}-{}-{}", label, pid, ts)
    }
}

/// One-shot mock host: binds before it returns, hands the raw request frame
/// to the test, waits `delay`, then writes `reply` (given the request id).
fn spawn_host(
    ep: String,
    delay: Duration,
    reply: impl FnOnce(String) -> BridgeResponse + Send + 'static,
) -> (thread::JoinHandle<()>, mpsc::Receiver<Vec<u8>>) {
    let listener = listen(&ep).expect("listen");
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let stream = accept(&listener).expect("accept");
        let mut reader = BufReader::new(&stream);
        let mut writer = BufWriter::new(&stream);
        let frame = read_frame(&mut reader).expect("read_frame");
        let id = serde_json::from_slice::<serde_json::Value>(&frame).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        tx.send(frame).unwrap();
        thread::sleep(delay);
        let body = serde_json::to_vec(&reply(id)).unwrap();
        // The client may already have given up; that is the test's verdict.
        let _ = write_frame(&mut writer, &body).and_then(|_| writer.flush());
        release(&ep);
    });
    (handle, rx)
}

fn ok_reply(id: String) -> BridgeResponse {
    BridgeResponse::AskResult {
        id,
        text: "late answer".into(),
        conversation_url: None,
        warning: None,
        diagnostics: None,
    }
}

fn fixture(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/../protocol/tests/fixtures/{}.json",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    serde_json::from_str(&std::fs::read_to_string(&path).expect(&path)).unwrap()
}

fn assert_frame_matches_fixture(label: &str, target: Option<ChatTarget>, fixture_name: &str) {
    let ep = endpoint(label);
    let (host, rx) = spawn_host(ep.clone(), Duration::ZERO, ok_reply);
    let req = AskRequest {
        id: "a1".into(),
        text: "hello".into(),
        timeout_ms: 60_000,
        target,
        project: None,
        capture_stream: false,
        attachments: Vec::new(),
    };
    let outcome = ask_once(&ep, &req, 60_000);
    host.join().expect("host panicked");
    assert!(matches!(outcome, AskOutcome::Ok(_)));

    let frame: serde_json::Value = serde_json::from_slice(&rx.recv().unwrap()).unwrap();
    let mut expected = fixture(fixture_name);
    // The fixtures use fixed ids; the wire id is the request's own.
    expected["id"] = frame["id"].clone();
    assert_eq!(frame, expected, "{}", fixture_name);
}

#[test]
fn wire_frame_equals_ask_fixture() {
    assert_frame_matches_fixture("plain", None, "request_ask");
}

#[test]
fn wire_frame_keeps_conversation_target() {
    assert_frame_matches_fixture(
        "conv",
        Some(ChatTarget::Conversation {
            url: "https://chatgpt.com/c/abc-123".into(),
        }),
        "request_ask_conversation",
    );
}

#[test]
fn wire_frame_keeps_new_chat_target() {
    assert_frame_matches_fixture("newc", Some(ChatTarget::NewChat), "request_ask_new_chat");
}

#[test]
fn reply_just_after_timeout_ms_is_printed_not_timed_out() {
    let ep = endpoint("late");
    let timeout_ms = 300;
    let (host, _rx) = spawn_host(ep.clone(), Duration::from_millis(timeout_ms + 50), ok_reply);
    let req = AskRequest {
        id: "late-1".into(),
        text: "hi".into(),
        timeout_ms,
        target: None,
        project: None,
        capture_stream: false,
        attachments: Vec::new(),
    };
    let outcome = ask_once(&ep, &req, timeout_ms);
    host.join().expect("host panicked");
    match outcome {
        AskOutcome::Ok(BridgeResponse::AskResult { text, .. }) => {
            assert_eq!(text, "late answer")
        }
        AskOutcome::Timeout => panic!("client gave up at timeout_ms"),
        _ => panic!("unexpected outcome"),
    }
}

#[test]
fn hosts_own_timeout_error_arrives_before_client_gives_up() {
    // The host answers with its timeout error at timeout_ms + its grace; the
    // client must still be listening then.
    let ep = endpoint("grace");
    let timeout_ms = 100;
    let (host, _rx) = spawn_host(
        ep.clone(),
        Duration::from_millis(timeout_ms + HOST_TIMEOUT_GRACE_MS + 50),
        |id| BridgeResponse::Error {
            id: Some(id),
            code: ErrorCode::Timeout,
            message: "extension did not respond".into(),
        },
    );
    let req = AskRequest {
        id: "grace-1".into(),
        text: "hi".into(),
        timeout_ms,
        target: None,
        project: None,
        capture_stream: false,
        attachments: Vec::new(),
    };
    let outcome = ask_once(&ep, &req, timeout_ms);
    host.join().expect("host panicked");
    match outcome {
        AskOutcome::Ok(BridgeResponse::Error { code, .. }) => {
            assert_eq!(code, ErrorCode::Timeout)
        }
        AskOutcome::Timeout => panic!("client gave up before the host's own error"),
        _ => panic!("unexpected outcome"),
    }
}

#[test]
fn host_bad_request_reason_is_shown_by_the_cli() {
    // TASK-017: the host fails the waiter at once on a frame it cannot parse,
    // with a masked reason. The CLI must print that reason, not a timeout.
    let ep = endpoint("badreq");
    let reason =
        "unknown variant `bogus`, expected one of `pong` at line 1 column 8; \"…(12 chars)\"";
    let reason_owned = reason.to_string();
    let (host, _rx) = spawn_host(ep.clone(), Duration::ZERO, move |id| {
        BridgeResponse::Error {
            id: Some(id),
            code: ErrorCode::BadRequest,
            message: reason_owned,
        }
    });
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_cgpt"))
        .args(["--socket", ep.as_str(), "ask", "go"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("spawn cgpt");
    host.join().expect("host panicked");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("BadRequest"), "{stderr}");
    assert!(stderr.contains(reason), "{stderr}");
    assert!(!stderr.contains("no answer from the host"), "{stderr}");
}
