//! The gateway, its real `BridgeBackend` and the real native host binary,
//! with the extension played on the host's stdin/stdout.
//!
//! Each test starts the host with `TMPDIR` set to its own sandbox and no
//! `XDG_RUNTIME_DIR` (on Windows `USERNAME` names the pipe), checks that the
//! socket the host reports is that sandbox's and not the default one, and
//! serves the gateway on an ephemeral port. The fake extension answers every
//! `status` itself; the tests script `ask` and `stop`.

use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cgpt_bridge_protocol::{default_endpoint, read_frame, write_frame, HOST_TIMEOUT_GRACE_MS};
use serde_json::{json, Value};

use crate::backend::BridgeBackend;
use crate::config::{Config, DEFAULT_PORT};
use crate::cooldown::Cooldown;
use crate::engine::Engine;
use crate::http::{start, Running};
use crate::prompt::INTERRUPTED_NOTE;
use crate::session::{SessionState, Sessions};

const TOKEN: &str = "e2e-token";
const SESSION: &str = "ses_e2e";
const CHAT: &str = "https://chatgpt.com/c/e2e";
/// Upper bound for anything the tests wait for; only reached on a failure.
const WAIT: Duration = Duration::from_secs(30);
/// How long after `timeout_ms` the fake extension reports its own timeout,
/// unless `CGW_E2E_EXT_LATE_MS` says otherwise: after the gateway's turn
/// timeout and inside the host's grace, 1 s before its end.
const EXT_LATE_MS: u64 = HOST_TIMEOUT_GRACE_MS - 1_000;

/// The host binary of this build, next to the `deps/` directory of this
/// test. It is built once per test process, so a run of the gateway alone
/// never tests a stale host (a no-op when `cargo test --workspace` has just
/// built it).
fn host_bin() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let exe = std::env::current_exe().expect("test executable path");
        let dir = exe
            .parent()
            .and_then(Path::parent)
            .expect("test executable in target/<profile>/deps");
        let bin = dir.join(format!("cgpt-bridge-host{}", std::env::consts::EXE_SUFFIX));
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut cmd = Command::new(cargo);
        cmd.args(["build", "-p", "cgpt-bridge-host"]);
        if dir.ends_with("release") {
            cmd.arg("--release");
        }
        let out = cmd.output().expect("run cargo build");
        assert!(
            out.status.success(),
            "building cgpt-bridge-host failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // A build into another target dir (`--target`, `--target-dir`)
        // leaves nothing here.
        assert!(
            bin.exists(),
            "{} not found; run `cargo test --workspace`",
            bin.display()
        );
        bin
    })
    .clone()
}

/// A fresh directory for one test, removed with everything in it (the
/// socket included) when dropped, also when the test fails. `/tmp` on Unix
/// keeps the socket path short enough for `sun_path` on macOS. Declared
/// before the [`Host`], so the host is gone before its directory.
struct Sandbox(PathBuf);

