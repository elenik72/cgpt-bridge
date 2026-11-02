//! cgpt-bridge native messaging host.
//!
//! Two duplex channels meet in this process:
//!   • Chrome stdio:  Chrome ↔ extension service worker (via Native Messaging)
//!   • Local socket:  CLI processes (`cgpt ask`, ...) on the local machine
//!                    (Unix domain socket; named pipe on Windows)
//!
//! The host is a thin router. CLI requests are forwarded over stdout to the
//! extension; the extension performs the tab interaction and posts the
//! response back over stdin; the response is delivered to whichever CLI
//! connection is still waiting on that request id.
//!
//! Anything diagnostic goes to stderr — Chrome interprets every stdout byte
//! as a Native Messaging frame, so a stray println would corrupt the channel.

use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cgpt_bridge_protocol::{
    accept, check_project_name, default_endpoint, dispatch_basic, error_response, flush_to_peer,
    listen, read_frame, release, response_id, write_frame, AskRequest, BridgeRequest,
    BridgeResponse, ErrorCode, LocalListener, LocalStream, ReadFrameError, StatusRequest,
    StopAskRequest, StopRequest, BUILD, HOST_TIMEOUT_GRACE_MS,
};

/// How long `stop`, `stop_ask` and `status` wait for the extension. These are quick
/// page-state operations, unlike `ask`, so there is no caller-supplied timeout.
const CONTROL_TIMEOUT_MS: u64 = 10_000;

/// Pause after a failed `accept`, so a persistent error does not spin the
/// accept thread.
const ACCEPT_ERROR_BACKOFF_MS: u64 = 100;

/// How long a shutting-down host waits for CLI connections to get the replies
/// they already hold (an extension response that just arrived, or the
/// shutdown error) before the process exits.
const SHUTDOWN_GRACE_MS: u64 = 2_000;

/// Shared stdout writer protected by a mutex so the socket accept threads and
/// the keepalive responder can both write frames to Chrome without
/// interleaving.
type SharedStdout = Arc<Mutex<BufWriter<io::Stdout>>>;

/// Map from request id → channel used to wake the socket handler thread waiting
/// for that response.
#[derive(Default)]
struct PendingMap {
    inner: Mutex<PendingState>,
}

#[derive(Default)]
struct PendingState {
    waiting: HashMap<String, Sender<BridgeResponse>>,
    /// Set at shutdown: nothing will answer a request registered after that.
    closed: bool,
}

/// Why a caller could not be registered.
#[derive(Debug)]
enum Refused {
    /// The host is shutting down.
    Closed,
    /// Another caller already waits on this id; it keeps its slot.
    Duplicate,
}

impl PendingMap {
    /// Register a caller.
    fn insert(&self, id: String, tx: Sender<BridgeResponse>) -> Result<(), Refused> {
        let mut state = self.inner.lock().unwrap();
        if state.closed {
            return Err(Refused::Closed);
        }
        if state.waiting.contains_key(&id) {
            return Err(Refused::Duplicate);
        }
        state.waiting.insert(id, tx);
        Ok(())
    }

    fn remove(&self, id: &str) -> Option<Sender<BridgeResponse>> {
        self.inner.lock().unwrap().waiting.remove(id)
    }

    /// Wake every pending caller with an error and refuse new ones. Used at
    /// shutdown so CLI clients see an immediate failure instead of hanging
    /// until their own timeout fires.
    fn drain_with_error(&self, code: ErrorCode, message: &str) {
        let mut state = self.inner.lock().unwrap();
        state.closed = true;
        for (id, tx) in state.waiting.drain() {
            let resp = error_response(Some(id), code, message.to_string());
            let _ = tx.send(resp);
        }
    }
}

/// Number of CLI connections being served. Shutdown waits for it to reach
/// zero: exiting the process kills the handler threads, and a connection
/// whose handler dies before writing its reply is closed with no reply at
/// all (the client reads EOF instead of a response or an error).
#[derive(Default)]
struct InFlight {
    count: Mutex<usize>,
    idle: Condvar,
}

