//! End-to-end test for the host's CLI ↔ extension router.
//!
//! Spawns the real host binary as a subprocess, gives it a fresh `TMPDIR`
//! (Unix) / `USERNAME` (Windows) so its local socket lands in a sandbox (and
//! does not collide with any running production host), then:
//!
//!   1. A "fake extension" thread reads the request the host forwards on
//!      stdout, asserts shape, and writes a canned response on stdin.
//!   2. The test thread connects to the host's local socket as if it were `cgpt ask`,
//!      sends an Ask request, waits for the response.
//!   3. Assert the response matches the canned text.

use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use cgpt_bridge_protocol::{
    connect, read_frame, round_trip, write_frame, AskRequest, BridgeResponse, ReadFrameError, BUILD,
};

/// Resolve the freshly built host binary.
fn host_bin() -> PathBuf {
    // Cargo sets CARGO_BIN_EXE_<name> for integration tests of bin crates.
    let p = env!("CARGO_BIN_EXE_cgpt-bridge-host");
    PathBuf::from(p)
}

/// Parent of the per-test sandbox dirs. `/tmp` keeps the Unix socket path
/// short enough for `sun_path` on macOS.
fn sandbox_root() -> PathBuf {
    if cfg!(unix) {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    }
}

/// The endpoint a host started by `spawn_host` listens on. Mimics
/// `protocol::default_endpoint()` for the sandboxed environment.
#[cfg(unix)]
fn expected_endpoint(tmp: &std::path::Path, _tag: &str) -> String {
    extern "C" {
        fn getuid() -> u32;
    }
    let uid = unsafe { getuid() };
    tmp.join(format!("cgpt-bridge.{}.sock", uid))
        .to_string_lossy()
        .into_owned()
}

/// On Windows the pipe name comes from `USERNAME`, so each test host gets its
/// own pipe through its own fake user name.
#[cfg(windows)]
fn expected_endpoint(_tmp: &std::path::Path, tag: &str) -> String {
    format!("cgpt-bridge-{}", fake_user(tag))
}

#[cfg(windows)]
fn fake_user(tag: &str) -> String {
    format!("cgbhost{}{}", std::process::id(), tag)
}

/// How long a test waits for a freshly spawned host to bind its socket. The
/// wait returns as soon as the endpoint accepts a connection, so a generous
/// limit costs nothing on a quiet machine; it only matters under heavy load
/// (load average ~70), where process start-up alone took more than 3 s.
const HOST_START_TIMEOUT: Duration = Duration::from_secs(20);

