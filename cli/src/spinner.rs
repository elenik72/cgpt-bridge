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

use crate::runner::char_cols;

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

/// Width used when the terminal's is unknown (as `runner::panel_width`).
const DEFAULT_COLS: usize = 80;

/// How often the width is read again (the window may be resized).
const WIDTH_READ_INTERVAL: Duration = Duration::from_secs(1);

/// Columns from a terminal size reading: 0 (some terminals report 0x0) or
/// an error is unknown, 80.
fn cols_from(size: io::Result<(u16, u16)>) -> usize {
    match size {
        Ok((w, _)) if w > 0 => w as usize,
        _ => DEFAULT_COLS,
    }
}

/// Whether the terminal's size can be read without side effects. crossterm
/// asks `/dev/tty`, else stdout, and when neither answers it runs `tput`
/// twice: so only with stdout a terminal or `/dev/tty` there (the spinner
/// draws on stderr, also when stdout is redirected). Windows asks the
/// console.
fn width_readable() -> bool {
    cfg!(windows) || io::stdout().is_terminal() || std::fs::File::open("/dev/tty").is_ok()
}

/// The terminal's width, read at most once per WIDTH_READ_INTERVAL.
struct Width {
    cols: usize,
    read_at: Option<Instant>,
}

impl Width {
    fn new() -> Self {
        Self {
            cols: DEFAULT_COLS,
            read_at: None,
        }
    }

    fn get(&mut self, now: Instant, read: impl FnOnce() -> usize) -> usize {
        if self
            .read_at
            .is_none_or(|at| now.duration_since(at) >= WIDTH_READ_INTERVAL)
        {
            self.cols = read();
            self.read_at = Some(now);
        }
        self.cols
    }
}

fn cols(s: &str) -> usize {
    s.chars().map(char_cols).sum()
}

/// One spinner frame, `<frame> <message> (<n>s)`, kept within `width - 1`
/// columns (the last column is left free: some terminals wrap on it). The
/// message is cut first, with `…`; widths are overestimated (`char_cols`).
fn frame_line(frame: &str, message: &str, elapsed: u64, width: usize) -> String {
    let budget = width.saturating_sub(1);
    let message: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let suffix = format!(" ({}s)", elapsed);
    let fixed = cols(frame) + 1 + cols(&suffix);
    if fixed + cols(&message) <= budget {
        return format!("{} {}{}", frame, message, suffix);
    }
    let ellipsis = '…';
    if fixed + char_cols(ellipsis) <= budget {
        let room = budget - fixed - char_cols(ellipsis);
        let mut cut = String::new();
        let mut used = 0;
        for c in message.chars() {
            if used + char_cols(c) > room {
                break;
            }
            used += char_cols(c);
            cut.push(c);
        }
        cut.push(ellipsis);
        return format!("{} {}{}", frame, cut, suffix);
    }
    // Too narrow even for the time: the frame alone, if it fits.
    if cols(frame) <= budget {
        frame.to_string()
    } else {
        String::new()
    }
}

fn run_spinner(stop: Arc<AtomicBool>, message: String) {
    let started = Instant::now();
    let mut i: usize = 0;
    let mut err = io::stderr().lock();
    let readable = width_readable();
    let mut width = Width::new();

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let frame = FRAMES[i % FRAMES.len()];
        let elapsed = started.elapsed().as_secs();
        // \x1b[2K = ANSI "erase entire line", \r = return to col 0.
        // We rewrite the same line each tick so the spinner doesn't scroll.
        // A line wider than the terminal wraps, and `\r` then only returns
        // to the start of its last row: every frame would leave a new line.
        let cols = width.get(Instant::now(), || {
            if readable {
                cols_from(termimad::crossterm::terminal::size())
            } else {
                DEFAULT_COLS
            }
        });
        let line = frame_line(frame, &message, elapsed, cols);
        let _ = write!(err, "\r\x1b[2K{}", line);
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

#[cfg(test)]
mod tests {
    use super::*;

    const MESSAGE: &str = "waiting for ChatGPT's answer to turn 3 of the agent session";

    #[test]
    fn a_wide_terminal_gets_the_whole_line() {
        assert_eq!(
            frame_line("⠋", MESSAGE, 12, 120),
            format!("⠋ {} (12s)", MESSAGE)
        );
    }

    #[test]
    fn a_narrow_terminal_gets_a_line_that_does_not_wrap() {
        for width in [0, 1, 2, 3, 5, 8, 10, 20, 40, 64, 70] {
            for elapsed in [0, 9, 125, 99_999] {
                let line = frame_line("⠋", MESSAGE, elapsed, width);
                assert!(
                    cols(&line) < width.max(1),
                    "width {width}: {line:?} takes {} columns",
                    cols(&line)
                );
            }
        }
        let line = frame_line("⠋", MESSAGE, 12, 40);
        assert!(line.starts_with("⠋ waiting for"), "{line:?}");
        assert!(line.ends_with("… (12s)"), "{line:?}");
    }

    #[test]
    fn a_message_with_a_line_break_stays_on_one_line() {
        assert_eq!(
            frame_line("⠋", "first\nsecond", 1, 80),
            "⠋ first second (1s)"
        );
    }

    #[test]
    fn a_terminal_that_reports_zero_columns_counts_as_unknown() {
        assert_eq!(cols_from(Ok((0, 0))), DEFAULT_COLS);
        assert_eq!(cols_from(Err(io::Error::other("no tty"))), DEFAULT_COLS);
        assert_eq!(cols_from(Ok((40, 10))), 40);
        // The spinner still shows in a terminal that says 0x0.
        assert!(!frame_line("⠋", MESSAGE, 1, cols_from(Ok((0, 0)))).is_empty());
    }

    #[test]
    fn the_width_is_read_at_most_once_per_second() {
        let mut width = Width::new();
        let start = Instant::now();
        let mut reads = 0;
        for ms in [0, 80, 160, 999] {
            width.get(start + Duration::from_millis(ms), || {
                reads += 1;
                50
            });
        }
        assert_eq!(reads, 1);
        let cols = width.get(start + Duration::from_millis(1_000), || {
            reads += 1;
            30
        });
        assert_eq!((reads, cols), (2, 30));
    }

    #[test]
    fn non_ascii_text_is_counted_wide() {
        let line = frame_line("⠋", "ожидание ответа ChatGPT на ход агента", 3, 30);
        assert!(cols(&line) < 30, "{line:?}");
        assert!(line.ends_with("… (3s)"), "{line:?}");
    }
}