/// One served connection; dropping it marks the connection done.
struct InFlightGuard(Arc<InFlight>);

impl InFlight {
    fn enter(self: &Arc<Self>) -> InFlightGuard {
        *self.count.lock().unwrap() += 1;
        InFlightGuard(Arc::clone(self))
    }

    /// Wait until no connection is being served, at most `timeout`. Returns
    /// how many are still in flight.
    fn wait_idle(&self, timeout: Duration) -> usize {
        let count = self.count.lock().unwrap();
        let (count, _) = self
            .idle
            .wait_timeout_while(count, timeout, |n| *n > 0)
            .unwrap();
        *count
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut count = self.0.count.lock().unwrap();
        *count -= 1;
        if *count == 0 {
            self.0.idle.notify_all();
        }
    }
}

fn main() -> ExitCode {
    let endpoint = default_endpoint();

    let listener = match listen(&endpoint) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "cgpt-bridge-host: cannot listen on local socket {}: {}",
                endpoint, e
            );
            return ExitCode::from(1);
        }
    };

    eprintln!(
        "cgpt-bridge-host v{} (build {}) started (pid {}, socket {})",
        env!("CARGO_PKG_VERSION"),
        BUILD,
        std::process::id(),
        endpoint
    );

    let pending: Arc<PendingMap> = Arc::new(PendingMap::default());
    let in_flight: Arc<InFlight> = Arc::new(InFlight::default());
    let stdout: SharedStdout = Arc::new(Mutex::new(BufWriter::new(io::stdout())));

    // Accept loop runs in a worker thread. Each accepted connection spawns
    // its own handler thread; one CLI client per request.
    {
        let pending = Arc::clone(&pending);
        let in_flight = Arc::clone(&in_flight);
        let stdout = Arc::clone(&stdout);
        thread::spawn(move || run_accept(listener, pending, in_flight, stdout));
    }

    // Stdio reader loop runs on the main thread. When Chrome closes the pipe
    // (extension SW unloaded), read_frame returns Eof and we exit cleanly.
    let exit = run_stdio_reader(Arc::clone(&pending), Arc::clone(&stdout));

    // Cleanup: unlink the socket so the next host instance can bind.
    release(&endpoint);

    // Fail every still-pending CLI request with a clear shutdown error.
    pending.drain_with_error(
        ErrorCode::ExtensionUnavailable,
        "host shutting down (extension disconnected)",
    );

    // Let the handlers write what they now hold — responses routed just
    // before stdin closed, and the errors above — before the exit kills them.
    let left = in_flight.wait_idle(Duration::from_millis(SHUTDOWN_GRACE_MS));
    if left > 0 {
        eprintln!(
            "cgpt-bridge-host: {} cli connection(s) still open after {} ms, closing them",
            left, SHUTDOWN_GRACE_MS
        );
    }

    eprintln!("cgpt-bridge-host: clean shutdown");
    exit
}

fn run_accept(
    listener: LocalListener,
    pending: Arc<PendingMap>,
    in_flight: Arc<InFlight>,
    stdout: SharedStdout,
) {
    loop {
        let stream = match accept(&listener) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cgpt-bridge-host: accept error: {}", e);
                thread::sleep(Duration::from_millis(ACCEPT_ERROR_BACKOFF_MS));
                continue;
            }
        };
        let pending = Arc::clone(&pending);
        let stdout = Arc::clone(&stdout);
        let served = in_flight.enter();
        thread::spawn(move || {
            let _served = served;
            // Flushing to the peer keeps the connection counted as served
            // until the client has its reply (on Windows a reply still in
            // the pipe dies with the process).
            if let Err(e) =
                handle_cli_client(&stream, pending, stdout).and_then(|()| flush_to_peer(&stream))
            {
                eprintln!("cgpt-bridge-host: cli client error: {}", e);
            }
        });
    }
}