fn wait_for_endpoint(endpoint: &str, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        // Probe with a real connect: a Unix socket file appears at bind(),
        // before listen(), and a connect in between is refused.
        if connect(endpoint, 200).is_ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn cli_ask_round_trips_through_extension_mock() {
    let (mut child, endpoint, test_dir, _stderr) = spawn_host("ask");

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

    // Give the host a moment to bind its socket and start its accept loop.
    assert!(
        wait_for_endpoint(&endpoint, HOST_START_TIMEOUT),
        "host did not create socket at {} in time",
        endpoint
    );

    // The CLI half: connect, send Ask, wait for response.
    let ask = AskRequest {
        id: "e2e-ask-1".into(),
        text: "smoke".into(),
        timeout_ms: 3_000,
        target: None,
        project: None,
    };
    let envelope = serde_json::json!({
        "type": "ask",
        "id": ask.id,
        "text": ask.text,
        "timeout_ms": ask.timeout_ms,
    });

    let resp = local_round_trip(&endpoint, &envelope);

    let (req_type, fwd_id) = join_within(extension, "fake extension finishes").expect("ext err");
    assert_eq!(req_type, "ask");
    assert_eq!(fwd_id, "e2e-ask-1");

    match resp {
        BridgeResponse::AskResult { id, text, .. } => {
            assert_eq!(id, "e2e-ask-1");
            assert_eq!(text, "live response from fake extension");
        }
        other => panic!("unexpected response: {:?}", other),
    }

    // Tear down: the host does not exit on its own while its stdin is open.
    let _ = child.kill();
    let _ = child.wait();

    let _ = std::fs::remove_dir_all(&test_dir);
}

#[test]
fn cli_ask_times_out_when_extension_silent() {
    let (mut child, endpoint, test_dir, _stderr) = spawn_host("silent");

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

    assert!(wait_for_endpoint(&endpoint, HOST_START_TIMEOUT));

    let ask_id = "silent-1";
    let envelope = serde_json::json!({
        "type": "ask",
        "id": ask_id,
        "text": "anything",
        "timeout_ms": 150u64, // very short so the test runs quickly
    });

    let resp = local_round_trip(&endpoint, &envelope);

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

/// The host's stderr, written to a file in the test sandbox. Printed when the
/// test fails, so a CI failure shows what the host logged.
struct HostStderr(PathBuf);

impl HostStderr {
    fn contents(&self) -> String {
        std::fs::read_to_string(&self.0).unwrap_or_default()
    }
}

impl Drop for HostStderr {
    fn drop(&mut self) {
        if thread::panicking() {
            let log = std::fs::read_to_string(&self.0)
                .unwrap_or_else(|e| format!("<cannot read {}: {}>", self.0.display(), e));
            eprintln!("---- host stderr ----\n{}---- end host stderr ----", log);
        }
    }
}

/// Spawn the host in its own sandbox (TMPDIR on Unix, USERNAME on Windows);
/// returns the child, the endpoint it will listen on, the sandbox dir and the
/// guard that prints the host's stderr on failure.
/// Putting each host under a per-test name prevents parallel tests from
/// sharing a socket.
fn spawn_host(tag: &str) -> (std::process::Child, String, PathBuf, HostStderr) {
    let test_dir = sandbox_root().join(format!("cgb-host-test-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&test_dir);
    std::fs::create_dir_all(&test_dir).unwrap();
    let endpoint = expected_endpoint(&test_dir, tag);
    let mut cmd = Command::new(host_bin());
    // Make sure XDG_RUNTIME_DIR does not steer the path away from TMPDIR — we
    // want a predictable location for the assertion.
    cmd.env("TMPDIR", &test_dir).env_remove("XDG_RUNTIME_DIR");
    #[cfg(windows)]
    cmd.env("USERNAME", fake_user(tag));
    let stderr_path = test_dir.join("host-stderr.log");
    let stderr_file = std::fs::File::create(&stderr_path).expect("create host stderr log");
    let child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .expect("spawn host");
    (child, endpoint, test_dir, HostStderr(stderr_path))
}

/// One request/response round trip over the host's local socket, as a client
/// would.
fn local_round_trip(endpoint: &str, request: &serde_json::Value) -> BridgeResponse {
    let stream = connect(endpoint, 5_000).expect("local socket connect");
    let body = round_trip(
        stream,
        &serde_json::to_vec(request).unwrap(),
        Duration::from_secs(15),
    )
    .expect("round trip");
    serde_json::from_slice(&body).expect("parse response")
}

#[test]
fn host_forwards_stop_and_status() {
    let (mut child, endpoint, test_dir, _stderr) = spawn_host("stopstatus");
    let child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();

    // Extension mock: answers exactly three frames, in order — stop,
    // stop_ask, then status — and returns the frames it saw so the test can
    // assert them.
    let extension = thread::spawn(move || -> Result<Vec<serde_json::Value>, String> {
        let mut reader = BufReader::new(child_stdout);
        let mut writer = BufWriter::new(child_stdin);
        let mut seen = Vec::new();
        for _ in 0..3 {
            let body = read_frame(&mut reader).map_err(fmt_frame_err)?;
            let value: serde_json::Value =
                serde_json::from_slice(&body).map_err(|e| format!("parse: {}", e))?;
            let id = value["id"].as_str().ok_or("missing id")?.to_string();
            let resp = match value["type"].as_str() {
                Some("stop") | Some("stop_ask") => serde_json::json!({
                    "type": "stopped", "id": id, "was_generating": true,
                }),
                Some("status") => serde_json::json!({
                    "type": "status_result", "id": id, "extension": true,
                    "tab_open": true, "logged_in": true,
                    "conversation_url": "https://chatgpt.com/c/abc",
                    "build": "ext-build-1",
                }),
                other => return Err(format!("unexpected frame type {:?}", other)),
            };
            seen.push(value);
            write_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
                .map_err(|e| format!("write: {}", e))?;
            writer.flush().map_err(|e| format!("flush: {}", e))?;
        }
        Ok(seen)
    });

    assert!(wait_for_endpoint(&endpoint, HOST_START_TIMEOUT));

    let resp = local_round_trip(
        &endpoint,
        &serde_json::json!({"type": "stop", "id": "stop-1"}),
    );
    match resp {
        BridgeResponse::Stopped {
            id, was_generating, ..
        } => {
            assert_eq!(id, "stop-1");
            assert!(was_generating);
        }
        other => panic!("expected Stopped, got {:?}", other),
    }

    // `cgpt ask` on Ctrl+C: a stop of the one ask it names (C1).
    let resp = local_round_trip(
        &endpoint,
        &serde_json::json!({"type": "stop_ask", "id": "stop-2", "ask_id": "ask-1"}),
    );
    match resp {
        BridgeResponse::Stopped { id, .. } => assert_eq!(id, "stop-2"),
        other => panic!("expected Stopped, got {:?}", other),
    }

    let resp = local_round_trip(
        &endpoint,
        &serde_json::json!({"type": "status", "id": "status-1"}),
    );
    match resp {
        BridgeResponse::StatusResult {
            id,
            extension,
            tab_open,
            logged_in,
            conversation_url,
            build,
        } => {
            assert_eq!(id, "status-1");
            assert!(extension && tab_open && logged_in);
            assert_eq!(
                conversation_url.as_deref(),
                Some("https://chatgpt.com/c/abc")
            );
            // The extension's build reaches the caller as the extension sent it.
            assert_eq!(build.as_deref(), Some("ext-build-1"));
        }
        other => panic!("expected StatusResult, got {:?}", other),
    }

    let seen = join_within(extension, "fake extension finishes").expect("ext err");
    assert_eq!(seen[0], serde_json::json!({"type": "stop", "id": "stop-1"}));
    assert_eq!(
        seen[1],
        serde_json::json!({"type": "stop_ask", "id": "stop-2", "ask_id": "ask-1"})
    );
    assert_eq!(
        seen[2],
        serde_json::json!({"type": "status", "id": "status-1"})
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&test_dir);
}

#[test]
fn host_forwards_ask_target_to_extension() {
    let (mut child, endpoint, test_dir, _stderr) = spawn_host("target");
    let child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();

    let extension = thread::spawn(move || -> Result<serde_json::Value, String> {
        let mut reader = BufReader::new(child_stdout);
        let mut writer = BufWriter::new(child_stdin);
        let body = read_frame(&mut reader).map_err(fmt_frame_err)?;
        let value: serde_json::Value =
            serde_json::from_slice(&body).map_err(|e| format!("parse: {}", e))?;
        let resp = serde_json::json!({
            "type": "ask_result",
            "id": value["id"],
            "text": "ok",
            "conversation_url": "https://chatgpt.com/c/new1",
        });
        write_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .map_err(|e| format!("write: {}", e))?;
        writer.flush().map_err(|e| format!("flush: {}", e))?;
        Ok(value)
    });

    assert!(wait_for_endpoint(&endpoint, HOST_START_TIMEOUT));

    let resp = local_round_trip(
        &endpoint,
        &serde_json::json!({
            "type": "ask", "id": "tgt-1", "text": "hello", "timeout_ms": 3000,
            "target": {"kind": "new_chat"},
        }),
    );
    match resp {
        BridgeResponse::AskResult {
            id,
            text,
            conversation_url,
            ..
        } => {
            assert_eq!(id, "tgt-1");
            assert_eq!(text, "ok");
            assert_eq!(
                conversation_url.as_deref(),
                Some("https://chatgpt.com/c/new1")
            );
        }
        other => panic!("expected AskResult, got {:?}", other),
    }

    let forwarded = join_within(extension, "fake extension finishes").expect("ext err");
    assert_eq!(
        forwarded,
        serde_json::json!({
            "type": "ask", "id": "tgt-1", "text": "hello", "timeout_ms": 3000,
            "target": {"kind": "new_chat"},
        })
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&test_dir);
}

#[test]
fn host_forwards_the_project_and_the_answers_warning_and_refuses_a_bad_project() {
    let (mut child, endpoint, test_dir, _stderr) = spawn_host("project");
    let child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();

    let extension = thread::spawn(move || -> Result<serde_json::Value, String> {
        let mut reader = BufReader::new(child_stdout);
        let mut writer = BufWriter::new(child_stdin);
        // The first frame to arrive: the ask with a bad project never comes.
        let body = read_frame(&mut reader).map_err(fmt_frame_err)?;
        let value: serde_json::Value =
            serde_json::from_slice(&body).map_err(|e| format!("parse: {}", e))?;
        let resp = serde_json::json!({
            "type": "ask_result",
            "id": value["id"],
            "text": "ok",
            "conversation_url": "https://chatgpt.com/c/new1",
            "warning": "not in the project",
        });
        write_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
            .map_err(|e| format!("write: {}", e))?;
        writer.flush().map_err(|e| format!("flush: {}", e))?;
        Ok(value)
    });

    assert!(wait_for_endpoint(&endpoint, HOST_START_TIMEOUT));

    for bad in ["", " padded ", "a\u{1b}b"] {
        let resp = local_round_trip(
            &endpoint,
            &serde_json::json!({
                "type": "ask", "id": "bad-1", "text": "hello", "timeout_ms": 3000,
                "target": {"kind": "new_chat"}, "project": bad,
            }),
        );
        match resp {
            BridgeResponse::Error { id, code, message } => {
                assert_eq!(id.as_deref(), Some("bad-1"));
                assert_eq!(
                    code,
                    cgpt_bridge_protocol::ErrorCode::BadRequest,
                    "{message}"
                );
                assert!(message.contains("project"), "{message}");
            }
            other => panic!("expected Error/BadRequest for {bad:?}, got {:?}", other),
        }
    }

    let resp = local_round_trip(
        &endpoint,
        &serde_json::json!({
            "type": "ask", "id": "prj-1", "text": "hello", "timeout_ms": 3000,
            "target": {"kind": "new_chat"}, "project": "cgpt-bridge",
        }),
    );
    match resp {
        BridgeResponse::AskResult { id, warning, .. } => {
            assert_eq!(id, "prj-1");
            assert_eq!(warning.as_deref(), Some("not in the project"));
        }
        other => panic!("expected AskResult, got {:?}", other),
    }

    let forwarded = join_within(extension, "fake extension finishes").expect("ext err");
    assert_eq!(
        forwarded,
        serde_json::json!({
            "type": "ask", "id": "prj-1", "text": "hello", "timeout_ms": 3000,
            "target": {"kind": "new_chat"}, "project": "cgpt-bridge",
        })
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&test_dir);
}

#[test]
fn status_without_extension_reports_extension_false_and_stop_errors() {
    let (mut child, endpoint, test_dir, _stderr) = spawn_host("noext");
    // The extension side is gone: nothing reads the host's stdout any more,
    // so forwarding fails with a broken pipe.
    drop(child.stdout.take());
    let _stdin = child.stdin.take(); // keep the host alive

    assert!(wait_for_endpoint(&endpoint, HOST_START_TIMEOUT));

    let resp = local_round_trip(
        &endpoint,
        &serde_json::json!({"type": "status", "id": "status-2"}),
    );
    match resp {
        BridgeResponse::StatusResult {
            id,
            extension,
            tab_open,
            logged_in,
            conversation_url,
            build,
        } => {
            assert_eq!(id, "status-2");
            assert!(!extension && !tab_open && !logged_in);
            assert_eq!(conversation_url, None);
            // No extension, no extension build.
            assert_eq!(build, None);
        }
        other => panic!("expected StatusResult, got {:?}", other),
    }

    let resp = local_round_trip(
        &endpoint,
        &serde_json::json!({"type": "stop", "id": "stop-2"}),
    );
    match resp {
        BridgeResponse::Error { id, code, .. } => {
            assert_eq!(id.as_deref(), Some("stop-2"));
            assert_eq!(code, cgpt_bridge_protocol::ErrorCode::ExtensionUnavailable);
        }
        other => panic!("expected Error, got {:?}", other),
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&test_dir);
}

/// Join `handle`, failing with `stage` in the message instead of hanging
/// when it is still blocked after 20 s.
fn join_within<T>(handle: thread::JoinHandle<T>, stage: &str) -> T {
    let limit = Duration::from_secs(20);
    let started = Instant::now();
    while !handle.is_finished() {
        assert!(
            started.elapsed() < limit,
            "{}: still blocked after {:?}",
            stage,
            limit
        );
        thread::sleep(Duration::from_millis(10));
    }
    handle.join().expect(stage)
}

fn fmt_frame_err(e: ReadFrameError) -> String {
    format!("{:?}", e)
}

#[test]
fn pending_request_gets_error_when_extension_disconnects() {
    let (mut child, endpoint, test_dir, _stderr) = spawn_host("disc");
    let child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();

    // Extension mock: takes the forwarded stop and disconnects without an
    // answer, as an unloaded service worker does.
    let extension = thread::spawn(move || -> Result<(), String> {
        let mut reader = BufReader::new(child_stdout);
        read_frame(&mut reader).map_err(fmt_frame_err)?;
        drop(child_stdin);
        Ok(())
    });

    assert!(wait_for_endpoint(&endpoint, HOST_START_TIMEOUT));

    let resp = local_round_trip(
        &endpoint,
        &serde_json::json!({"type": "stop", "id": "stop-3"}),
    );
    match resp {
        BridgeResponse::Error { id, code, .. } => {
            assert_eq!(id.as_deref(), Some("stop-3"));
            assert_eq!(code, cgpt_bridge_protocol::ErrorCode::ExtensionUnavailable);
        }
        other => panic!("expected Error, got {:?}", other),
    }

    join_within(extension, "fake extension finishes").expect("ext err");
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&test_dir);
}

/// Spawn a host whose fake extension answers the first forwarded request with
/// `frames` (raw JSON, one frame each, built from the request id), then drain
/// stdout. Returns the host handles and the extension thread.
fn host_with_scripted_extension(
    tag: &str,
    frames: impl FnOnce(&str) -> Vec<serde_json::Value> + Send + 'static,
) -> (
    std::process::Child,
    String,
    PathBuf,
    HostStderr,
    thread::JoinHandle<Result<(), String>>,
) {
    let (mut child, endpoint, test_dir, stderr) = spawn_host(tag);
    let child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();
    let extension = thread::spawn(move || -> Result<(), String> {
        let mut reader = BufReader::new(child_stdout);
        let mut writer = BufWriter::new(child_stdin);
        let body = read_frame(&mut reader).map_err(fmt_frame_err)?;
        let value: serde_json::Value =
            serde_json::from_slice(&body).map_err(|e| format!("parse: {}", e))?;
        let id = value["id"].as_str().ok_or("missing id")?.to_string();
        for frame in frames(&id) {
            write_frame(&mut writer, &serde_json::to_vec(&frame).unwrap())
                .map_err(|e| format!("write: {}", e))?;
        }
        writer.flush().map_err(|e| format!("flush: {}", e))?;
        // Keep the pipe open so the host does not see a disconnect, which
        // would also wake the caller and hide the behaviour under test.
        while read_frame(&mut reader).is_ok() {}
        Ok(())
    });
    (child, endpoint, test_dir, stderr, extension)
}

fn finish_host(
    mut child: std::process::Child,
    test_dir: PathBuf,
    extension: thread::JoinHandle<Result<(), String>>,
) {
    let _ = child.kill();
    let _ = child.wait();
    let _ = join_within(extension, "fake extension finishes");
    let _ = std::fs::remove_dir_all(&test_dir);
}

/// The waiting caller must get `internal` with the serde reason at once, not
/// a timeout.
fn assert_frame_not_understood(resp: BridgeResponse, id: &str, elapsed: Duration) {
    match resp {
        BridgeResponse::Error {
            id: got,
            code,
            message,
        } => {
            assert_eq!(got.as_deref(), Some(id));
            assert_eq!(code, cgpt_bridge_protocol::ErrorCode::Internal, "{message}");
            assert!(
                message.starts_with("extension frame not understood: "),
                "{message}"
            );
            assert!(message.len() > "extension frame not understood: ".len());
        }
        other => panic!("expected Error/Internal, got {:?}", other),
    }
    assert!(
        elapsed < Duration::from_secs(1),
        "error took {:?}, the host waited for a timeout",
        elapsed
    );
}

#[test]
fn ping_is_answered_with_the_hosts_build_and_the_start_line_names_it() {
    let (mut child, endpoint, test_dir, stderr) = spawn_host("build");
    let _stdin = child.stdin.take(); // keep the host alive
    let _stdout = child.stdout.take();
    assert!(wait_for_endpoint(&endpoint, HOST_START_TIMEOUT));

    match local_round_trip(&endpoint, &serde_json::json!({"type": "ping", "id": "p-b"})) {
        BridgeResponse::Pong { id, build, .. } => {
            assert_eq!(id.as_deref(), Some("p-b"));
            assert_eq!(build.as_deref(), Some(BUILD));
        }
        other => panic!("expected Pong, got {:?}", other),
    }
    let log = stderr.contents();
    assert!(log.contains(&format!("(build {})", BUILD)), "{log}");

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&test_dir);
}

#[test]
fn cancelled_from_the_extension_reaches_the_caller_as_cancelled() {
    let (child, endpoint, test_dir, _stderr, extension) =
        host_with_scripted_extension("cancelled", |id| {
            vec![serde_json::json!({
                "type": "error", "id": id, "code": "cancelled",
                "message": "The answer was stopped in the ChatGPT page before it ended.",
            })]
        });
    assert!(wait_for_endpoint(&endpoint, HOST_START_TIMEOUT));

    let resp = local_round_trip(&endpoint, &ask_json("ask-c"));
    match resp {
        BridgeResponse::Error { id, code, .. } => {
            assert_eq!(id.as_deref(), Some("ask-c"));
            assert_eq!(code, cgpt_bridge_protocol::ErrorCode::Cancelled);
        }
        other => panic!("expected Error/Cancelled, got {:?}", other),
    }

    finish_host(child, test_dir, extension);
}

#[test]
fn tab_busy_from_the_extension_reaches_the_caller_as_tab_busy() {
    let (child, endpoint, test_dir, _stderr, extension) =
        host_with_scripted_extension("tab-busy", |id| {
            vec![serde_json::json!({
                "type": "error", "id": id, "code": "tab_busy",
                "message": "another ask is running in this tab",
            })]
        });
    assert!(wait_for_endpoint(&endpoint, HOST_START_TIMEOUT));

    match local_round_trip(&endpoint, &ask_json("ask-b")) {
        BridgeResponse::Error { id, code, message } => {
            assert_eq!(id.as_deref(), Some("ask-b"));
            assert_eq!(code, cgpt_bridge_protocol::ErrorCode::TabBusy);
            assert_eq!(message, "another ask is running in this tab");
        }
        other => panic!("expected Error/TabBusy, got {:?}", other),
    }

    finish_host(child, test_dir, extension);
}

#[test]
fn status_result_with_misnamed_field_fails_the_caller_at_once() {
    // `tabOpen` instead of `tab_open`: the frame is a known type that does
    // not parse. The caller used to wait out the 10 s control timeout.
    let (child, endpoint, test_dir, stderr, extension) =
        host_with_scripted_extension("badstatus", |id| {
            vec![serde_json::json!({
                "type": "status_result", "id": id,
                "extension": true, "tabOpen": true, "logged_in": true,
            })]
        });
    assert!(wait_for_endpoint(&endpoint, HOST_START_TIMEOUT));

    let started = Instant::now();
    let resp = local_round_trip(
        &endpoint,
        &serde_json::json!({"type": "status", "id": "status-bad"}),
    );
    assert_frame_not_understood(resp, "status-bad", started.elapsed());
    let log = stderr.contents();
    assert!(
        log.contains("extension frame not understood: ") && log.contains("status-bad"),
        "{log}"
    );

    finish_host(child, test_dir, extension);
}

#[test]
fn error_with_unknown_code_fails_the_ask_caller_at_once() {
    // The ask would otherwise wait timeout_ms + 5 s; the caller's own socket
    // read gives up at 15 s, so a regression fails here instead of hanging.
    let (child, endpoint, test_dir, _stderr, extension) =
        host_with_scripted_extension("badcode", |id| {
            vec![serde_json::json!({
                "type": "error", "id": id,
                "code": "brand_new_code", "message": "x",
            })]
        });
    assert!(wait_for_endpoint(&endpoint, HOST_START_TIMEOUT));

    let started = Instant::now();
    let resp = local_round_trip(
        &endpoint,
        &serde_json::json!({
            "type": "ask", "id": "ask-bad", "text": "hi", "timeout_ms": 30_000u64,
        }),
    );
    assert_frame_not_understood(resp, "ask-bad", started.elapsed());

    finish_host(child, test_dir, extension);
}

#[test]
fn frame_without_id_is_skipped_and_the_next_response_still_routes() {
    let (child, endpoint, test_dir, stderr, extension) =
        host_with_scripted_extension("noid", |id| {
            vec![
                serde_json::json!({
                    "type": "stopped", "was_generating": "SECRET NO-ID TEXT",
                }),
                serde_json::json!({
                    "type": "stopped", "id": id, "was_generating": false,
                }),
            ]
        });
    assert!(wait_for_endpoint(&endpoint, HOST_START_TIMEOUT));

    let resp = local_round_trip(
        &endpoint,
        &serde_json::json!({"type": "stop", "id": "stop-noid"}),
    );
    match resp {
        BridgeResponse::Stopped {
            id, was_generating, ..
        } => {
            assert_eq!(id, "stop-noid");
            assert!(!was_generating);
        }
        other => panic!("expected Stopped, got {:?}", other),
    }
    let log = stderr.contents();
    assert!(log.contains("ignoring unrecognized frame"), "{log}");
    assert!(!log.contains("extension frame not understood"), "{log}");
    assert!(!log.contains("SECRET"), "{log}");

    finish_host(child, test_dir, extension);
}

/// Message text of an `internal` error, with the invariants every
/// not-understood reply shares.
fn not_understood_message(resp: BridgeResponse, id: &str) -> String {
    match resp {
        BridgeResponse::Error {
            id: got,
            code,
            message,
        } => {
            assert_eq!(got.as_deref(), Some(id));
            assert_eq!(code, cgpt_bridge_protocol::ErrorCode::Internal, "{message}");
            assert!(
                message.starts_with("extension frame not understood: "),
                "{message}"
            );
            message
        }
        other => panic!("expected Error/Internal, got {:?}", other),
    }
}

#[test]
fn huge_unknown_type_gives_a_short_internal_error() {
    // serde echoes an unknown `type` in full; the reply must stay short.
    let (child, endpoint, test_dir, stderr, extension) =
        host_with_scripted_extension("hugetype", |id| {
            vec![serde_json::json!({"type": "A".repeat(200_000), "id": id})]
        });
    assert!(wait_for_endpoint(&endpoint, HOST_START_TIMEOUT));

    let resp = local_round_trip(
        &endpoint,
        &serde_json::json!({"type": "status", "id": "huge-1"}),
    );
    let message = not_understood_message(resp, "huge-1");
    assert!(message.len() < 300, "{} bytes", message.len());
    assert!(
        stderr.contents().len() < 1_000,
        "stderr grew with the frame"
    );

    finish_host(child, test_dir, extension);
}

#[test]
fn near_cap_frame_still_reaches_the_caller_as_internal() {
    // A frame just under the 1 MiB cap must not make the reply exceed the
    // cap, which would drop the caller's connection instead.
    let (child, endpoint, test_dir, _stderr, extension) =
        host_with_scripted_extension("nearcap", |id| {
            vec![serde_json::json!({"type": "A".repeat(1_040_000), "id": id})]
        });
    assert!(wait_for_endpoint(&endpoint, HOST_START_TIMEOUT));

    let resp = local_round_trip(
        &endpoint,
        &serde_json::json!({"type": "status", "id": "cap-1"}),
    );
    let message = not_understood_message(resp, "cap-1");
    assert!(message.len() < 300, "{} bytes", message.len());

    finish_host(child, test_dir, extension);
}

#[test]
fn frame_content_does_not_reach_the_caller_or_stderr() {
    // A wrong-typed field makes serde echo the offending value.
    let (child, endpoint, test_dir, stderr, extension) =
        host_with_scripted_extension("secret", |id| {
            vec![serde_json::json!({
                "type": "stopped", "id": id, "was_generating": "SECRET ANSWER TEXT",
            })]
        });
    assert!(wait_for_endpoint(&endpoint, HOST_START_TIMEOUT));

    let resp = local_round_trip(
        &endpoint,
        &serde_json::json!({"type": "stop", "id": "secret-1"}),
    );
    let message = not_understood_message(resp, "secret-1");
    assert!(!message.contains("SECRET"), "{message}");
    let log = stderr.contents();
    assert!(log.contains("extension frame not understood"), "{log}");
    assert!(!log.contains("SECRET"), "{log}");

    finish_host(child, test_dir, extension);
}

/// One frame written without the cap `write_frame` enforces, as an extension
/// that ignores the limit would.
fn write_raw_frame(w: &mut impl Write, body: &[u8]) {
    w.write_all(&(body.len() as u32).to_le_bytes()).unwrap();
    w.write_all(body).unwrap();
    w.flush().unwrap();
}

/// An ask_result of exactly `total` bytes whose text holds `marker` followed
/// by padding. `id_first` puts `id` right after `type`, as the extension's
/// frame builders do; otherwise it comes after the text, out of the host's
/// sight.
fn oversize_ask_result(id: &str, marker: &str, total: usize, id_first: bool) -> Vec<u8> {
    let (head, tail) = if id_first {
        (
            format!(r#"{{"type":"ask_result","id":"{id}","text":"{marker}"#),
            r#""}"#.to_string(),
        )
    } else {
        (
            format!(r#"{{"type":"ask_result","text":"{marker}"#),
            format!(r#"","id":"{id}"}}"#),
        )
    };
    let pad = total - head.len() - tail.len();
    let mut out = head.into_bytes();
    out.extend(std::iter::repeat_n(b'x', pad));
    out.extend_from_slice(tail.as_bytes());
    assert_eq!(out.len(), total);
    out
}

fn request_id(reader: &mut impl Read) -> Result<String, String> {
    let body = read_frame(reader).map_err(fmt_frame_err)?;
    let value: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| format!("parse: {}", e))?;
    Ok(value["id"].as_str().ok_or("missing id")?.to_string())
}

fn ask_json(id: &str) -> serde_json::Value {
    serde_json::json!({"type": "ask", "id": id, "text": "hi", "timeout_ms": 30_000u64})
}

#[test]
fn oversize_extension_frame_fails_only_its_caller_and_the_host_keeps_serving() {
    let (mut child, endpoint, test_dir, stderr) = spawn_host("oversize");
    let child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();

    // The extension sees two asks at once, answers one with a 1.2 MB frame
    // and the other normally, then serves a third ask.
    let extension = thread::spawn(move || -> Result<(), String> {
        let mut reader = BufReader::new(child_stdout);
        let mut writer = BufWriter::new(child_stdin);
        let first = request_id(&mut reader)?;
        let second = request_id(&mut reader)?;
        for id in [&first, &second] {
            if id == "big-ask" {
                let frame = oversize_ask_result(id, "SECRET OVERSIZE ANSWER", 1_200_089, true);
                write_raw_frame(&mut writer, &frame);
            } else {
                let ok = serde_json::json!({"type": "ask_result", "id": id, "text": "fine"});
                write_frame(&mut writer, &serde_json::to_vec(&ok).unwrap()).unwrap();
            }
        }
        let third = request_id(&mut reader)?;
        let ok = serde_json::json!({"type": "ask_result", "id": third, "text": "still alive"});
        write_frame(&mut writer, &serde_json::to_vec(&ok).unwrap()).unwrap();
        writer.flush().map_err(|e| format!("flush: {}", e))?;
        while read_frame(&mut reader).is_ok() {}
        Ok(())
    });
    assert!(wait_for_endpoint(&endpoint, HOST_START_TIMEOUT));

    let big_endpoint = endpoint.clone();
    let started = Instant::now();
    let big = thread::spawn(move || local_round_trip(&big_endpoint, &ask_json("big-ask")));
    let ok = local_round_trip(&endpoint, &ask_json("ok-ask"));
    let big = join_within(big, "oversize caller");
    let elapsed = started.elapsed();

    match big {
        BridgeResponse::Error { id, code, message } => {
            assert_eq!(id.as_deref(), Some("big-ask"));
            assert_eq!(
                code,
                cgpt_bridge_protocol::ErrorCode::OversizeFrame,
                "{message}"
            );
            assert_eq!(message, "ответ 1200089 байт больше лимита моста (1048576)");
            assert!(!message.contains("SECRET"), "{message}");
        }
        other => panic!("expected Error/OversizeFrame, got {:?}", other),
    }
    assert!(elapsed < Duration::from_secs(3), "took {:?}", elapsed);
    match ok {
        BridgeResponse::AskResult { text, .. } => assert_eq!(text, "fine"),
        other => panic!("expected AskResult, got {:?}", other),
    }

    // The host did not exit, and the next request goes through.
    assert!(child.try_wait().unwrap().is_none(), "host exited");
    match local_round_trip(&endpoint, &ask_json("after-ask")) {
        BridgeResponse::AskResult { text, .. } => assert_eq!(text, "still alive"),
        other => panic!("expected AskResult, got {:?}", other),
    }

    let log = stderr.contents();
    assert!(log.contains("oversize frame from extension"), "{log}");
    assert!(!log.contains("exiting"), "{log}");
    assert!(!log.contains("SECRET"), "{log}");
    assert!(log.len() < 1_000, "stderr grew with the frame");

    finish_host(child, test_dir, extension);
}

#[test]
fn oversize_frame_with_no_visible_id_is_skipped_and_the_next_response_routes() {
    // The id sits past the part of the frame the host looks at, so no caller
    // can be failed; the frame is dropped whole and the stream stays in sync.
    let (mut child, endpoint, test_dir, stderr) = spawn_host("oversize-late");
    let child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();
    let extension = thread::spawn(move || -> Result<(), String> {
        let mut reader = BufReader::new(child_stdout);
        let mut writer = BufWriter::new(child_stdin);
        let id = request_id(&mut reader)?;
        let frame = oversize_ask_result(&id, "SECRET LATE-ID ANSWER", 1_300_000, false);
        write_raw_frame(&mut writer, &frame);
        let ok = serde_json::json!({"type": "ask_result", "id": id, "text": "after the big one"});
        write_frame(&mut writer, &serde_json::to_vec(&ok).unwrap()).unwrap();
        writer.flush().map_err(|e| format!("flush: {}", e))?;
        while read_frame(&mut reader).is_ok() {}
        Ok(())
    });
    assert!(wait_for_endpoint(&endpoint, HOST_START_TIMEOUT));

    match local_round_trip(&endpoint, &ask_json("late-ask")) {
        BridgeResponse::AskResult { text, .. } => assert_eq!(text, "after the big one"),
        other => panic!("expected AskResult, got {:?}", other),
    }
    assert!(child.try_wait().unwrap().is_none(), "host exited");
    let log = stderr.contents();
    assert!(log.contains("oversize frame from extension"), "{log}");
    assert!(!log.contains("exiting"), "{log}");
    assert!(!log.contains("SECRET"), "{log}");

    finish_host(child, test_dir, extension);
}

#[test]
fn a_length_prefix_over_64_mib_ends_the_host_as_a_desync() {
    // Chrome never sends this much; a prefix this large means the stream is
    // out of step, so the host exits (and is restarted) instead of draining.
    let (mut child, _endpoint, test_dir, stderr) = spawn_host("desync");
    let mut child_stdin = child.stdin.take().unwrap();
    child_stdin
        .write_all(&(64u32 * 1024 * 1024 + 1).to_le_bytes())
        .unwrap();
    child_stdin.flush().unwrap();

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "host kept running"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(1));
    assert!(
        stderr.contents().contains("desynced length prefix"),
        "{}",
        stderr.contents()
    );
    let _ = std::fs::remove_dir_all(&test_dir);
}
