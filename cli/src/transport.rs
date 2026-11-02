//! Local-socket client for talking to the cgpt-bridge native host: a Unix
//! domain socket on macOS/Linux, a named pipe on Windows (see
//! `cgpt_bridge_protocol::local_socket`).
//!
//! The CLI talks to *any* server that speaks the same protocol (including the
//! integration test mock) and to the real host's listener.

use std::io::{self, BufReader, BufWriter, ErrorKind, Write};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use cgpt_bridge_protocol::{
    connect, default_endpoint, read_frame, round_trip, write_frame, AskRequest, BridgeRequest,
    BridgeResponse, ErrorCode, LocalStream, ReadFrameError, StopAskRequest,
    CLIENT_TIMEOUT_GRACE_MS, MAX_FRAME_BYTES,
};

use crate::interrupt::Armed;

// The generator lives in `protocol`; the gateway uses the same one.
pub use cgpt_bridge_protocol::new_request_id;

/// Outcome of a single ask round trip. Mapped to CLI exit codes by main.rs.
pub enum AskOutcome {
    Ok(BridgeResponse),
    /// The endpoint does not exist. Usually means the host is not running.
    SocketMissing,
    /// Any other I/O error while connecting/reading/writing the socket.
    SocketIo(io::Error),
    /// Hit the per-request timeout while waiting for the response.
    Timeout,
    /// The serialized request (`len` bytes) is over the frame cap (`cap`):
    /// refused before connecting, nothing was sent.
    TooLarge {
        len: usize,
        cap: usize,
    },
    /// Got bytes back but they did not parse as a `BridgeResponse`.
    BadResponse(String),
    /// Ctrl+C while waiting for the answer; what came of stopping it. The
    /// caller prints its line and ends with `interrupt::die`.
    Interrupted(StopOutcome),
}

/// What Ctrl+C during an ask got from the host (`stop_ask`).
#[derive(Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// The extension stopped the answer ChatGPT was writing for the ask.
    Stopped,
    /// The extension found no answer of the ask being written: it had
    /// ended, or the extension knows no ask by that id.
    NotRunning,
    /// The ask was cancelled before its prompt was sent (its tab was still
    /// being chosen, it was queued, or the page had not sent it yet): it
    /// ends `cancelled` and nothing of it reaches the chat.
    NotSent,
    /// The tab shows another chat than the ask's: nothing was pressed.
    OtherChat,
    /// The host refused `stop_ask` (`bad_request`): one built before it.
    OldHost,
    /// No answer to the stop in time, or an error: why. An extension built
    /// before `stop_ask` ignores it, so this is also what that looks like.
    Unconfirmed(String),
}

/// How long Ctrl+C waits for the host to answer the stop. The host gives the
/// extension longer; the user pressed Ctrl+C to be done.
pub const STOP_WAIT_MS: u64 = 3_000;

/// How often the wait for an answer looks for a Ctrl+C.
const PRESS_POLL: Duration = Duration::from_millis(50);

/// The endpoint to talk to: the `--socket` override if given (a socket path on
/// Unix, a pipe name on Windows), the platform default otherwise.
pub fn resolve_socket_path(override_path: Option<&Path>) -> io::Result<String> {
    if let Some(p) = override_path {
        return Ok(p.to_string_lossy().into_owned());
    }
    Ok(default_endpoint())
}

/// The ChatGPT project a new chat of this run goes into: the one named after
/// the project folder of `dir` (the repository root above it, else `dir`;
/// `cgpt_bridge_core::project::project_root`); none in the home folder or at
/// a disk root. A folder name that cannot be a project name gets a warning
/// on stderr, and the chat is a plain one. An ask that continues the chat
/// the tab shows ignores the project.
pub fn project_for(dir: &Path) -> Option<String> {
    use cgpt_bridge_core::project::{project_name, project_root, refused_warning};
    // The current directory comes resolved (macOS: `/private/tmp/…`), so the
    // home folder is compared resolved too; on Windows that adds `\\?\`,
    // which the comparison leaves out (`cgpt_bridge_core::project::dir_key`).
    let home = home_dir(|v| std::env::var_os(v)).map(|h| std::fs::canonicalize(&h).unwrap_or(h));
    let root = project_root(dir, home.as_deref());
    let named = match root.to_str() {
        Some(root) => project_name(root, home.as_deref().and_then(Path::to_str)),
        None => Err("the folder name is not UTF-8".to_string()),
    };
    named.unwrap_or_else(|reason| {
        show_warning(Some(&refused_warning(&reason)));
        None
    })
}