/// One CLI connection = one request/response round trip. After the response
/// is delivered we close the socket; the CLI is expected to reconnect for a
/// new request.
fn handle_cli_client(
    stream: &LocalStream,
    pending: Arc<PendingMap>,
    stdout: SharedStdout,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream);
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
        BridgeRequest::Ask(ref ask) => {
            let id = ask.id.clone();
            if let Some(Err(e)) = ask.project.as_deref().map(check_project_name) {
                let resp = error_response(
                    Some(id),
                    ErrorCode::BadRequest,
                    format!("invalid request: {}", e),
                );
                write_response(&mut writer, &resp)?;
                return Ok(());
            }
            let wait_for = ask_wait(ask.timeout_ms);
            let timeout_msg = format!(
                "extension did not respond within {} ms (+ {} ms grace)",
                ask.timeout_ms, HOST_TIMEOUT_GRACE_MS
            );
            let resp =
                match forward_to_extension(&req, &id, wait_for, timeout_msg, &pending, &stdout) {
                    Ok(r) => r,
                    Err(e) => error_response(Some(id), ErrorCode::ExtensionUnavailable, e),
                };
            write_response(&mut writer, &resp)?;
        }
        BridgeRequest::Stop(StopRequest { ref id })
        | BridgeRequest::StopAsk(StopAskRequest { ref id, .. }) => {
            let id = id.clone();
            let wait_for = Duration::from_millis(CONTROL_TIMEOUT_MS);
            let timeout_msg = format!("extension did not respond within {} ms", CONTROL_TIMEOUT_MS);
            let resp =
                match forward_to_extension(&req, &id, wait_for, timeout_msg, &pending, &stdout) {
                    Ok(r) => r,
                    Err(e) => error_response(Some(id), ErrorCode::ExtensionUnavailable, e),
                };
            write_response(&mut writer, &resp)?;
        }
        BridgeRequest::Status(ref status) => {
            let id = status.id.clone();
            let wait_for = Duration::from_millis(CONTROL_TIMEOUT_MS);
            let timeout_msg = format!("extension did not respond within {} ms", CONTROL_TIMEOUT_MS);
            let resp =
                match forward_to_extension(&req, &id, wait_for, timeout_msg, &pending, &stdout) {
                    Ok(r) => r,
                    // No extension to ask: a status query is exactly how a client
                    // finds that out, so answer it instead of failing it.
                    Err(_) => BridgeResponse::StatusResult {
                        id,
                        extension: false,
                        tab_open: false,
                        logged_in: false,
                        conversation_url: None,
                        build: None,
                    },
                };
            write_response(&mut writer, &resp)?;
        }
    }

    Ok(())
}

/// How long the host waits for the extension on an `ask`.
fn ask_wait(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms.saturating_add(HOST_TIMEOUT_GRACE_MS))
}

