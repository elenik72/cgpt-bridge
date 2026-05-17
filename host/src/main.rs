//! cgpt-bridge native messaging host.
//!
//! Two duplex channels meet in this process:
//!   • Chrome stdio:  Chrome ↔ extension service worker (via Native Messaging)
//!   • Unix socket:   CLI processes (`cgpt ask`, ...) on the local machine
//!
//! The host is a thin router. CLI requests are forwarded over stdout to the
//! extension; the extension performs the tab interaction and posts the
//! response back over stdin; the response is delivered to whichever CLI
//! connection is still waiting on that request id.
//!
//! Anything diagnostic goes to stderr — Chrome interprets every stdout byte
//! as a Native Messaging frame, so a stray println would corrupt the channel.

use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cgpt_bridge_protocol::{
    default_socket_path, dispatch_basic, error_response, read_frame, response_id, write_frame,
    BridgeRequest, BridgeResponse, ErrorCode, ReadFrameError,
};

/// How long a CLI request waits beyond its own `timeout_ms` before the host
/// gives up. The extension is supposed to honor `timeout_ms` itself; we add a
/// small grace window so we never beat the extension to the timeout error.
const TIMEOUT_GRACE_MS: u64 = 5_000;

/// Shared stdout writer protected by a mutex so the UDS accept threads and
/// the keepalive responder can both write frames to Chrome without
/// interleaving.
type SharedStdout = Arc<Mutex<BufWriter<io::Stdout>>>;

/// Map from request id → channel used to wake the UDS handler thread waiting
/// for that response.
#[derive(Default)]
struct PendingMap {
    inner: Mutex<HashMap<String, Sender<BridgeResponse>>>,
}

impl PendingMap {
    fn insert(&self, id: String, tx: Sender<BridgeResponse>) {
        self.inner.lock().unwrap().insert(id, tx);
    }

    fn remove(&self, id: &str) -> Option<Sender<BridgeResponse>> {
        self.inner.lock().unwrap().remove(id)
    }

    /// Wake every pending caller with an error. Used at shutdown so CLI
    /// clients see an immediate failure instead of hanging until their own
    /// timeout fires.
    fn drain_with_error(&self, code: ErrorCode, message: &str) {
        let mut guard = self.inner.lock().unwrap();
        for (id, tx) in guard.drain() {
            let resp = error_response(Some(id), code, message.to_string());
            let _ = tx.send(resp);
        }
    }
}

fn main() -> ExitCode {
    let socket_path = default_socket_path();

    let listener = match bind_or_takeover(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "cgpt-bridge-host: cannot bind UDS at {}: {}",
                socket_path.display(),
                e
            );
            return ExitCode::from(1);
        }
    };

    eprintln!(
        "cgpt-bridge-host v{} started (pid {}, socket {})",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        socket_path.display()
    );

    let pending: Arc<PendingMap> = Arc::new(PendingMap::default());
    let stdout: SharedStdout = Arc::new(Mutex::new(BufWriter::new(io::stdout())));

    // UDS accept loop runs in a worker thread. Each accepted connection
    // spawns its own handler thread; one CLI client per request.
    {
        let pending = Arc::clone(&pending);
        let stdout = Arc::clone(&stdout);
        thread::spawn(move || run_uds_accept(listener, pending, stdout));
    }

    // Stdio reader loop runs on the main thread. When Chrome closes the pipe
    // (extension SW unloaded), read_frame returns Eof and we exit cleanly.
    let exit = run_stdio_reader(Arc::clone(&pending), Arc::clone(&stdout));

    // Cleanup: unlink the socket so the next host instance can bind.
    if let Err(e) = std::fs::remove_file(&socket_path) {
        if e.kind() != io::ErrorKind::NotFound {
            eprintln!(
                "cgpt-bridge-host: failed to remove socket {}: {}",
                socket_path.display(),
                e
            );
        }
    }

    // Fail every still-pending CLI request with a clear shutdown error.
    pending.drain_with_error(
        ErrorCode::ExtensionUnavailable,
        "host shutting down (extension disconnected)",
    );

    eprintln!("cgpt-bridge-host: clean shutdown");
    exit
}