/// The user's home folder: `HOME`, else `USERPROFILE` (Windows sets only
/// that one). `env` reads a variable; an empty one counts as unset.
fn home_dir(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<std::path::PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .find_map(|v| env(v).filter(|h| !h.is_empty()))
        .map(std::path::PathBuf::from)
}

/// Show the bridge's warning about an answered ask (say, the new chat went
/// to a plain new chat instead of the project) on stderr.
pub fn show_warning(warning: Option<&str>) {
    if let Some(w) = warning {
        eprintln!("cgpt: warning: {}", crate::runner::escape_control(w));
    }
}

/// Single ask round trip: connect, frame-write the request, frame-read the
/// response, disconnect. Ctrl+C once the request is written stops the ask in
/// ChatGPT (`stop_ask`) and ends the wait with `Interrupted`.
pub fn ask_once(endpoint: &str, request: &AskRequest, timeout_ms: u64) -> AskOutcome {
    // The wire form is the typed request, so no field (e.g. `target`) is lost.
    let body = match serde_json::to_vec(&BridgeRequest::Ask(request.clone())) {
        Ok(b) => b,
        Err(e) => return AskOutcome::BadResponse(format!("serialize: {}", e)),
    };
    // The host takes no frame over the cap: say so before connecting.
    let cap = MAX_FRAME_BYTES as usize;
    if body.len() > cap {
        return AskOutcome::TooLarge {
            len: body.len(),
            cap,
        };
    }

    let stream = match connect(endpoint, timeout_ms) {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::NotFound => return AskOutcome::SocketMissing,
        Err(e) if e.kind() == ErrorKind::ConnectionRefused => return AskOutcome::SocketMissing,
        Err(e) => return AskOutcome::SocketIo(e),
    };

    // The wall-clock wait covers the whole exchange and outlasts the ask's own
    // `timeout_ms` by `CLIENT_TIMEOUT_GRACE_MS`, so the host's reply (a
    // partial answer or its timeout error) arrives before we give up.
    let wait = Duration::from_millis(timeout_ms.saturating_add(CLIENT_TIMEOUT_GRACE_MS));
    let reply = match wait_for_reply(stream, body, wait) {
        Waited::Reply(reply) => reply,
        Waited::Pressed(armed) => {
            // Still armed while the stop goes out: a second Ctrl+C ends it.
            let outcome = stop_ask(endpoint, &request.id);
            drop(armed);
            return AskOutcome::Interrupted(outcome);
        }
    };
    match reply {
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

enum Waited {
    Reply(Result<Vec<u8>, ReadFrameError>),
    Pressed(Armed),
}

/// `round_trip` (the exchange in a helper thread, as Windows pipes cannot
/// time out), with Ctrl+C armed once the request is written: before that it
/// ends `cgpt` as always, after that it ends the wait with `Pressed`.
fn wait_for_reply(stream: LocalStream, request: Vec<u8>, wait: Duration) -> Waited {
    enum Step {
        Sent,
        Done(Result<Vec<u8>, ReadFrameError>),
    }
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| {
            let mut writer = BufWriter::new(&stream);
            write_frame(&mut writer, &request)?;
            writer.flush()?;
            let _ = tx.send(Step::Sent);
            read_frame(&mut BufReader::new(&stream))
        })();
        let _ = tx.send(Step::Done(result));
    });
    let deadline = Instant::now() + wait;
    let mut armed: Option<Armed> = None;
    loop {
        if let Some(a) = armed.take() {
            if a.pressed() {
                // An answer that is already here wins over the press.
                return match rx.try_recv() {
                    Ok(Step::Done(result)) => Waited::Reply(result),
                    _ => Waited::Pressed(a),
                };
            }
            armed = Some(a);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Waited::Reply(Err(ReadFrameError::Io(io::Error::new(
                ErrorKind::TimedOut,
                "no response within the timeout",
            ))));
        }
        let tick = if armed.is_some() {
            left.min(PRESS_POLL)
        } else {
            left
        };
        match rx.recv_timeout(tick) {
            Ok(Step::Sent) => armed = Some(Armed::arm()),
            Ok(Step::Done(result)) => return Waited::Reply(result),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Waited::Reply(Err(ReadFrameError::Io(io::Error::other(
                    "exchange thread ended without a result",
                ))))
            }
        }
    }
}