/// Forward a parsed request to the extension (serialized with its `type`
/// tag) and wait for the response carrying the same `id`. Only the fields the
/// protocol structs declare are forwarded: the host parses the CLI frame into
/// them, so a field unknown to the protocol (not `target` or `project`, which
/// are declared) is dropped here, not passed through.
///
/// `Err` means the request never reached the extension (the host is shutting
/// down, or the stdout write failed);
/// every other outcome, including a timeout and an id that another caller
/// already waits on (`bad_request`), is an `Ok` response to hand back.
fn forward_to_extension(
    req: &BridgeRequest,
    id: &str,
    wait_for: Duration,
    timeout_msg: String,
    pending: &Arc<PendingMap>,
    stdout: &SharedStdout,
) -> Result<BridgeResponse, String> {
    let envelope_bytes = serde_json::to_vec(req).map_err(|e| format!("serialize: {}", e))?;

    let (tx, rx) = mpsc::channel();
    match pending.insert(id.to_string(), tx) {
        Ok(()) => {}
        Err(Refused::Closed) => {
            return Err("host shutting down (extension disconnected)".to_string())
        }
        Err(Refused::Duplicate) => {
            return Ok(error_response(
                Some(id.to_string()),
                ErrorCode::BadRequest,
                format!("request id {} is already pending", id),
            ))
        }
    }

    let forward_result = {
        let mut so = stdout.lock().unwrap();
        write_frame(&mut *so, &envelope_bytes).and_then(|_| so.flush())
    };
    if let Err(e) = forward_result {
        pending.remove(id);
        return Err(format!("cannot forward to extension: {}", e));
    }

    Ok(match rx.recv_timeout(wait_for) {
        Ok(r) => r,
        Err(RecvTimeoutError::Timeout) => {
            pending.remove(id);
            error_response(Some(id.to_string()), ErrorCode::Timeout, timeout_msg)
        }
        Err(RecvTimeoutError::Disconnected) => {
            pending.remove(id);
            error_response(
                Some(id.to_string()),
                ErrorCode::ExtensionUnavailable,
                "host shut down before extension responded",
            )
        }
    })
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
                // `read_frame` stopped after the length prefix, so the body is
                // still on the stream: drop it and go on. Exiting would fail
                // every other caller for one oversized answer.
                match skip_oversize_frame(&mut reader, len) {
                    Ok(head) => fail_oversize_waiter(&pending, &head, len, cap),
                    Err(e) => {
                        // A refused length prefix is logged as "desynced length
                        // prefix"; the host exits and Chrome starts a new one.
                        eprintln!(
                            "cgpt-bridge-host: stdin error while skipping an oversize frame: {}",
                            e
                        );
                        return ExitCode::from(1);
                    }
                }
                continue;
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
        let response_err = match serde_json::from_slice::<BridgeResponse>(&body) {
            Ok(resp) => {
                handle_extension_response(&pending, resp);
                continue;
            }
            Err(e) => e,
        };

        if let Ok(req) = serde_json::from_slice::<BridgeRequest>(&body) {
            handle_extension_request(&stdout, req);
            continue;
        }

        // Neither shape parsed. If a caller waits on this id, fail it now
        // with the reason instead of letting it run into its timeout.
        let detail = frame_error_reason(&response_err);
        let reason = format!("extension frame not understood: {}", detail);
        let waiting = extract_id(&body).and_then(|id| pending.remove(&id).map(|tx| (id, tx)));
        match waiting {
            Some((id, tx)) => {
                eprintln!(
                    "cgpt-bridge-host: {} (id {}, {} bytes)",
                    reason,
                    id,
                    body.len()
                );
                let _ = tx.send(error_response(Some(id), ErrorCode::Internal, reason));
            }
            None => eprintln!(
                "cgpt-bridge-host: ignoring unrecognized frame from extension ({} bytes): {}",
                body.len(),
                detail
            ),
        }
    }
}

/// How much of an oversize frame is kept to find its `id`. The extension puts
/// `id` right after `type`, so it is within the first few dozen bytes.
const OVERSIZE_HEAD_BYTES: usize = 1_024;

/// Longest `id` taken from the head of an oversize frame; the host's own ids
/// are far shorter, anything longer is not one of them.
const OVERSIZE_ID_MAX_BYTES: usize = 256;

/// Largest oversize frame that is drained. Chrome takes at most 64 MiB from
/// an extension and ours sends about 1 MiB at most, so a bigger length prefix
/// means the stream is out of step, not a real answer.
const OVERSIZE_DRAIN_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Read and drop the `len` bytes of a frame that is over the cap, so the next
/// frame starts where it should. Returns the first `OVERSIZE_HEAD_BYTES`
/// (fewer for a shorter frame) for the id lookup; the rest is never held in
/// memory. A length over `OVERSIZE_DRAIN_MAX_BYTES` is an error, nothing is
/// read.
fn skip_oversize_frame<R: Read>(reader: &mut R, len: u32) -> io::Result<Vec<u8>> {
    let total = u64::from(len);
    if total > OVERSIZE_DRAIN_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("desynced length prefix ({} bytes)", len),
        ));
    }
    let mut head = Vec::new();
    reader
        .by_ref()
        .take(total.min(OVERSIZE_HEAD_BYTES as u64))
        .read_to_end(&mut head)?;
    let rest = total - head.len() as u64;
    let dropped = io::copy(&mut reader.by_ref().take(rest), &mut io::sink())?;
    if head.len() as u64 + dropped != total {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(head)
}

