//! Unix-domain-socket client for talking to the cgpt-bridge native host.
//!
//! M5 phase 1 establishes the wire shape and lets the CLI talk to *any*
//! UDS server that speaks the same protocol (including the integration test
//! mock). Phase 2 wires the real host's UDS listener.

use std::io::{self, BufReader, BufWriter, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cgpt_bridge_protocol::{
    default_socket_path as protocol_default_socket_path, read_frame, write_frame, AskRequest,
    BridgeResponse, ReadFrameError,
};

/// Outcome of a single ask round trip. Mapped to CLI exit codes by main.rs.
pub enum AskOutcome {
    Ok(BridgeResponse),
    /// The socket file does not exist. Usually means the host is not running.
    SocketMissing,
    /// Any other I/O error while connecting/reading/writing the socket.
    SocketIo(io::Error),
    /// Hit the per-request timeout while waiting for the response.
    Timeout,
    /// Got bytes back but they did not parse as a `BridgeResponse`.
    BadResponse(String),
}

/// Re-export the shared default socket path so callers don't need to depend
/// directly on the protocol crate just for this.
pub fn default_socket_path() -> PathBuf {
    protocol_default_socket_path()
}

pub fn resolve_socket_path(override_path: Option<&Path>) -> io::Result<PathBuf> {
    if let Some(p) = override_path {
        return Ok(p.to_path_buf());
    }
    Ok(default_socket_path())
}

/// Single ask round trip: connect, frame-write the request, frame-read the
/// response, disconnect.
pub fn ask_once(socket_path: &Path, request: &AskRequest, timeout_ms: u64) -> AskOutcome {
    let stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::NotFound => return AskOutcome::SocketMissing,
        Err(e) if e.kind() == ErrorKind::ConnectionRefused => return AskOutcome::SocketMissing,
        Err(e) => return AskOutcome::SocketIo(e),
    };

    // Apply the wall-clock timeout to both directions. The server is
    // responsible for streaming the response within this window; we are not
    // willing to wait longer.
    let dur = Some(Duration::from_millis(timeout_ms));
    if let Err(e) = stream.set_read_timeout(dur) {
        return AskOutcome::SocketIo(e);
    }
    if let Err(e) = stream.set_write_timeout(dur) {
        return AskOutcome::SocketIo(e);
    }

    // Build the wire envelope. We re-tag here so the same `AskRequest` struct
    // can be reused in both client and server without leaking the tag.
    let envelope = serde_json::json!({ "type": "ask", "id": request.id, "text": request.text, "timeout_ms": request.timeout_ms });
    let body = match serde_json::to_vec(&envelope) {
        Ok(b) => b,
        Err(e) => return AskOutcome::BadResponse(format!("serialize: {}", e)),
    };

    let stream_clone = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => return AskOutcome::SocketIo(e),
    };
    let mut writer = BufWriter::new(stream);
    let mut reader = BufReader::new(stream_clone);

    if let Err(e) = write_frame(&mut writer, &body) {
        return AskOutcome::SocketIo(e);
    }
    if let Err(e) = writer.flush() {
        return AskOutcome::SocketIo(e);
    }

    match read_frame(&mut reader) {
        Ok(bytes) => match serde_json::from_slice::<BridgeResponse>(&bytes) {
            Ok(resp) => AskOutcome::Ok(resp),
            Err(e) => AskOutcome::BadResponse(format!("parse response: {}", e)),
        },
        Err(ReadFrameError::Eof) => {
            AskOutcome::BadResponse("host closed connection without sending a response".into())
        }
        Err(ReadFrameError::OversizeFrame { len, cap }) => AskOutcome::BadResponse(format!(
            "host sent oversize frame: {} bytes (cap {})",
            len, cap
        )),
        Err(ReadFrameError::Io(e)) => {
            if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut {
                AskOutcome::Timeout
            } else {
                AskOutcome::SocketIo(e)
            }
        }
    }
}

/// Generate a request id. Combines wall-clock millis, a per-process counter,
/// and a prefix. Not crypto-strength; just unique enough to correlate within
/// a single CLI run.
pub fn new_request_id(prefix: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{}_{:x}_{:x}", prefix, ms, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Path-resolution tests live in `cgpt-bridge-protocol::socket_path` so
    // both CLI and host share a single source of truth (and a single set of
    // env-var races to coordinate).

    #[test]
    fn missing_socket_reports_socket_missing() {
        let req = AskRequest {
            id: "test-1".into(),
            text: "hi".into(),
            timeout_ms: 100,
        };
        let outcome = ask_once(Path::new("/tmp/cgpt-bridge.does-not-exist.sock"), &req, 100);
        match outcome {
            AskOutcome::SocketMissing => {}
            _ => panic!("expected SocketMissing"),
        }
    }

    #[test]
    fn request_ids_are_unique_per_call() {
        let a = new_request_id("ask");
        let b = new_request_id("ask");
        assert_ne!(a, b);
    }
}
