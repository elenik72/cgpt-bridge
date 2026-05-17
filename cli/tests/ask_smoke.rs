//! Integration test for the CLI's UDS client.
//!
//! Spawns a tiny mock server on a per-test temp socket, runs the same code
//! path the production CLI uses (`transport::ask_once`), and asserts the
//! response is parsed correctly. The real host's UDS server (M5 phase 2)
//! will speak the same wire protocol.

use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use cgpt_bridge_cli::transport::{ask_once, AskOutcome};
use cgpt_bridge_protocol::{read_frame, write_frame, AskRequest, BridgeResponse, ErrorCode};

/// Build a unique socket path. macOS caps `sun_path` at 104 bytes, and the
/// default `TMPDIR` under `/var/folders/...` is already ~49 chars before we
/// append a filename — close enough to the limit that long test labels push
/// us over. Use `/tmp` directly with a short name to stay well under SUN_LEN.
fn temp_socket(label: &str) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let short = &label[..label.len().min(4)];
    PathBuf::from(format!("/tmp/cgb-{}-{}-{}.sock", short, pid, ts))
}

/// Start a mock server that accepts a single connection, reads one frame,
/// asserts it parses as an Ask, and replies with a canned response. Returns
/// the joinable thread.
fn spawn_mock_ok(
    socket_path: PathBuf,
    reply_text: String,
) -> thread::JoinHandle<Result<(), String>> {
    thread::spawn(move || {
        let listener = UnixListener::bind(&socket_path)
            .map_err(|e| format!("bind {}: {}", socket_path.display(), e))?;
        let (stream, _addr) = listener.accept().map_err(|e| format!("accept: {}", e))?;
        let stream_clone = stream.try_clone().map_err(|e| format!("clone: {}", e))?;
        let mut reader = BufReader::new(stream);
        let mut writer = BufWriter::new(stream_clone);

        let req_bytes = read_frame(&mut reader).map_err(|e| format!("read_frame: {}", e))?;
        let req_value: serde_json::Value =
            serde_json::from_slice(&req_bytes).map_err(|e| format!("parse: {}", e))?;
        let req_id = req_value
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing id".to_string())?;
        let req_type = req_value
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing type".to_string())?;
        if req_type != "ask" {
            return Err(format!("expected ask, got {}", req_type));
        }

        let resp = BridgeResponse::AskResult {
            id: req_id.to_string(),
            text: reply_text,
        };
        let body = serde_json::to_vec(&resp).map_err(|e| format!("serialize: {}", e))?;
        write_frame(&mut writer, &body).map_err(|e| format!("write_frame: {}", e))?;

        // Cleanup the socket file so reruns do not collide.
        let _ = std::fs::remove_file(&socket_path);
        Ok(())
    })
}

fn spawn_mock_error(
    socket_path: PathBuf,
    code: ErrorCode,
    message: String,
) -> thread::JoinHandle<Result<(), String>> {
    thread::spawn(move || {
        let listener = UnixListener::bind(&socket_path).map_err(|e| format!("bind: {}", e))?;
        let (stream, _) = listener.accept().map_err(|e| format!("accept: {}", e))?;
        let stream_clone = stream.try_clone().map_err(|e| format!("clone: {}", e))?;
        let mut reader = BufReader::new(stream);
        let mut writer = BufWriter::new(stream_clone);
        let req_bytes = read_frame(&mut reader).map_err(|e| format!("read: {}", e))?;
        let req_value: serde_json::Value =
            serde_json::from_slice(&req_bytes).map_err(|e| format!("parse: {}", e))?;
        let req_id = req_value
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let resp = BridgeResponse::Error {
            id: req_id,
            code,
            message,
        };
        let body = serde_json::to_vec(&resp).map_err(|e| format!("ser: {}", e))?;
        write_frame(&mut writer, &body).map_err(|e| format!("write: {}", e))?;
        let _ = std::fs::remove_file(&socket_path);
        Ok(())
    })
}

#[test]
fn round_trip_ask_returns_assistant_text() {
    let socket = temp_socket("ok");
    let server = spawn_mock_ok(socket.clone(), "Pong.".to_string());

    // Give the listener a moment to bind before the client connects.
    thread::sleep(Duration::from_millis(50));

    let req = AskRequest {
        id: "test-1".into(),
        text: "say pong".into(),
        timeout_ms: 5000,
    };
    let outcome = ask_once(&socket, &req, 5000);
    server
        .join()
        .expect("server thread panicked")
        .expect("server err");

    match outcome {
        AskOutcome::Ok(BridgeResponse::AskResult { id, text }) => {
            assert_eq!(id, "test-1");
            assert_eq!(text, "Pong.");
        }
        other => panic!("unexpected outcome: {:?}", outcome_debug(&other)),
    }
}

#[test]
fn server_error_is_surfaced() {
    let socket = temp_socket("err");
    let server = spawn_mock_error(
        socket.clone(),
        ErrorCode::TabUnavailable,
        "no chatgpt tab".to_string(),
    );
    thread::sleep(Duration::from_millis(50));

    let req = AskRequest {
        id: "test-2".into(),
        text: "hi".into(),
        timeout_ms: 5000,
    };
    let outcome = ask_once(&socket, &req, 5000);
    server.join().expect("server panic").expect("server err");

    match outcome {
        AskOutcome::Ok(BridgeResponse::Error { code, message, id }) => {
            assert_eq!(code, ErrorCode::TabUnavailable);
            assert_eq!(message, "no chatgpt tab");
            assert_eq!(id.as_deref(), Some("test-2"));
        }
        other => panic!("unexpected outcome: {:?}", outcome_debug(&other)),
    }
}

#[test]
fn missing_socket_is_reported_as_socket_missing() {
    let socket = temp_socket("missing");
    // Don't spawn a server; the path must not exist.
    let req = AskRequest {
        id: "test-3".into(),
        text: "hi".into(),
        timeout_ms: 200,
    };
    let outcome = ask_once(&socket, &req, 200);
    assert!(
        matches!(outcome, AskOutcome::SocketMissing),
        "expected SocketMissing, got {}",
        outcome_debug(&outcome)
    );
}

fn outcome_debug(o: &AskOutcome) -> String {
    match o {
        AskOutcome::Ok(r) => format!("Ok({:?})", r),
        AskOutcome::SocketMissing => "SocketMissing".into(),
        AskOutcome::SocketIo(e) => format!("SocketIo({})", e),
        AskOutcome::Timeout => "Timeout".into(),
        AskOutcome::BadResponse(s) => format!("BadResponse({})", s),
    }
}