/// Ctrl+C during the ask `ask_id`: ask the host, on a new connection, to
/// stop that ask in ChatGPT (`stop_ask`), waiting at most `STOP_WAIT_MS`.
/// A part built before `stop_ask` refuses (host) or ignores (extension) it;
/// neither stops any other answer.
pub fn stop_ask(endpoint: &str, ask_id: &str) -> StopOutcome {
    let deadline = Instant::now() + Duration::from_millis(STOP_WAIT_MS);
    let stop = BridgeRequest::StopAsk(StopAskRequest {
        id: new_request_id("stop"),
        ask_id: ask_id.to_string(),
    });
    match exchange(endpoint, &stop, deadline) {
        Ok(BridgeResponse::Stopped {
            other_chat: true, ..
        }) => StopOutcome::OtherChat,
        Ok(BridgeResponse::Stopped {
            was_generating: true,
            ..
        }) => StopOutcome::Stopped,
        Ok(BridgeResponse::Stopped { not_sent: true, .. }) => StopOutcome::NotSent,
        Ok(BridgeResponse::Stopped { .. }) => StopOutcome::NotRunning,
        Ok(BridgeResponse::Error {
            code: ErrorCode::BadRequest,
            ..
        }) => StopOutcome::OldHost,
        Ok(BridgeResponse::Error { code, message, .. }) => StopOutcome::Unconfirmed(format!(
            "host error ({:?}): {}",
            code,
            crate::runner::escape_control(&message)
        )),
        Ok(_) => StopOutcome::Unconfirmed("the host answered something else".into()),
        Err(why) => StopOutcome::Unconfirmed(why),
    }
}

/// One request and its response on a new connection, by `deadline`.
fn exchange(
    endpoint: &str,
    request: &BridgeRequest,
    deadline: Instant,
) -> Result<BridgeResponse, String> {
    let left = || deadline.saturating_duration_since(Instant::now());
    let stream = connect(endpoint, (left().as_millis() as u64).max(1))
        .map_err(|e| format!("cannot reach the host: {}", e))?;
    let body = serde_json::to_vec(request).map_err(|e| format!("serialize: {}", e))?;
    let bytes = round_trip(stream, &body, left()).map_err(|e| match e {
        ReadFrameError::Io(e)
            if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) =>
        {
            "no answer in time".to_string()
        }
        e => e.to_string(),
    })?;
    serde_json::from_slice(&bytes).map_err(|e| format!("bad answer: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_is_home_else_userprofile() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |v: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == v)
                    .map(|(_, val)| std::ffi::OsString::from(*val))
            }
        };
        assert_eq!(
            home_dir(env(&[("HOME", "/home/u"), ("USERPROFILE", r"C:\Users\u")])),
            Some("/home/u".into())
        );
        assert_eq!(
            home_dir(env(&[("USERPROFILE", r"C:\Users\u")])),
            Some(r"C:\Users\u".into())
        );
        assert_eq!(
            home_dir(env(&[("HOME", ""), ("USERPROFILE", r"C:\Users\u")])),
            Some(r"C:\Users\u".into())
        );
        assert_eq!(home_dir(env(&[])), None);
    }

    // Endpoint-resolution tests live in `cgpt-bridge-protocol::local_socket` so
    // both CLI and host share a single source of truth (and a single set of
    // env-var races to coordinate).

    #[test]
    fn missing_socket_reports_socket_missing() {
        let req = AskRequest {
            id: "test-1".into(),
            text: "hi".into(),
            timeout_ms: 100,
            target: None,
            project: None,
        };
        let outcome = ask_once("/tmp/cgpt-bridge.does-not-exist.sock", &req, 100);
        match outcome {
            AskOutcome::SocketMissing => {}
            _ => panic!("expected SocketMissing"),
        }
    }
}