/// Try to bind the socket path. If a previous instance left a stale file,
/// remove it and retry once. If a *live* instance is already bound, refuse
/// to start (single-instance guarantee).
fn bind_or_takeover(path: &Path) -> io::Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            // Probe: if we can connect, another live host owns the socket.
            match UnixStream::connect(path) {
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "another cgpt-bridge-host appears to be running",
                )),
                Err(_) => {
                    // Stale socket file. Unlink and retry exactly once.
                    std::fs::remove_file(path)?;
                    UnixListener::bind(path)
                }
            }
        }
        Err(e) => Err(e),
    }
}

fn run_uds_accept(listener: UnixListener, pending: Arc<PendingMap>, stdout: SharedStdout) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cgpt-bridge-host: uds accept error: {}", e);
                continue;
            }
        };
        let pending = Arc::clone(&pending);
        let stdout = Arc::clone(&stdout);
        thread::spawn(move || {
            if let Err(e) = handle_cli_client(stream, pending, stdout) {
                eprintln!("cgpt-bridge-host: cli client error: {}", e);
            }
        });
    }
}

/// One CLI connection = one request/response round trip. After the response
/// is delivered we close the socket; the CLI is expected to reconnect for a
/// new request.
fn handle_cli_client(
    stream: UnixStream,
    pending: Arc<PendingMap>,
    stdout: SharedStdout,
) -> io::Result<()> {
    let reader_stream = stream.try_clone()?;
    let mut reader = BufReader::new(reader_stream);
    let mut writer = BufWriter::new(stream);

    let body = match read_frame(&mut reader) {
        Ok(b) => b,
        Err(ReadFrameError::Eof) => return Ok(()),
        Err(ReadFrameError::OversizeFrame { len, cap }) => {
            let resp = error_response(
                None,
                ErrorCode::OversizeFrame,
                format!("cli frame {} bytes exceeds cap {}", len, cap),
            );
            write_response(&mut writer, &resp)?;
            return Ok(());
        }
        Err(ReadFrameError::Io(e)) => return Err(e),
    };

    let req: BridgeRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            let resp = error_response(
                extract_id(&body),
                ErrorCode::BadRequest,
                format!("invalid request: {}", e),
            );
            write_response(&mut writer, &resp)?;
            return Ok(());
        }
    };

    match req {
        BridgeRequest::Ping(p) => {
            // CLI ping is mostly a diagnostic; we answer locally without
            // bothering the extension.
            let resp = dispatch_basic(BridgeRequest::Ping(p), now_unix_ms())
                .expect("dispatch_basic handles Ping");
            write_response(&mut writer, &resp)?;
        }
        BridgeRequest::Ask(ask) => {
            let (tx, rx) = mpsc::channel();
            pending.insert(ask.id.clone(), tx);

            // Forward to the extension. If the stdout write fails (Chrome
            // pipe closed), short-circuit to an error response.
            let envelope = serde_json::json!({
                "type": "ask",
                "id": ask.id,
                "text": ask.text,
                "timeout_ms": ask.timeout_ms,
            });
            let envelope_bytes = serde_json::to_vec(&envelope).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("serialize: {}", e))
            })?;

            let forward_result = {
                let mut so = stdout.lock().unwrap();
                write_frame(&mut *so, &envelope_bytes).and_then(|_| so.flush())
            };
            if let Err(e) = forward_result {
                pending.remove(&ask.id);
                let resp = error_response(
                    Some(ask.id),
                    ErrorCode::ExtensionUnavailable,
                    format!("cannot forward to extension: {}", e),
                );
                write_response(&mut writer, &resp)?;
                return Ok(());
            }

            let wait_for = Duration::from_millis(ask.timeout_ms + TIMEOUT_GRACE_MS);
            let resp = match rx.recv_timeout(wait_for) {
                Ok(r) => r,
                Err(RecvTimeoutError::Timeout) => {
                    pending.remove(&ask.id);
                    error_response(
                        Some(ask.id),
                        ErrorCode::Timeout,
                        format!(
                            "extension did not respond within {} ms (+ {} ms grace)",
                            ask.timeout_ms, TIMEOUT_GRACE_MS
                        ),
                    )
                }
                Err(RecvTimeoutError::Disconnected) => {
                    pending.remove(&ask.id);
                    error_response(
                        Some(ask.id),
                        ErrorCode::ExtensionUnavailable,
                        "host shut down before extension responded",
                    )
                }
            };

            write_response(&mut writer, &resp)?;
        }
    }

    Ok(())
}

