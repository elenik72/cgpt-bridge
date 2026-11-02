//! C1: Ctrl+C while `cgpt ask` (or a turn of `cgpt agent`) waits for the
//! answer stops that ask in ChatGPT. The CLI sends the host `stop_ask` with
//! the ask's id on a new connection, waits at most about 3 s, prints one
//! line on stderr and then ends as killed by SIGINT, so a shell loop around
//! it stops too. Before the ask frame is written, Ctrl+C ends `cgpt` as it
//! always did and nothing is sent. The real binary runs against a fake host
//! on a temp socket; SIGINT comes from `kill`, so these run on Unix only.
#![cfg(unix)]

mod common;

use std::io::{BufReader, BufWriter, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use cgpt_bridge_protocol::{
    accept, error_response, listen, read_frame, release, write_frame, BridgeResponse, ErrorCode,
    LocalStream,
};
use serde_json::Value;

const SIGINT: i32 = 2;

fn sigint(child: &Child) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    assert_eq!(unsafe { kill(child.id() as i32, SIGINT) }, 0, "kill");
}

fn scratch_dir(label: &str) -> PathBuf {
    let p = PathBuf::from(format!("/tmp/cgb-cc-{}-{}", label, std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn endpoint(label: &str) -> String {
    format!("/tmp/cgb-cc-{}-{}.sock", label, std::process::id())
}

/// What the fake host answers to `stop_ask`.
#[derive(Clone, Copy)]
enum StopReply {
    /// `stopped` with this `was_generating`.
    Stopped(bool),
    /// `stopped` with `other_chat`: the tab shows another chat.
    OtherChat,
    /// `stopped` with `not_sent`: the ask was cancelled before its prompt
    /// was sent.
    NotSent,
    /// A host built before `stop_ask`: it cannot parse the frame.
    BadRequest,
    /// Never answered (an extension built before `stop_ask` ignores it).
    Hold,
}

/// How the fake host behaves.
#[derive(Clone, Copy)]
struct Fake {
    stop: StopReply,
    /// Read nothing of the ask connection until the test says so.
    hold_ask_unread: bool,
}

/// What the test hears from the fake host.
enum Heard {
    /// A connection for the ask was accepted (the frame maybe not read yet).
    AskConnected,
    /// The ask frame was read whole.
    Ask,
    StopAsk,
}

/// A fake host: every connection is one frame. The ask is never answered
/// (its connection is kept open), `stop_ask` as `fake.stop` says. Returns
/// every frame it read. It ends on a connection that sends nothing
/// (`common::Host::join`).
fn spawn_host(
    ep: String,
    fake: Fake,
    heard: mpsc::Sender<Heard>,
    go: mpsc::Receiver<()>,
) -> common::Host<Vec<Value>> {
    let listener = listen(&ep).expect("listen");
    let wake = ep.clone();
    let handle = thread::spawn(move || {
        let mut frames = Vec::new();
        let mut held: Vec<LocalStream> = Vec::new();
        let mut first = true;
        loop {
            let stream = accept(&listener).expect("accept");
            let unread = first && fake.hold_ask_unread;
            first = false;
            if unread {
                let _ = heard.send(Heard::AskConnected);
                let _ = go.recv_timeout(Duration::from_secs(10));
            }
            let body = match read_frame(&mut BufReader::new(&stream)) {
                Ok(b) => b,
                // A cut-off ask frame: go on, and see what came after it.
                Err(_) if unread => continue,
                // The wake-up connection of `join`.
                Err(_) => break,
            };
            let req: Value = serde_json::from_slice(&body).expect("json");
            let id = req["id"].as_str().unwrap().to_string();
            let reply = match req["type"].as_str() {
                Some("ask") => {
                    let _ = heard.send(Heard::Ask);
                    None
                }
                Some("stop_ask") => {
                    let _ = heard.send(Heard::StopAsk);
                    match fake.stop {
                        StopReply::Stopped(was_generating) => Some(BridgeResponse::Stopped {
                            id,
                            was_generating,
                            other_chat: false,
                            not_sent: false,
                        }),
                        StopReply::OtherChat => Some(BridgeResponse::Stopped {
                            id,
                            was_generating: false,
                            other_chat: true,
                            not_sent: false,
                        }),
                        StopReply::NotSent => Some(BridgeResponse::Stopped {
                            id,
                            was_generating: false,
                            other_chat: false,
                            not_sent: true,
                        }),
                        StopReply::BadRequest => Some(error_response(
                            Some(id),
                            ErrorCode::BadRequest,
                            "invalid request: unknown variant `stop_ask`",
                        )),
                        StopReply::Hold => None,
                    }
                }
                // Anything else is recorded, unanswered, for the assertions.
                _ => None,
            };
            frames.push(req);
            match reply {
                Some(resp) => {
                    // The CLI may be gone already; the frames tell the test.
                    let mut w = BufWriter::new(&stream);
                    let _ = write_frame(&mut w, &serde_json::to_vec(&resp).unwrap());
                    let _ = w.flush();
                }
                None => held.push(stream),
            }
        }
        release(&ep);
        frames
    });
    common::Host::new(wake, handle)
}

fn spawn_cgpt(cmd: &str, ep: &str, dir: &PathBuf, stdin: Stdio) -> Child {
    Command::new(env!("CARGO_BIN_EXE_cgpt"))
        .args(["--socket", ep, cmd, "hi"])
        .current_dir(dir)
        .env("CGPT_SPINNER", "off")
        .stdin(stdin)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cgpt")
}

/// Wait for the child (bounded) and return its status and stderr.
fn finish(mut child: Child, within: Duration) -> (ExitStatus, String) {
    let deadline = Instant::now() + within;
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("cgpt did not exit within {:?}", within);
        }
        thread::sleep(Duration::from_millis(10));
    };
    let mut err = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut err).unwrap();
    (status, err)
}

