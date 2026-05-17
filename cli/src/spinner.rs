//! Tiny stderr-only spinner for long-running operations + a `Phase` helper
//! that bookends a unit of work with header / footer lines so the user
//! always sees progress even when the animated spinner is no-op (piped
//! stderr, no TTY, CI).
//!
//! Why on stderr: cgpt reserves stdout for assistant content. The spinner
//! is progress UI, not output.
//!
//! Env override:
//!   * `CGPT_SPINNER=always` — force animation on (useful when stderr is
//!     wrapped by a terminal multiplexer that fails `is_terminal`).
//!   * `CGPT_SPINNER=off` — disable animation. Phase header/footer lines
//!     are still printed.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// One Spinner = one running indicator. Constructed via [`Spinner::start`],
/// stopped via [`Spinner::stop`] or by dropping.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    enabled: bool,
}

impl Spinner {
    /// Start a spinner with `message`. If stderr is not a TTY, returns a
    /// disabled (no-op) spinner — callers can still call `.stop()` safely.
    pub fn start(message: impl Into<String>) -> Self {
        let enabled = spinner_enabled();
        let stop = Arc::new(AtomicBool::new(false));
        if !enabled {
            return Self {
                stop,
                handle: None,
                enabled: false,
            };
        }
        let msg = message.into();
        let stop_for_thread = Arc::clone(&stop);
        let handle = thread::spawn(move || run_spinner(stop_for_thread, msg));
        Self {
            stop,
            handle: Some(handle),
            enabled: true,
        }
    }

    /// Replace the current message. Cheap; the next frame will pick it up.
    /// Currently we recreate the spinner because the message lives in the
    /// thread; for v0.1 this is fine because messages change at coarse
    /// boundaries (per turn / per phase) not on every frame.
    pub fn rename(&mut self, message: impl Into<String>) {
        if !self.enabled {
            return;
        }
        // Stop the current frame loop, then start a new one. Cheap.
        self.stop_now();
        let stop = Arc::new(AtomicBool::new(false));
        let msg = message.into();
        let stop_for_thread = Arc::clone(&stop);
        let handle = thread::spawn(move || run_spinner(stop_for_thread, msg));
        self.stop = stop;
        self.handle = Some(handle);
    }

    /// Stop the spinner and clear its line. Safe to call multiple times.
    pub fn stop(mut self) {
        self.stop_now();
    }

    fn stop_now(&mut self) {
        if !self.enabled {
            return;
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop_now();
    }
}

const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const FRAME_INTERVAL: Duration = Duration::from_millis(80);

fn spinner_enabled() -> bool {
    match std::env::var("CGPT_SPINNER").ok().as_deref() {
        Some("always") | Some("1") | Some("on") => true,
        Some("off") | Some("0") | Some("never") => false,
        _ => io::stderr().is_terminal(),
    }
}

/// A single user-visible phase: prints `▶ <label>` immediately on
/// construction, runs an animated spinner alongside it, and on `done` /
/// `fail` clears the spinner line and prints a closing `◀ <summary>` or
/// `✗ <summary>` line. Phase markers are always printed regardless of
/// TTY state so non-interactive runs still get progress feedback.
pub struct Phase {
    label: String,
    spinner: Spinner,
    started: Instant,
    closed: bool,
}

impl Phase {
    pub fn start(label: impl Into<String>) -> Self {
        let label = label.into();
        // Always print the header — works in piped stderr / CI / non-TTY.
        eprintln!("▶ {}", label);
        let spinner = Spinner::start(label.clone());
        Self {
            label,
            spinner,
            started: Instant::now(),
            closed: false,
        }
    }

    /// Replace the in-flight spinner message without closing the phase.
    /// Useful when one phase has internal sub-steps.
    pub fn rename(&mut self, message: impl Into<String>) {
        self.spinner.rename(message);
    }

    /// Finish a successful phase. `summary` becomes the `◀` line; if empty,
    /// a generic `<label> done in Nms` is used.
    pub fn done(mut self, summary: impl AsRef<str>) {
        self.close_spinner();
        let s = summary.as_ref();
        if s.is_empty() {
            eprintln!(
                "◀ {} ({}ms)",
                self.label,
                self.started.elapsed().as_millis()
            );
        } else {
            eprintln!("◀ {}", s);
        }
    }

    /// Finish a failed phase with an `✗ <summary>` line.
    pub fn fail(mut self, summary: impl AsRef<str>) {
        self.close_spinner();
        eprintln!("✗ {}", summary.as_ref());
    }

    fn close_spinner(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        // Move the spinner out so its `stop` runs once.
        let s = std::mem::replace(
            &mut self.spinner,
            Spinner {
                stop: Arc::new(AtomicBool::new(true)),
                handle: None,
                enabled: false,
            },
        );
        s.stop();
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if !self.closed {
            self.close_spinner();
            eprintln!("◀ {} (dropped)", self.label);
        }
    }
}

fn run_spinner(stop: Arc<AtomicBool>, message: String) {
    let started = Instant::now();
    let mut i: usize = 0;
    let mut err = io::stderr().lock();

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let frame = FRAMES[i % FRAMES.len()];
        let elapsed = started.elapsed().as_secs();
        // \x1b[2K = ANSI "erase entire line", \r = return to col 0.
        // We rewrite the same line each tick so the spinner doesn't scroll.
        let _ = write!(err, "\r\x1b[2K{} {} ({}s)", frame, message, elapsed);
        let _ = err.flush();
        i += 1;
        // Sleep in small slices so .stop() returns quickly.
        let target = Instant::now() + FRAME_INTERVAL;
        while Instant::now() < target {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    // Clear our line on exit so the next stderr message starts clean.
    let _ = write!(err, "\r\x1b[2K");
    let _ = err.flush();
}