impl Sandbox {
    fn new(tag: &str) -> Sandbox {
        let root = if cfg!(unix) {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let dir = root.join(format!("cgw-e2e-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Sandbox(dir)
    }
}

impl std::ops::Deref for Sandbox {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        if thread::panicking() {
            // A turn still running in the gateway when the test failed saves
            // its session once the host is gone, re-creating the directory.
            for _ in 0..10 {
                thread::sleep(Duration::from_millis(100));
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}

/// The extension's end of the host's stdio.
struct Extension {
    /// Every frame the host wrote, in order, `status` included, with the
    /// moment it was read off the pipe.
    frames: Receiver<(Instant, Value)>,
    /// The host's stdin; `None` once closed.
    to_host: Arc<Mutex<Option<BufWriter<ChildStdin>>>>,
}

fn write_to(to_host: &Mutex<Option<BufWriter<ChildStdin>>>, frame: &Value) -> std::io::Result<()> {
    let mut guard = to_host.lock().unwrap();
    let writer = guard.as_mut().expect("the host's stdin is open");
    write_frame(writer, frame.to_string().as_bytes())?;
    writer.flush()
}

impl Extension {
    fn attach(stdin: ChildStdin, stdout: ChildStdout) -> Extension {
        let to_host = Arc::new(Mutex::new(Some(BufWriter::new(stdin))));
        let (tx, frames) = mpsc::channel();
        let to_host2 = to_host.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(body) = read_frame(&mut reader) {
                let read_at = Instant::now();
                let frame: Value = serde_json::from_slice(&body).expect("host frame is JSON");
                if frame["type"] == "status" {
                    // A current extension with its tab open and logged in.
                    let _ = write_to(
                        &to_host2,
                        &json!({"type": "status_result", "id": frame["id"],
                            "extension": true, "tab_open": true, "logged_in": true}),
                    );
                }
                if tx.send((read_at, frame)).is_err() {
                    break;
                }
            }
        });
        Extension { frames, to_host }
    }

    fn next(&self) -> Value {
        self.next_stamped().1
    }

    /// The next frame and when it was read off the host's stdout.
    fn next_stamped(&self) -> (Instant, Value) {
        match self.frames.recv_timeout(WAIT) {
            Ok(stamped) => stamped,
            Err(RecvTimeoutError::Timeout) => panic!("no frame from the host in {:?}", WAIT),
            Err(RecvTimeoutError::Disconnected) => panic!("the host closed its stdout"),
        }
    }

    /// The next frame other than `status`, which must be of type `kind`.
    fn expect(&self, kind: &str) -> Value {
        self.expect_stamped(kind).1
    }

    /// [`Extension::expect`] with the moment the frame was read.
    fn expect_stamped(&self, kind: &str) -> (Instant, Value) {
        loop {
            let (read_at, frame) = self.next_stamped();
            if frame["type"] != "status" {
                assert_eq!(frame["type"], kind, "{}", frame);
                return (read_at, frame);
            }
        }
    }

    /// Nothing at all comes from the host for `period`.
    fn quiet_for(&self, period: Duration) {
        match self.frames.recv_timeout(period) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok((_, frame)) => panic!("unexpected frame from the host: {}", frame),
            Err(RecvTimeoutError::Disconnected) => panic!("the host closed its stdout"),
        }
    }

    fn reply(&self, frame: Value) {
        write_to(&self.to_host, &frame).expect("write to the host's stdin");
    }

    /// Close the host's stdin, as Chrome does when the service worker goes.
    fn close(&self) {
        self.to_host.lock().unwrap().take();
    }
}

/// A host process in a sandbox. Its stderr is collected and printed when the
/// test fails.
struct Host {
    child: Child,
    endpoint: String,
    ext: Extension,
    log: Arc<Mutex<String>>,
    log_reader: Option<JoinHandle<()>>,
}

impl Host {
    /// Start the host with `TMPDIR=<sandbox>`, without `XDG_RUNTIME_DIR`, and
    /// wait until it listens. Two hosts of one sandbox use the same socket.
    fn spawn(sandbox: &Path) -> Host {
        let mut cmd = Command::new(host_bin());
        cmd.env("TMPDIR", sandbox).env_remove("XDG_RUNTIME_DIR");
        #[cfg(windows)]
        cmd.env("USERNAME", sandbox.file_name().unwrap());
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn cgpt-bridge-host");
        let log = Arc::new(Mutex::new(String::new()));
        let (started_tx, started_rx) = mpsc::channel();
        let stderr = child.stderr.take().unwrap();
        let log_reader = {
            let log = log.clone();
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    let Ok(line) = line else { break };
                    // "cgpt-bridge-host v… started (pid …, socket <endpoint>)",
                    // logged once the host listens.
                    if let Some(endpoint) = line
                        .split_once(", socket ")
                        .and_then(|(_, rest)| rest.strip_suffix(')'))
                    {
                        let _ = started_tx.send(endpoint.to_string());
                    }
                    let mut log = log.lock().unwrap();
                    log.push_str(&line);
                    log.push('\n');
                }
            })
        };
        let ext = Extension::attach(child.stdin.take().unwrap(), child.stdout.take().unwrap());
        let mut host = Host {
            child,
            endpoint: String::new(),
            ext,
            log,
            log_reader: Some(log_reader),
        };
        host.endpoint = started_rx
            .recv_timeout(WAIT)
            .expect("the host logs its socket when it listens");
        assert_ne!(
            host.endpoint,
            default_endpoint(),
            "the test host must not use the default socket"
        );
        #[cfg(unix)]
        assert!(
            Path::new(&host.endpoint).starts_with(sandbox),
            "socket {} is outside the sandbox {}",
            host.endpoint,
            sandbox.display()
        );
        #[cfg(windows)]
        assert_eq!(
            host.endpoint,
            format!(
                "cgpt-bridge-{}",
                sandbox.file_name().unwrap().to_string_lossy()
            )
        );
        host
    }

    /// Wait for the host to exit (its stdin closed) and return its whole log.
    fn exit(&mut self) -> String {
        self.ext.close();
        self.child.wait().expect("wait for the host");
        if let Some(reader) = self.log_reader.take() {
            reader.join().expect("host stderr reader");
        }
        self.log.lock().unwrap().clone()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if thread::panicking() {
            eprintln!(
                "---- host stderr ----\n{}---- end host stderr ----",
                self.log.lock().unwrap()
            );
        }
    }
}