/// Read messages coming back from the extension (over Chrome stdio stdin),
/// route responses to the matching pending CLI request, and answer
/// extension-originated requests (currently only ping for keepalive).
fn run_stdio_reader(pending: Arc<PendingMap>, stdout: SharedStdout) -> ExitCode {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());

    loop {
        let body = match read_frame(&mut reader) {
            Ok(b) => b,
            Err(ReadFrameError::Eof) => return ExitCode::SUCCESS,
            Err(ReadFrameError::OversizeFrame { len, cap }) => {
                eprintln!(
                    "cgpt-bridge-host: oversize frame from extension ({} > {}), exiting",
                    len, cap
                );
                return ExitCode::from(1);
            }
            Err(ReadFrameError::Io(e)) => {
                eprintln!("cgpt-bridge-host: stdin io error: {}", e);
                return ExitCode::from(1);
            }
        };

        // The extension can send either a request (ping for keepalive) or
        // a response (ask_result / error correlated with a pending ask).
        // We try the response shape first because it has a richer
        // discriminator; if that fails, fall back to request.
        if let Ok(resp) = serde_json::from_slice::<BridgeResponse>(&body) {
            handle_extension_response(&pending, resp);
            continue;
        }

        if let Ok(req) = serde_json::from_slice::<BridgeRequest>(&body) {
            handle_extension_request(&stdout, req);
            continue;
        }

        eprintln!(
            "cgpt-bridge-host: ignoring unrecognized frame from extension ({} bytes)",
            body.len()
        );
    }
}

fn handle_extension_response(pending: &Arc<PendingMap>, resp: BridgeResponse) {
    let id_owned = response_id(&resp).map(|s| s.to_string());
    let Some(id) = id_owned else {
        eprintln!("cgpt-bridge-host: extension response had no id, dropping");
        return;
    };
    match pending.remove(&id) {
        Some(tx) => {
            if let Err(_) = tx.send(resp) {
                eprintln!(
                    "cgpt-bridge-host: response for id {} arrived after caller gave up",
                    id
                );
            }
        }
        None => {
            eprintln!(
                "cgpt-bridge-host: response for unknown id {} (caller may have timed out)",
                id
            );
        }
    }
}

fn handle_extension_request(stdout: &SharedStdout, req: BridgeRequest) {
    let resp = match req {
        BridgeRequest::Ping(p) => dispatch_basic(BridgeRequest::Ping(p), now_unix_ms())
            .expect("dispatch_basic handles Ping"),
        BridgeRequest::Ask(a) => {
            // The extension is never supposed to initiate asks at the host.
            // Reply with a clear error rather than silently ignoring.
            error_response(
                Some(a.id),
                ErrorCode::BadRequest,
                "ask requests from extension are not supported",
            )
        }
    };

    let bytes = match serde_json::to_vec(&resp) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cgpt-bridge-host: serialize response: {}", e);
            return;
        }
    };
    let mut so = stdout.lock().unwrap();
    if let Err(e) = write_frame(&mut *so, &bytes).and_then(|_| so.flush()) {
        eprintln!("cgpt-bridge-host: stdout write: {}", e);
    }
}

fn write_response<W: Write>(writer: &mut W, resp: &BridgeResponse) -> io::Result<()> {
    let bytes = serde_json::to_vec(resp)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("serialize: {}", e)))?;
    write_frame(writer, &bytes)?;
    writer.flush()
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn extract_id(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("id")?.as_str().map(|s| s.to_string())
}

// The PathBuf import is kept for future test helpers; suppress unused warning
// while the tests in this module do not yet exercise it.
#[allow(dead_code)]
fn _phantom(_p: PathBuf) {}