fn expect(heard: &mpsc::Receiver<Heard>, what: &str, ok: impl Fn(&Heard) -> bool) {
    let h = heard
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|_| panic!("the fake host did not hear {what}"));
    assert!(ok(&h), "the fake host heard something else than {what}");
}

/// The CLI arms Ctrl+C right after it wrote the ask frame, and the host may
/// read the frame a moment before that. Nothing outside the process shows
/// the moment (macOS `ps` has no caught-signals column), so the test waits
/// this long; a press that came too early fails the test (no `stop_ask`).
const ARMING: Duration = Duration::from_millis(300);

/// Killed by Ctrl+C, as a shell sees it (130).
fn killed_by_sigint(status: ExitStatus) -> bool {
    status.signal() == Some(SIGINT)
}

fn sigint_during_the_wait(
    cmd: &str,
    dir: &PathBuf,
    label: &str,
    stop: StopReply,
) -> (ExitStatus, String, Vec<Value>) {
    let ep = endpoint(label);
    let (tx, heard) = mpsc::channel();
    let (_go_tx, go) = mpsc::channel();
    let fake = Fake {
        stop,
        hold_ask_unread: false,
    };
    let host = spawn_host(ep.clone(), fake, tx, go);
    let child = spawn_cgpt(cmd, &ep, dir, Stdio::null());
    expect(&heard, "the ask", |h| matches!(h, Heard::Ask));
    thread::sleep(ARMING);
    sigint(&child);
    let (status, err) = finish(child, Duration::from_secs(10));
    let frames = host.join().expect("fake host");
    (status, err, frames)
}