/// The gateway on an ephemeral port, talking to `endpoint` through the real
/// `BridgeBackend`, with its sessions in the sandbox.
fn gateway(sandbox: &Path, endpoint: &str, tweak: impl FnOnce(&mut Config)) -> Running {
    let mut cfg = Config::with_token(TOKEN.to_string());
    cfg.port = 0;
    tweak(&mut cfg);
    let engine = Engine::new(
        Sessions::new(sandbox.join("sessions")),
        cfg.allow_tools.clone(),
        Cooldown::new(sandbox.join("cooldown.json")),
    );
    let gw = start(
        cfg,
        Box::new(BridgeBackend::new(endpoint.to_string())),
        engine,
    )
    .unwrap();
    assert_ne!(gw.port(), DEFAULT_PORT);
    gw
}

fn session_state(sandbox: &Path) -> (SessionState, Option<String>) {
    let s = Sessions::new(sandbox.join("sessions"))
        .load(SESSION)
        .expect("the session is saved");
    (s.state, s.chat_url)
}

/// The first request of the session, and the one after ChatGPT answered it.
fn first_request() -> Value {
    json!({
        "model": "chatgpt-web",
        "messages": [
            { "role": "system", "content": "rules" },
            { "role": "user", "content": "привет" },
        ],
    })
}

fn second_request(stream: bool) -> Value {
    let mut req = first_request();
    req["messages"].as_array_mut().unwrap().extend([
        json!({ "role": "assistant", "content": "здравствуй" }),
        json!({ "role": "user", "content": "расскажи сказку" }),
    ]);
    req["stream"] = json!(stream);
    req
}

/// POST a chat request of the session on a fresh connection.
fn post(port: u16, body: &Value) -> TcpStream {
    let body = body.to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let head = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\
         Authorization: Bearer {}\r\nx-opencode-session-id: {}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        TOKEN,
        SESSION,
        body.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body.as_bytes()).unwrap();
    stream
}

/// Status and JSON body of a whole (not streamed) answer.
fn answer(mut stream: TcpStream) -> (u16, Value) {
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("a whole answer");
    let (head, body) = raw.split_once("\r\n\r\n").expect("header end");
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = serde_json::from_str(body).unwrap_or_else(|e| panic!("body {:?}: {}", body, e));
    (status, body)
}

/// Send `req` in the background; join for its status and body.
fn send(port: u16, req: Value) -> JoinHandle<(u16, Value)> {
    thread::spawn(move || answer(post(port, &req)))
}

/// The first turn: a new chat, answered with `CHAT`.
fn first_turn(host: &Host, port: u16) {
    let call = send(port, first_request());
    let ask = host.ext.expect("ask");
    assert_eq!(ask["target"], json!({"kind": "new_chat"}));
    host.ext.reply(json!({"type": "ask_result", "id": ask["id"],
        "text": "здравствуй", "conversation_url": CHAT}));
    let (status, body) = call.join().unwrap();
    assert_eq!(status, 200, "{}", body);
}

/// The ask of a retried second turn: the same chat, the note first, the
/// user's message once and no bootstrap.
fn assert_retry_with_note(ask: &Value) {
    assert_eq!(ask["target"], json!({"kind": "conversation", "url": CHAT}));
    let text = ask["text"].as_str().unwrap();
    assert!(text.starts_with(INTERRUPTED_NOTE), "{}", text);
    assert_eq!(text.matches("расскажи сказку").count(), 1, "{}", text);
    assert!(
        !text.contains("Правила шлюза"),
        "no bootstrap again: {}",
        text
    );
}