/// The string after the first `"id":` in the head of a frame, when it is
/// complete there and plain (no escapes). The frame is cut off, so it is not
/// parsed as JSON.
fn id_in_frame_head(head: &[u8]) -> Option<String> {
    const KEY: &[u8] = b"\"id\"";
    let mut from = 0;
    while let Some(pos) = head[from..].windows(KEY.len()).position(|w| w == KEY) {
        let mut i = from + pos + KEY.len();
        let skip_ws = |mut i: usize| {
            while head.get(i).is_some_and(u8::is_ascii_whitespace) {
                i += 1;
            }
            i
        };
        i = skip_ws(i);
        if head.get(i) == Some(&b':') {
            i = skip_ws(i + 1);
            if head.get(i) == Some(&b'"') {
                let value = &head[i + 1..];
                let end = value.iter().position(|&b| b == b'"' || b == b'\\')?;
                if value[end] == b'"' && end <= OVERSIZE_ID_MAX_BYTES {
                    return std::str::from_utf8(&value[..end]).ok().map(str::to_string);
                }
                return None;
            }
        }
        from += pos + KEY.len();
    }
    None
}

/// Fail the caller waiting on an oversize frame's id, if one can be found, so
/// it gets an answer now instead of running into its timeout. Only sizes and
/// the host's own request id are logged or sent on, never frame content.
fn fail_oversize_waiter(pending: &PendingMap, head: &[u8], len: u32, cap: u32) {
    // Same wording as the extension's own cap (bridgeProtocol.ts), with the
    // host's limit added.
    let reason = format!("ответ {} байт больше лимита моста ({})", len, cap);
    let waiting = id_in_frame_head(head).and_then(|id| pending.remove(&id).map(|tx| (id, tx)));
    match waiting {
        Some((id, tx)) => {
            eprintln!(
                "cgpt-bridge-host: oversize frame from extension ({} > {}), skipped (id {})",
                len, cap, id
            );
            let _ = tx.send(error_response(Some(id), ErrorCode::OversizeFrame, reason));
        }
        None => eprintln!(
            "cgpt-bridge-host: oversize frame from extension ({} > {}), skipped",
            len, cap
        ),
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
            if tx.send(resp).is_err() {
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
        BridgeRequest::Ask(AskRequest { id, .. })
        | BridgeRequest::Stop(StopRequest { id })
        | BridgeRequest::StopAsk(StopAskRequest { id, .. })
        | BridgeRequest::Status(StatusRequest { id }) => {
            // The extension is never supposed to initiate these at the host.
            // Reply with a clear error rather than silently ignoring.
            error_response(
                Some(id),
                ErrorCode::BadRequest,
                "ask, stop and status requests from extension are not supported",
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

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Longest serde reason (in chars) that is passed on to a caller or the log.
const FRAME_REASON_MAX_CHARS: usize = 200;

/// The serde error of an unparsable extension frame, made safe to show: every
/// double-quoted literal (serde echoes string values from the frame, which can
/// be prompt or answer text) is replaced by a length marker, then the result
/// is cut to `FRAME_REASON_MAX_CHARS` so a huge `type` or `code` cannot push
/// the reply past the frame cap. Field and variant names in backticks stay.
fn frame_error_reason(err: &serde_json::Error) -> String {
    let text = err.to_string();
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '"' {
            out.push(c);
            continue;
        }
        let mut len = 0usize;
        while let Some(d) = chars.next() {
            match d {
                '"' => break,
                // An escaped character counts as one and cannot end the literal.
                '\\' => {
                    chars.next();
                    len += 1;
                }
                _ => len += 1,
            }
        }
        out.push_str(&format!("\"…({} chars)\"", len));
    }
    if out.chars().count() > FRAME_REASON_MAX_CHARS {
        out = out.chars().take(FRAME_REASON_MAX_CHARS).collect();
        out.push('…');
    }
    out
}

fn extract_id(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("id")?.as_str().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask_result(id: &str) -> BridgeResponse {
        serde_json::from_value(serde_json::json!({
            "type": "ask_result", "id": id, "text": "x",
        }))
        .unwrap()
    }

    #[test]
    fn frame_reason_masks_literals_and_is_capped() {
        let err = serde_json::from_str::<bool>(r#""SE\"CRET""#).unwrap_err();
        let r = frame_error_reason(&err);
        assert!(!r.contains("SE"), "{r}");
        assert!(r.contains("\"…(7 chars)\""), "{r}");

        // A real serde error far over the cap: an unknown `type` is echoed in
        // backticks, which the masking leaves alone. Non-ASCII on purpose: the
        // cap counts chars, not bytes, and must not cut inside a character.
        let frame = format!(r#"{{"type": "{}", "id": "x"}}"#, "é".repeat(500));
        let err = serde_json::from_str::<BridgeResponse>(&frame).unwrap_err();
        let raw = err.to_string();
        assert!(
            raw.chars().count() > FRAME_REASON_MAX_CHARS * 2,
            "serde text is short: {raw}"
        );
        assert!(
            raw.contains(&"é".repeat(500)),
            "serde no longer echoes the type: {raw}"
        );
        let r = frame_error_reason(&err);
        assert_eq!(r.chars().count(), FRAME_REASON_MAX_CHARS + 1, "{r}");
        assert!(r.ends_with('…'), "{r}");
        assert!(
            r.len() > FRAME_REASON_MAX_CHARS + 3,
            "no multi-byte char in the cut text: {r}"
        );
    }

    #[test]
    fn duplicate_pending_id_is_refused_and_keeps_the_first_caller() {
        let pending = PendingMap::default();
        let (tx1, rx1) = mpsc::channel();
        let (tx2, rx2) = mpsc::channel();
        assert!(pending.insert("dup".into(), tx1).is_ok());
        assert!(matches!(
            pending.insert("dup".into(), tx2),
            Err(Refused::Duplicate)
        ));
        pending
            .remove("dup")
            .unwrap()
            .send(ask_result("dup"))
            .unwrap();
        assert!(rx1.try_recv().is_ok(), "first caller lost its slot");
        assert!(rx2.try_recv().is_err());
    }

    #[test]
    fn insert_after_shutdown_is_refused() {
        let pending = PendingMap::default();
        pending.drain_with_error(ErrorCode::ExtensionUnavailable, "bye");
        let (tx, _rx) = mpsc::channel();
        assert!(matches!(
            pending.insert("a".into(), tx),
            Err(Refused::Closed)
        ));
    }

    #[test]
    fn duplicate_id_gets_bad_request() {
        let pending = Arc::new(PendingMap::default());
        let (tx, _rx) = mpsc::channel();
        assert!(pending.insert("dup".into(), tx).is_ok());
        let stdout: SharedStdout = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
        let req: BridgeRequest = serde_json::from_value(serde_json::json!({
            "type": "status", "id": "dup",
        }))
        .unwrap();
        let resp = forward_to_extension(
            &req,
            "dup",
            Duration::from_millis(10),
            String::new(),
            &pending,
            &stdout,
        )
        .expect("a reply, not a forwarding failure");
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["type"], "error", "{v}");
        assert_eq!(v["code"], "bad_request", "{v}");
        assert_eq!(v["id"], "dup", "{v}");
    }

    #[test]
    fn skipping_an_oversize_frame_leaves_the_next_one_in_place() {
        let mut bytes = vec![b'x'; 5_000];
        bytes.extend_from_slice(b"next");
        let mut cur = io::Cursor::new(bytes);
        let head = skip_oversize_frame(&mut cur, 5_000).unwrap();
        assert_eq!(head.len(), OVERSIZE_HEAD_BYTES);
        let mut next = Vec::new();
        cur.read_to_end(&mut next).unwrap();
        assert_eq!(next, b"next");

        // A short frame is kept whole; a stream that ends inside it is an error.
        let mut cur = io::Cursor::new(b"abc".to_vec());
        assert_eq!(skip_oversize_frame(&mut cur, 3).unwrap(), b"abc");
        let mut cur = io::Cursor::new(vec![b'x'; 4_000]);
        assert!(skip_oversize_frame(&mut cur, 5_000).is_err());
        let mut cur = io::Cursor::new(vec![b'x'; 10]);
        assert!(skip_oversize_frame(&mut cur, 20).is_err());
    }

    #[test]
    fn a_length_prefix_over_the_ceiling_is_not_drained() {
        // Over the ceiling: refused before anything is read.
        let mut cur = io::Cursor::new(vec![b'x'; 10]);
        let err = skip_oversize_frame(&mut cur, OVERSIZE_DRAIN_MAX_BYTES as u32 + 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(cur.position(), 0);

        // At the ceiling: drained whole.
        let mut stream = io::repeat(b'x');
        let head = skip_oversize_frame(&mut stream, OVERSIZE_DRAIN_MAX_BYTES as u32).unwrap();
        assert_eq!(head.len(), OVERSIZE_HEAD_BYTES);
    }

    #[test]
    fn id_is_found_in_a_cut_off_frame_head() {
        let id = |s: &str| id_in_frame_head(s.as_bytes());
        assert_eq!(
            id(r#"{"type":"ask_result","id":"gw_ask_1_2_3","text":"abc"#),
            Some("gw_ask_1_2_3".into())
        );
        assert_eq!(id(r#"{"id" : "a1","type":"error""#), Some("a1".into()));
        // `"id"` as a value, then the key.
        assert_eq!(id(r#"{"type":"id","id":"a2","#), Some("a2".into()));
        // Not usable: cut off inside the value, escaped, too long, absent.
        assert_eq!(id(r#"{"type":"ask_result","id":"abc"#), None);
        assert_eq!(id(r#"{"id":"a\"b","#), None);
        assert_eq!(id(&format!(r#"{{"id":"{}","#, "a".repeat(300))), None);
        assert_eq!(id(r#"{"type":"ask_result","text":"x"#), None);
        assert_eq!(id(r#"{"id":7,"#), None);
    }

    #[test]
    fn oversize_waiter_is_failed_without_frame_content() {
        let pending = PendingMap::default();
        let (tx, rx) = mpsc::channel();
        pending.insert("w1".into(), tx).unwrap();
        let head = br#"{"type":"ask_result","id":"w1","text":"SECRET"#;
        fail_oversize_waiter(&pending, head, 2_000_000, 1_048_576);
        let v = serde_json::to_value(rx.try_recv().unwrap()).unwrap();
        assert_eq!(v["code"], "oversize_frame", "{v}");
        assert_eq!(v["id"], "w1", "{v}");
        assert_eq!(
            v["message"], "ответ 2000000 байт больше лимита моста (1048576)",
            "{v}"
        );
        assert!(!v.to_string().contains("SECRET"), "{v}");
        // An id nobody waits on fails nobody and leaves no waiter behind.
        fail_oversize_waiter(&pending, head, 2_000_000, 1_048_576);
        assert!(pending.remove("w1").is_none());
    }

    #[test]
    fn ask_wait_saturates_on_huge_timeout() {
        assert_eq!(ask_wait(u64::MAX), Duration::from_millis(u64::MAX));
        assert_eq!(
            ask_wait(1_000),
            Duration::from_millis(1_000 + HOST_TIMEOUT_GRACE_MS)
        );
    }
}
