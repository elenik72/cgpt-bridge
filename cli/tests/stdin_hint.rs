//! A piped stdin that stays open and silent (a script or a background job
//! that passes nothing) makes `cgpt ask` say, after a few seconds, that it is
//! still reading it, instead of waiting in silence. A pipe that closes at
//! once says nothing. No host is needed: the socket does not exist.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const HINT: &str = "still reading the prompt from stdin";
const NO_SOCKET: &str = "/tmp/cgb-sh-nope.sock";

fn scratch_dir(label: &str) -> PathBuf {
    let p = PathBuf::from(format!("/tmp/cgb-sh-{}-{}", label, std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn an_open_silent_stdin_gets_the_hint_and_closing_it_goes_on() {
    let dir = scratch_dir("open");
    let mut child = Command::new(env!("CARGO_BIN_EXE_cgpt"))
        .args(["--no-spinner", "--socket", NO_SOCKET, "ask", "hi"])
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cgpt");
    let started = Instant::now();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });

    let hint = loop {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(line) if line.contains(HINT) => break line,
            Ok(_) => continue,
            Err(e) => {
                let _ = child.kill();
                panic!("no stdin hint within 20 s: {e}");
            }
        }
    };
    let after = started.elapsed();
    assert!(
        after >= Duration::from_millis(4_500),
        "hint after {after:?}"
    );
    assert!(hint.contains("< /dev/null"), "{hint}");

    // Closing stdin lets it go on: the missing socket is a setup error.
    drop(child.stdin.take());
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(3));
}

#[test]
fn a_pipe_that_closes_at_once_gets_no_hint() {
    let dir = scratch_dir("closed");
    let mut child = Command::new(env!("CARGO_BIN_EXE_cgpt"))
        .args(["--no-spinner", "--socket", NO_SOCKET, "ask"])
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cgpt");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"a question").unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains(HINT), "{err}");
}