#[test]
fn extension_timeout_before_the_host_grace_is_a_504() {
    let dir = Sandbox::new("timeout");
    let mut host = Host::spawn(&dir);
    let gw = gateway(&dir, &host.endpoint, |c| c.turn_timeout_ms = 1_000);

    let call = send(gw.port(), first_request());
    let ext_late_ms = std::env::var("CGW_E2E_EXT_LATE_MS")
        .map(|v| v.parse().expect("CGW_E2E_EXT_LATE_MS is a number of ms"))
        .unwrap_or(EXT_LATE_MS);
    assert!(
        ext_late_ms > 0 && ext_late_ms < HOST_TIMEOUT_GRACE_MS,
        "CGW_E2E_EXT_LATE_MS must be inside the host's grace"
    );
    let (received, ask) = host.ext.expect_stamped("ask");
    let limit = ask["timeout_ms"].as_u64().unwrap();
    assert!(limit > 0 && limit <= 1_000, "timeout_ms {}", limit);
    // The extension gives up on its own at L + 4 s: past the gateway's turn
    // timeout, inside the host's grace. It says so with a `timeout` error.
    let late = Duration::from_millis(limit + ext_late_ms);
    thread::sleep(late.saturating_sub(received.elapsed()));
    let error = json!({"type": "error", "id": ask["id"], "code": "timeout",
        "message": format!("answer not finished within {} ms", limit)});
    host.ext.reply(error);
    let answered_ms = received.elapsed().as_millis() as u64;
    // A timeout may leave ChatGPT writing: the gateway presses Stop.
    let stop = host.ext.expect("stop");
    let stopped = json!({"type": "stopped", "id": stop["id"], "was_generating": true});
    host.ext.reply(stopped);

    let (status, body) = call.join().unwrap();
    assert_eq!(status, 504, "{}", body);
    assert_eq!(body["error"]["code"], "timeout", "{}", body);
    assert!(
        received.elapsed() >= late,
        "the 504 came before the extension's error"
    );
    assert_eq!(session_state(&dir).0, SessionState::Interrupted);

    // The extension's error reached the waiting gateway; had the host timed
    // out first, it would have logged the error's id as unknown.
    let log = host.exit();
    assert!(log.contains("clean shutdown"), "{}", log);
    if log.contains("unknown id") || log.contains("gave up") {
        // An error that went out more than half the margin late means the
        // runner stalled; one sent on time means the host changed.
        let margin = HOST_TIMEOUT_GRACE_MS - ext_late_ms;
        assert!(
            answered_ms <= limit + ext_late_ms + margin / 2,
            "test timing margin exceeded: the fake extension answered {} ms after \
             the ask, planned L + {} ms with L = {} ms, host grace L + {} ms; \
             a lower CGW_E2E_EXT_LATE_MS widens the margin\n{}",
            answered_ms,
            ext_late_ms,
            limit,
            HOST_TIMEOUT_GRACE_MS,
            log
        );
        panic!(
            "the host timed out before the extension's timeout error, sent on time \
             {} ms after the ask (L = {} ms): is the host's grace still {} ms?\n{}",
            answered_ms, limit, HOST_TIMEOUT_GRACE_MS, log
        );
    }

    gw.shutdown();
}

#[test]
fn stdin_closed_mid_ask_leaves_the_session_interrupted_and_the_retry_has_the_note() {
    let dir = Sandbox::new("stdin");
    let mut host = Host::spawn(&dir);
    let gw = gateway(&dir, &host.endpoint, |_| {});
    let port = gw.port();
    first_turn(&host, port);

    let call = send(port, second_request(false));
    let ask = host.ext.expect("ask");
    assert_eq!(ask["target"], json!({"kind": "conversation", "url": CHAT}));
    assert!(!ask["text"].as_str().unwrap().starts_with(INTERRUPTED_NOTE));
    // The service worker goes away while ChatGPT has the message.
    let log = host.exit();
    assert!(log.contains("clean shutdown"), "{}", log);
    let (status, body) = call.join().unwrap();
    assert_eq!(status, 503, "{}", body);
    assert_eq!(body["error"]["code"], "extension_unavailable", "{}", body);
    assert_eq!(
        session_state(&dir),
        (SessionState::Interrupted, Some(CHAT.to_string()))
    );

    // Chrome starts a new host; OpenCode sends the same request again.
    let host2 = Host::spawn(&dir);
    assert_eq!(host2.endpoint, host.endpoint);
    let call = send(port, second_request(false));
    let ask = host2.ext.expect("ask");
    assert_retry_with_note(&ask);
    host2
        .ext
        .reply(json!({"type": "ask_result", "id": ask["id"],
        "text": "Жили-были дед и баба.", "conversation_url": CHAT}));
    let (status, body) = call.join().unwrap();
    assert_eq!(status, 200, "{}", body);
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "Жили-были дед и баба."
    );
    assert_eq!(session_state(&dir).0, SessionState::Idle);

    gw.shutdown();
}

