//! End-to-end test for the host's CLI ↔ extension router.
//!
//! Spawns the real host binary as a subprocess, gives it a fresh `TMPDIR`
//! so its UDS lands in a sandbox (and does not collide with any running
//! production host), then:
//!
//!   1. A "fake extension" thread reads the request the host forwards on
//!      stdout, asserts shape, and writes a canned response on stdin.
//!   2. The test thread connects to the host's UDS as if it were `cgpt ask`,
//!      sends an Ask request, waits for the response.
//!   3. Assert the response matches the canned text.

use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use cgpt_bridge_protocol::{read_frame, write_frame, AskRequest, BridgeResponse, ReadFrameError};

/// Resolve the freshly built host binary.
fn host_bin() -> PathBuf {
    // Cargo sets CARGO_BIN_EXE_<name> for integration tests of bin crates.
    let p = env!("CARGO_BIN_EXE_cgpt-bridge-host");
    PathBuf::from(p)
}

/// Mimic protocol::default_socket_path() using a custom tmpdir.
fn expected_socket_in(tmp: &Path) -> PathBuf {
    extern "C" {
        fn getuid() -> u32;
    }
    let uid = unsafe { getuid() };
    tmp.join(format!("cgpt-bridge.{}.sock", uid))
}

fn wait_for_socket(path: &Path, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if path.exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn cli_ask_round_trips_through_extension_mock() {
    // /tmp keeps the path short enough for sun_path on macOS, and putting
    // the host under a per-test subdir prevents two parallel test runs from
    // sharing a socket.
    let test_dir = PathBuf::from(format!("/tmp/cgb-host-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&test_dir);
    std::fs::create_dir_all(&test_dir).unwrap();
    let socket_path = expected_socket_in(&test_dir);

    let mut child = Command::new(host_bin())
        .env("TMPDIR", &test_dir)
        // Make sure XDG_RUNTIME_DIR does not steer the path away from
        // TMPDIR — we want a predictable location for the assertion.
        .env_remove("XDG_RUNTIME_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn host");

    let child_stdin = child.stdin.take().expect("child stdin");
    let child_stdout = child.stdout.take().expect("child stdout");

    // The extension half: read forwarded ask, write canned response.
    let extension = thread::spawn(move || -> Result<(String, String), String> {
        let mut reader = BufReader::new(child_stdout);
        let mut writer = BufWriter::new(child_stdin);

        let body = read_frame(&mut reader).map_err(fmt_frame_err)?;
        let value: serde_json::Value =
            serde_json::from_slice(&body).map_err(|e| format!("parse: {}", e))?;
        let req_type = value
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing type".to_string())?
            .to_string();
        let req_id = value
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing id".to_string())?
            .to_string();

        let resp = serde_json::json!({
            "type": "ask_result",
            "id": req_id,
            "text": "live response from fake extension",
        });
        let resp_bytes = serde_json::to_vec(&resp).unwrap();
        write_frame(&mut writer, &resp_bytes).map_err(|e| format!("write: {}", e))?;
        writer.flush().map_err(|e| format!("flush: {}", e))?;
        Ok((req_type, req_id))
    });

    // Give the host a moment to bind its UDS and start its accept loop.
    assert!(
        wait_for_socket(&socket_path, Duration::from_secs(3)),
        "host did not create socket at {} in time",
        socket_path.display()
    );

    // The CLI half: connect, send Ask, wait for response.
    let ask = AskRequest {
        id: "e2e-ask-1".into(),
        text: "smoke".into(),
        timeout_ms: 3_000,
    };
    let envelope = serde_json::json!({
        "type": "ask",
        "id": ask.id,
        "text": ask.text,
        "timeout_ms": ask.timeout_ms,
    });
    let envelope_bytes = serde_json::to_vec(&envelope).unwrap();

    let stream = UnixStream::connect(&socket_path).expect("uds connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let stream_clone = stream.try_clone().unwrap();
    let mut writer = BufWriter::new(stream);
    let mut reader = BufReader::new(stream_clone);

    write_frame(&mut writer, &envelope_bytes).expect("write ask");
    writer.flush().unwrap();

    let resp_bytes = read_frame(&mut reader).expect("read response");
    let resp: BridgeResponse = serde_json::from_slice(&resp_bytes).expect("parse response");

    let (req_type, fwd_id) = extension.join().expect("ext thread").expect("ext err");
    assert_eq!(req_type, "ask");
    assert_eq!(fwd_id, "e2e-ask-1");

    match resp {
        BridgeResponse::AskResult { id, text } => {
            assert_eq!(id, "e2e-ask-1");
            assert_eq!(text, "live response from fake extension");
        }
        other => panic!("unexpected response: {:?}", other),
    }

    // Tear down: closing child stdin makes the host's stdio reader see EOF
    // and exit cleanly.
    drop(writer);
    drop(reader);
    // child.stdin/stdout already taken; closing the original stream above
    // released the UDS side. Host should exit once stdin EOFs — which only
    // happens when our `writer` (NO: that wrote to the host's stdin) drops.
    let _ = child.kill();
    let _ = child.wait();

    let _ = std::fs::remove_dir_all(&test_dir);
}

#[test]
fn cli_ask_times_out_when_extension_silent() {
    let test_dir = PathBuf::from(format!("/tmp/cgb-host-test-{}-silent", std::process::id()));
    let _ = std::fs::remove_dir_all(&test_dir);
    std::fs::create_dir_all(&test_dir).unwrap();
    let socket_path = expected_socket_in(&test_dir);

    let mut child = Command::new(host_bin())
        .env("TMPDIR", &test_dir)
        .env_remove("XDG_RUNTIME_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn host");

    let mut child_stdout = child.stdout.take().expect("stdout");
    // Drain the forwarded ask so the host does not block writing it.
    let drain = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match child_stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    assert!(wait_for_socket(&socket_path, Duration::from_secs(3)));

    let ask_id = "silent-1";
    let envelope = serde_json::json!({
        "type": "ask",
        "id": ask_id,
        "text": "anything",
        "timeout_ms": 150u64, // very short so the test runs quickly
    });
    let envelope_bytes = serde_json::to_vec(&envelope).unwrap();

    let stream = UnixStream::connect(&socket_path).expect("uds connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let stream_clone = stream.try_clone().unwrap();
    let mut writer = BufWriter::new(stream);
    let mut reader = BufReader::new(stream_clone);

    write_frame(&mut writer, &envelope_bytes).unwrap();
    writer.flush().unwrap();

    let resp_bytes = read_frame(&mut reader).expect("read response");
    let resp: BridgeResponse = serde_json::from_slice(&resp_bytes).expect("parse");

    match resp {
        BridgeResponse::Error { id, code, message } => {
            assert_eq!(id.as_deref(), Some(ask_id));
            assert!(matches!(code, cgpt_bridge_protocol::ErrorCode::Timeout));
            assert!(message.contains("did not respond"));
        }
        other => panic!("expected Error/Timeout, got {:?}", other),
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = drain.join();
    let _ = std::fs::remove_dir_all(&test_dir);
}

fn fmt_frame_err(e: ReadFrameError) -> String {
    format!("{:?}", e)
}