fn ask_sigint(label: &str, stop: StopReply) -> (ExitStatus, String, Vec<Value>) {
    let dir = scratch_dir(label);
    let out = sigint_during_the_wait("ask", &dir, label, stop);
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn types(frames: &[Value]) -> Vec<&str> {
    frames.iter().map(|f| f["type"].as_str().unwrap()).collect()
}

#[test]
fn ctrl_c_during_the_wait_stops_that_ask_and_dies_by_sigint() {
    let (status, err, frames) = ask_sigint("stop", StopReply::Stopped(true));
    assert_eq!(types(&frames), ["ask", "stop_ask"], "{err}");
    let (ask, stop) = (&frames[0], &frames[1]);
    assert_eq!(stop["ask_id"], ask["id"], "the stop names the ask");
    assert_ne!(stop["id"], ask["id"], "the stop has its own request id");
    assert!(killed_by_sigint(status), "{status:?} {err}");
    assert!(
        err.contains("stopped: the answer in ChatGPT was interrupted"),
        "{err}"
    );
}

#[test]
fn ctrl_c_with_a_host_that_refuses_stop_ask_says_it_was_not_stopped() {
    let (status, err, frames) = ask_sigint("old", StopReply::BadRequest);
    assert_eq!(types(&frames), ["ask", "stop_ask"], "{err}");
    assert!(killed_by_sigint(status), "{status:?} {err}");
    assert!(
        err.contains("the answer in ChatGPT was not stopped: "),
        "{err}"
    );
    assert!(err.contains("update cgpt-bridge"), "{err}");
}

#[test]
fn ctrl_c_for_an_ask_the_extension_does_not_know_is_not_called_stopped() {
    let (status, err, frames) = ask_sigint("none", StopReply::Stopped(false));
    assert_eq!(types(&frames), ["ask", "stop_ask"], "{err}");
    assert!(killed_by_sigint(status), "{status:?} {err}");
    assert!(err.contains("not stopped: "), "{err}");
    assert!(!err.contains("stopped: the answer"), "{err}");
}

#[test]
fn ctrl_c_before_the_prompt_was_sent_says_it_was_not_sent() {
    // The extension was still choosing the tab (or the ask was queued): the
    // ask ends `cancelled` and its prompt never reaches ChatGPT.
    let (status, err, frames) = ask_sigint("notsent", StopReply::NotSent);
    assert_eq!(types(&frames), ["ask", "stop_ask"], "{err}");
    assert!(killed_by_sigint(status), "{status:?} {err}");
    assert!(
        err.contains("stopped: the prompt was not sent to ChatGPT"),
        "{err}"
    );
    assert!(!err.contains("not stopped"), "{err}");
}

#[test]
fn ctrl_c_while_the_tab_shows_another_chat_says_so() {
    let (status, err, frames) = ask_sigint("other", StopReply::OtherChat);
    assert_eq!(types(&frames), ["ask", "stop_ask"], "{err}");
    assert!(killed_by_sigint(status), "{status:?} {err}");
    assert!(
        err.contains("not stopped: the tab shows another chat"),
        "{err}"
    );
}

#[test]
fn an_unconfirmed_stop_is_reported_as_such() {
    // Nobody answers the stop (an extension built before `stop_ask` ignores
    // it): after about 3 s the CLI gives up and says so.
    let started = Instant::now();
    let (status, err, frames) = ask_sigint("noack", StopReply::Hold);
    assert_eq!(types(&frames), ["ask", "stop_ask"], "{err}");
    assert!(killed_by_sigint(status), "{status:?} {err}");
    assert!(err.contains("the stop was not confirmed"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_second_ctrl_c_dies_at_once() {
    let ep = endpoint("twice");
    let dir = scratch_dir("twice");
    let (tx, heard) = mpsc::channel();
    let (_go_tx, go) = mpsc::channel();
    let fake = Fake {
        stop: StopReply::Hold,
        hold_ask_unread: false,
    };
    let host = spawn_host(ep.clone(), fake, tx, go);
    let child = spawn_cgpt("ask", &ep, &dir, Stdio::null());
    expect(&heard, "the ask", |h| matches!(h, Heard::Ask));
    thread::sleep(ARMING);
    sigint(&child);
    // The stop waits for an answer that never comes; the second press ends it.
    expect(&heard, "the stop", |h| matches!(h, Heard::StopAsk));
    let pressed = Instant::now();
    sigint(&child);
    let (status, err) = finish(child, Duration::from_secs(10));
    assert!(
        pressed.elapsed() < Duration::from_millis(1_500),
        "{:?}",
        pressed.elapsed()
    );
    assert!(killed_by_sigint(status), "{status:?} {err}");
    host.join().expect("fake host");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ctrl_c_before_the_ask_frame_is_written_just_ends_cgpt() {
    // A prompt larger than the socket buffers and a host that does not read
    // it yet: the CLI is still writing the frame when SIGINT comes.
    let ep = endpoint("early");
    let dir = scratch_dir("early");
    let (tx, heard) = mpsc::channel();
    let (go_tx, go) = mpsc::channel();
    let fake = Fake {
        stop: StopReply::Stopped(true),
        hold_ask_unread: true,
    };
    let host = spawn_host(ep.clone(), fake, tx, go);
    let mut child = spawn_cgpt("ask", &ep, &dir, Stdio::piped());
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all("lorem ipsum dolor sit amet\n".repeat(30_000).as_bytes())
            .unwrap();
    }
    expect(&heard, "the ask connection", |h| {
        matches!(h, Heard::AskConnected)
    });
    thread::sleep(ARMING);
    sigint(&child);
    let (status, err) = finish(child, Duration::from_secs(10));
    go_tx.send(()).unwrap();
    let frames = host.join().expect("fake host");
    // As before C1: the default action of SIGINT, and nothing more sent.
    assert!(killed_by_sigint(status), "{status:?} {err}");
    assert!(types(&frames).iter().all(|t| *t == "ask"), "{frames:?}");
    assert!(!err.contains("stop"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ctrl_c_during_an_agent_turn_stops_that_ask_and_ends_the_session_cancelled() {
    let dir = scratch_dir("agent");
    let (status, err, frames) =
        sigint_during_the_wait("agent", &dir, "agent", StopReply::Stopped(true));
    assert_eq!(types(&frames), ["ask", "stop_ask"], "{err}");
    assert_eq!(frames[1]["ask_id"], frames[0]["id"]);
    assert!(killed_by_sigint(status), "{status:?} {err}");
    assert!(
        err.contains("stopped: the answer in ChatGPT was interrupted"),
        "{err}"
    );
    let session = std::fs::read_to_string(dir.join(".cgpt-bridge/session.json")).unwrap();
    let session: Value = serde_json::from_str(&session).unwrap();
    assert_eq!(session["status"], "user_cancelled", "{session}");
    let plan = std::fs::read_to_string(dir.join(".cgpt-bridge/plan.jsonl")).unwrap();
    assert!(plan.contains("ctrl_c"), "{plan}");
    let _ = std::fs::remove_dir_all(&dir);
}