#[test]
fn an_answer_from_another_chat_is_a_502_and_the_retry_goes_to_the_old_chat_with_the_note() {
    let dir = Sandbox::new("other-chat");
    let host = Host::spawn(&dir);
    let gw = gateway(&dir, &host.endpoint, |_| {});
    let port = gw.port();
    first_turn(&host, port);

    let call = send(port, second_request(false));
    let ask = host.ext.expect("ask");
    assert_eq!(ask["target"], json!({"kind": "conversation", "url": CHAT}));
    host.ext.reply(json!({"type": "ask_result", "id": ask["id"],
        "text": "чужой ответ", "conversation_url": "https://chatgpt.com/c/other"}));
    let (status, body) = call.join().unwrap();
    assert_eq!(status, 502, "{}", body);
    assert_eq!(body["error"]["code"], "dom_failure", "{}", body);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("ответ из другого чата"),
        "{}",
        body
    );
    assert_eq!(
        session_state(&dir),
        (SessionState::Interrupted, Some(CHAT.to_string()))
    );

    let call = send(port, second_request(false));
    let ask = host.ext.expect("ask");
    assert_retry_with_note(&ask);
    host.ext.reply(json!({"type": "ask_result", "id": ask["id"],
        "text": "Жили-были дед и баба.", "conversation_url": CHAT}));
    let (status, body) = call.join().unwrap();
    assert_eq!(status, 200, "{}", body);
    assert_eq!(
        session_state(&dir),
        (SessionState::Idle, Some(CHAT.to_string()))
    );

    gw.shutdown();
}

#[test]
fn stop_during_an_ask_drops_its_answer_and_holds_the_queue_until_stopped() {
    let dir = Sandbox::new("stop");
    let host = Host::spawn(&dir);
    let gw = gateway(&dir, &host.endpoint, |c| {
        c.keepalive_ms = 50;
        c.first_byte_wait_ms = 20;
    });
    let port = gw.port();
    first_turn(&host, port);

    // A streams; the user presses Stop while ChatGPT writes and types a new
    // message: B is A's history without an answer plus "другое". B is fully
    // written, and queues behind A, before A's client hangs up.
    let mut a = post(port, &second_request(true));
    let mut buf = [0u8; 64];
    assert!(a.read(&mut buf).unwrap() > 0, "A's stream started");
    let ask_a = host.ext.expect("ask");
    let mut b_request = second_request(false);
    b_request["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "role": "user", "content": "другое" }));
    let b_stream = post(port, &b_request);
    let b = thread::spawn(move || answer(b_stream));
    drop(a);
    let stop = host.ext.expect("stop");

    // A's answer still arrives, then the extension takes its time to stop.
    let answer_a = json!({"type": "ask_result", "id": ask_a["id"],
        "text": "ответ A", "conversation_url": CHAT});
    host.ext.reply(answer_a);
    host.ext.quiet_for(Duration::from_secs(1));
    let stopped = json!({"type": "stopped", "id": stop["id"], "was_generating": true});
    host.ext.reply(stopped);

    // Only now does B start: its status probe, then its ask.
    assert_eq!(host.ext.next()["type"], "status");
    let ask_b = host.ext.next();
    assert_eq!(ask_b["type"], "ask", "{}", ask_b);
    // A's answer was dropped: A's turn does not count as delivered, so B
    // resends A's message after the note. The note comes through the state
    // (Interrupted after the dropped answer); had A's answer been taken, B
    // would carry only "другое", and the history would lack A's message.
    assert_eq!(
        ask_b["target"],
        json!({"kind": "conversation", "url": CHAT})
    );
    let text = ask_b["text"].as_str().unwrap();
    assert!(text.starts_with(INTERRUPTED_NOTE), "{}", text);
    assert_eq!(text.matches("расскажи сказку").count(), 1, "{}", text);
    assert_eq!(text.matches("другое").count(), 1, "{}", text);
    assert!(!text.contains("ответ A"), "{}", text);
    assert!(
        !text.contains("Правила шлюза"),
        "no bootstrap again: {}",
        text
    );
    let answer_b = json!({"type": "ask_result", "id": ask_b["id"],
        "text": "ответ B", "conversation_url": CHAT});
    host.ext.reply(answer_b);
    let (status, body) = b.join().unwrap();
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["choices"][0]["message"]["content"], "ответ B");
    assert_eq!(session_state(&dir).0, SessionState::Idle);

    gw.shutdown();
}
