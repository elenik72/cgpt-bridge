//! Ctrl+C while an ask waits for its answer (C1). The first press is only
//! noted, so the caller can stop the answer in ChatGPT and then end the
//! process with [`die`]; a second press ends it at once. Either way the
//! process ends as killed by Ctrl+C, so a shell loop around `cgpt` stops too.
//! Armed for that wait alone: before it (the prompt not yet written to the
//! host) and after it (say, a command of `cgpt agent` running) Ctrl+C does
//! what it always did.
//!
//! No crate: the C library's `signal`/`raise` (Unix) and kernel32's
//! `SetConsoleCtrlHandler` (Windows), both linked by std already.

use std::sync::atomic::{AtomicU8, Ordering};

/// Presses since the last `arm`. Touched from the signal handler, so only
/// atomics here.
static PRESSES: AtomicU8 = AtomicU8::new(0);

/// Ctrl+C is caught while this lives; dropping it puts back what was there.
pub struct Armed {
    installed: bool,
}

impl Armed {
    /// Catch Ctrl+C. When that cannot be done (Ctrl+C ignored from the
    /// start, or the call fails) Ctrl+C keeps doing what it did, and
    /// `pressed` stays false.
    pub fn arm() -> Armed {
        PRESSES.store(0, Ordering::SeqCst);
        Armed {
            installed: os::install(),
        }
    }

    /// Whether Ctrl+C was pressed since `arm`.
    pub fn pressed(&self) -> bool {
        PRESSES.load(Ordering::SeqCst) > 0
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        if self.installed {
            os::restore();
        }
    }
}

/// End the process as killed by Ctrl+C: SIGINT with its default action on
/// Unix (shells show 130), `STATUS_CONTROL_C_EXIT` on Windows. Called after a
/// first press was handled.
pub fn die() -> ! {
    os::die()
}

/// Called from the handler. The second press ends the process: on Unix
/// SIGINT, held while the handler runs, does so as the handler returns.
fn press() {
    if PRESSES.fetch_add(1, Ordering::SeqCst) >= 1 {
        os::reraise();
    }
}

#[cfg(unix)]
mod os {
    use std::os::raw::c_int;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SIGINT: c_int = 2;
    const SIG_DFL: usize = 0;
    const SIG_IGN: usize = 1;
    const SIG_ERR: usize = usize::MAX;

    extern "C" {
        // glibc, musl and the BSDs (macOS) give `signal` BSD semantics: the
        // handler stays installed, SIGINT is held while it runs, and calls it
        // interrupts are restarted.
        fn signal(sig: c_int, handler: usize) -> usize;
        fn raise(sig: c_int) -> c_int;
    }

    /// The disposition `install` replaced.
    static PREVIOUS: AtomicUsize = AtomicUsize::new(SIG_DFL);

    extern "C" fn on_sigint(_: c_int) {
        super::press();
    }

    pub fn install() -> bool {
        // SAFETY (all calls here): `signal` with SIG_IGN, a saved
        // disposition or `on_sigint`, which only touches atomics and may
        // call `reraise` — `signal` and `raise`, both async-signal-safe.
        // Look first, with Ctrl+C ignored for that moment rather than
        // caught: a job a non-interactive shell started with `&` has SIGINT
        // ignored, and keeps it so.
        let previous = unsafe { signal(SIGINT, SIG_IGN) };
        if previous == SIG_ERR || previous == SIG_IGN {
            return false;
        }
        PREVIOUS.store(previous, Ordering::SeqCst);
        let handler = on_sigint as extern "C" fn(c_int) as usize;
        if unsafe { signal(SIGINT, handler) } == SIG_ERR {
            unsafe { signal(SIGINT, previous) };
            return false;
        }
        true
    }

    pub fn restore() {
        // SAFETY: puts back the disposition `install` found.
        unsafe { signal(SIGINT, PREVIOUS.load(Ordering::SeqCst)) };
    }

    /// SIGINT with its default action. Outside the handler it ends the
    /// process right here; inside, as the handler returns.
    pub fn reraise() {
        // SAFETY: both async-signal-safe.
        unsafe {
            signal(SIGINT, SIG_DFL);
            raise(SIGINT);
        }
    }

    pub fn die() -> ! {
        reraise();
        // Only when SIGINT is blocked (a mask inherited from the caller).
        std::process::exit(130)
    }
}

#[cfg(windows)]
mod os {
    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    /// What a console process ended by Ctrl+C exits with.
    const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;

    type Handler = unsafe extern "system" fn(u32) -> i32;

    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<Handler>, add: i32) -> i32;
        fn ExitProcess(code: u32) -> !;
    }

    /// Runs on a thread of its own that the console starts.
    unsafe extern "system" fn on_ctrl(kind: u32) -> i32 {
        if kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT {
            super::press();
            1
        } else {
            0
        }
    }

    pub fn install() -> bool {
        // SAFETY: registers a handler with the signature the call takes.
        unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) != 0 }
    }

    pub fn restore() {
        // SAFETY: removes the handler `install` added. Should that fail, the
        // handler stays: a later Ctrl+C is then only counted, and the second
        // ends the process as before.
        unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 0) };
    }

    pub fn reraise() {
        die()
    }

    pub fn die() -> ! {
        // SAFETY: ends the process, as the console's own Ctrl+C would.
        unsafe { ExitProcess(STATUS_CONTROL_C_EXIT) }
    }
}

/// Nowhere to catch Ctrl+C: it does what it always did.
#[cfg(not(any(unix, windows)))]
mod os {
    pub fn install() -> bool {
        false
    }
    pub fn restore() {}
    pub fn reraise() {
        die()
    }
    pub fn die() -> ! {
        std::process::exit(130)
    }
}
