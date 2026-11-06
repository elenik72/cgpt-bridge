//! Pauses after ChatGPT's rate limit: 5 minutes, 15 when it comes again
//! within an hour of the last one, then 30. While a pause lasts, nothing is
//! sent to ChatGPT. Kept in `<data_dir>/cgpt-gateway/cooldown.json`, so a
//! restart does not cut a pause short.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::backend::BackendError;
use crate::log::log;

const MINUTE_MS: u128 = 60 * 1000;
/// The pause for the first hit and for each repeat, in minutes.
const PAUSES_MIN: [u128; 3] = [5, 15, 30];
/// A hit within this long of the previous one is a repeat.
const REPEAT_WINDOW_MS: u128 = 60 * MINUTE_MS;
const LONGEST_PAUSE_MS: u128 = PAUSES_MIN[PAUSES_MIN.len() - 1] * MINUTE_MS;

/// Where the gateway keeps its pause. Clones share it: the engine and the
/// HTTP side hold one each.
#[derive(Debug, Clone)]
pub struct Cooldown {
    path: PathBuf,
    /// The last hit of this process, so a pause holds even when its file
    /// cannot be written.
    memory: Arc<Mutex<Option<State>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct State {
    last_hit_ms: u128,
    /// 1 for the first hit, 2 for a repeat, 3 for every one after.
    level: usize,
    until_ms: u128,
}

impl Cooldown {
    pub fn new(path: PathBuf) -> Cooldown {
        Cooldown {
            path,
            memory: Arc::new(Mutex::new(None)),
        }
    }

    /// ChatGPT limited the rate at `now_ms`: start the next pause; returns
    /// when it ends. A hit while a pause is running (asks of several
    /// sessions that were out when the first hit came) is the same limit:
    /// it neither raises the level nor moves the end, and returns the
    /// running pause's end. The pause holds in memory when the file cannot
    /// be written; a file that cannot be read counts as no earlier hit.
    pub fn record_hit(&self, now_ms: u128) -> u128 {
        let mut memory = self.memory.lock().unwrap_or_else(|e| e.into_inner());
        let file = self.load(now_ms).ok().flatten();
        let last = [file, memory.clone()]
            .into_iter()
            .flatten()
            .max_by_key(|s| s.last_hit_ms);
        if let Some(running) = last.as_ref().filter(|s| s.until_ms > now_ms) {
            return running.until_ms;
        }
        let level = match last {
            Some(last) if now_ms.saturating_sub(last.last_hit_ms) <= REPEAT_WINDOW_MS => {
                last.level.saturating_add(1).clamp(1, PAUSES_MIN.len())
            }
            _ => 1,
        };
        let state = State {
            last_hit_ms: now_ms,
            level,
            until_ms: now_ms + PAUSES_MIN[level - 1] * MINUTE_MS,
        };
        if let Err(e) = self.save(&state) {
            log(format_args!(
                "cgpt-gateway: {} not saved, the pause is kept in memory: {}",
                self.path.display(),
                e
            ));
        }
        let until = state.until_ms;
        *memory = Some(state);
        until
    }

    /// When the current pause ends, or `None` when there is none: the later
    /// of the file's and this process's.
    pub fn active_until(&self, now_ms: u128) -> io::Result<Option<u128>> {
        let memory = self
            .memory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| s.until_ms);
        let file = match self.load(now_ms) {
            Ok(state) => state.map(|s| s.until_ms),
            Err(e) if !memory.is_some_and(|m| m > now_ms) => return Err(e),
            Err(_) => None,
        };
        Ok(memory.max(file).filter(|&until| until > now_ms))
    }

    /// `Err(CoolingDown)` during a pause. A pause file that cannot be read
    /// does not stop the gateway.
    pub fn check(&self, now_ms: u128) -> Result<(), BackendError> {
        match self.active_until(now_ms) {
            Ok(Some(until_ms)) => Err(BackendError::CoolingDown { until_ms }),
            Ok(None) => Ok(()),
            Err(e) => {
                log(format_args!(
                    "cgpt-gateway: {} not read: {}",
                    self.path.display(),
                    e
                ));
                Ok(())
            }
        }
    }

    /// The saved state. A file that does not parse is removed, so it is
    /// reported once and not on every request. A pause that ends more than
    /// the longest pause from `now_ms` (a clock that ran ahead, a hand-edited
    /// file) is cut to the longest pause from now, and the cut is saved so
    /// that the end does not move along with the clock.
    fn load(&self, now_ms: u128) -> io::Result<Option<State>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        match serde_json::from_str::<State>(&text) {
            Ok(mut state) => {
                let latest = now_ms + LONGEST_PAUSE_MS;
                if state.until_ms > latest || state.last_hit_ms > now_ms {
                    state.until_ms = state.until_ms.min(latest);
                    state.last_hit_ms = state.last_hit_ms.min(now_ms);
                    if let Err(e) = self.save(&state) {
                        log(format_args!(
                            "cgpt-gateway: {} not saved: {}",
                            self.path.display(),
                            e
                        ));
                    }
                }
                Ok(Some(state))
            }
            Err(e) => {
                log(format_args!(
                    "cgpt-gateway: {} is broken, removed: {}",
                    self.path.display(),
                    e
                ));
                let _ = std::fs::remove_file(&self.path);
                Ok(None)
            }
        }
    }

    /// Write the file whole (a temporary file renamed over it): requests
    /// read it while a turn writes.
    fn save(&self, state: &State) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(state)?)?;
        std::fs::rename(&tmp, &self.path)
    }
}

/// `ЧЧ:ММ` of `ms` in local time, rounded up to the minute: a retry at that
/// minute is after the pause. `ЧЧ:ММ UTC` when local time is unknown.
pub fn clock(ms: u128) -> String {
    let secs = ms.div_ceil(MINUTE_MS) as i64 * 60;
    format_clock(secs, local_hour_minute(secs))
}

fn format_clock(secs: i64, local: Option<(u16, u16)>) -> String {
    match local {
        Some((hour, minute)) => format!("{:02}:{:02}", hour, minute),
        None => format!(
            "{:02}:{:02} UTC",
            (secs / 3600).rem_euclid(24),
            (secs / 60).rem_euclid(60)
        ),
    }
}

/// Hour and minute of local time at `secs` since the Unix epoch, from the C
/// library. Only where `time_t` and `long` are 64 bit, as declared below.
#[cfg(all(unix, target_pointer_width = "64"))]
fn local_hour_minute(secs: i64) -> Option<(u16, u16)> {
    use std::os::raw::{c_char, c_int, c_long};
    // `struct tm` of glibc, musl and the BSDs (macOS): `tm_sec`, `tm_min`,
    // `tm_hour` and six more ints, then `tm_gmtoff` and `tm_zone`.
    #[repr(C)]
    struct Tm {
        ints: [c_int; 9],
        gmtoff: c_long,
        zone: *const c_char,
    }
    extern "C" {
        fn tzset();
        fn localtime_r(time: *const i64, out: *mut Tm) -> *mut Tm;
    }
    let mut tm = Tm {
        ints: [0; 9],
        gmtoff: 0,
        zone: std::ptr::null(),
    };
    // SAFETY: both pointers are valid for the call; `localtime_r` writes
    // only into `tm`. `tzset` first: `localtime_r` need not re-read the
    // time zone, which may have changed since the gateway started.
    let ok = unsafe {
        tzset();
        !localtime_r(&secs, &mut tm).is_null()
    };
    ok.then(|| (tm.ints[2] as u16, tm.ints[1] as u16))
}

/// Hour and minute of local time at `secs` since the Unix epoch, by
/// Windows' rules for the current time zone.
#[cfg(windows)]
fn local_hour_minute(secs: i64) -> Option<(u16, u16)> {
    #[repr(C)]
    #[derive(Default)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn FileTimeToSystemTime(file_time: *const u64, out: *mut SystemTime) -> i32;
        fn SystemTimeToTzSpecificLocalTime(
            zone: *const std::ffi::c_void,
            utc: *const SystemTime,
            local: *mut SystemTime,
        ) -> i32;
    }
    /// 100 ns ticks from 1601-01-01 to 1970-01-01.
    const EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    let ticks = EPOCH_TICKS + secs.max(0) as u64 * 10_000_000;
    let (mut utc, mut local) = (SystemTime::default(), SystemTime::default());
    // SAFETY: all pointers are valid for the calls; a null zone means the
    // current time zone.
    let ok = unsafe {
        FileTimeToSystemTime(&ticks, &mut utc) != 0
            && SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) != 0
    };
    ok.then_some((local.hour, local.minute))
}

/// No known way to local time here: the clock says UTC.
#[cfg(not(any(all(unix, target_pointer_width = "64"), windows)))]
fn local_hour_minute(_secs: i64) -> Option<(u16, u16)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::temp_dir;

    const MIN: u128 = 60 * 1000;
    const T0: u128 = 1_800_000_000_000;

    fn cooldown(tag: &str) -> Cooldown {
        Cooldown::new(temp_dir(tag).join("cooldown.json"))
    }

    #[test]
    fn cooldown_5_15_30_within_hour_and_reset_after() {
        let c = cooldown("cd-steps");
        assert_eq!(c.active_until(T0).unwrap(), None, "no file yet");

        assert_eq!(c.record_hit(T0), T0 + 5 * MIN);
        assert_eq!(c.active_until(T0 + MIN).unwrap(), Some(T0 + 5 * MIN));
        assert_eq!(c.active_until(T0 + 5 * MIN).unwrap(), None, "over");

        // Again within an hour of the last hit: 15, then 30, then 30.
        assert_eq!(c.record_hit(T0 + 10 * MIN), T0 + 25 * MIN);
        assert_eq!(c.record_hit(T0 + 40 * MIN), T0 + 70 * MIN);
        assert_eq!(c.record_hit(T0 + 95 * MIN), T0 + 125 * MIN);
        assert_eq!(
            c.active_until(T0 + 100 * MIN).unwrap(),
            Some(T0 + 125 * MIN)
        );

        // More than an hour after the last hit: back to 5.
        let later = T0 + 95 * MIN + 61 * MIN;
        assert_eq!(c.record_hit(later), later + 5 * MIN);
        assert_eq!(c.record_hit(later + 6 * MIN), later + 21 * MIN);
    }

    #[test]
    fn a_hit_during_a_running_pause_neither_raises_nor_extends_it() {
        let c = cooldown("cd-during");
        assert_eq!(c.record_hit(T0), T0 + 5 * MIN);
        // Two more sessions' asks were out and hit the limit too.
        assert_eq!(c.record_hit(T0 + 1), T0 + 5 * MIN);
        assert_eq!(c.record_hit(T0 + 2 * MIN), T0 + 5 * MIN);
        assert_eq!(c.active_until(T0 + 3 * MIN).unwrap(), Some(T0 + 5 * MIN));
        // The next hit after the pause is the first repeat: 15, not 30.
        assert_eq!(c.record_hit(T0 + 10 * MIN), T0 + 25 * MIN);
    }

    #[test]
    fn pause_survives_a_restart_and_a_broken_file_does_not_block() {
        let c = cooldown("cd-file");
        c.record_hit(T0);
        let again = Cooldown::new(c.path.clone());
        assert_eq!(again.active_until(T0 + MIN).unwrap(), Some(T0 + 5 * MIN));
        assert!(matches!(
            again.check(T0 + MIN),
            Err(BackendError::CoolingDown { until_ms }) if until_ms == T0 + 5 * MIN
        ));
        assert_eq!(again.check(T0 + 5 * MIN), Ok(()));

        // A broken file is no pause, and it is removed (logged once).
        std::fs::write(&c.path, "{ broken").unwrap();
        let fresh = Cooldown::new(c.path.clone());
        assert_eq!(fresh.active_until(T0).unwrap(), None);
        assert!(!c.path.exists(), "the broken file is removed");
        assert_eq!(fresh.check(T0), Ok(()));
        assert_eq!(fresh.record_hit(T0), T0 + 5 * MIN, "starts over");
    }

    #[test]
    fn pause_held_in_memory_when_the_file_cannot_be_written() {
        let dir = temp_dir("cd-ro");
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "a file, not a directory").unwrap();
        let c = Cooldown::new(blocker.join("cooldown.json"));
        assert_eq!(c.record_hit(T0), T0 + 5 * MIN);
        // Shared by clones (the engine and the HTTP side hold one each).
        let other = c.clone();
        assert_eq!(other.active_until(T0 + MIN).unwrap(), Some(T0 + 5 * MIN));
        assert!(other.check(T0 + MIN).is_err());
        assert_eq!(c.record_hit(T0 + 10 * MIN), T0 + 25 * MIN, "still a repeat");
    }

    #[test]
    fn memory_and_file_the_later_pause_wins() {
        let c = cooldown("cd-max");
        c.record_hit(T0);
        // Another gateway process wrote a longer pause to the file.
        let state = State {
            last_hit_ms: T0 + MIN,
            level: 3,
            until_ms: T0 + 31 * MIN,
        };
        std::fs::write(&c.path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(c.active_until(T0 + 2 * MIN).unwrap(), Some(T0 + 31 * MIN));
    }

    #[test]
    fn a_pause_read_from_the_file_ends_within_30_minutes() {
        let c = cooldown("cd-far");
        // A clock that ran ahead, or a hand-edited file: ten days.
        let state = State {
            last_hit_ms: T0 + 10 * 24 * 60 * MIN,
            level: 3,
            until_ms: T0 + 10 * 24 * 60 * MIN,
        };
        std::fs::create_dir_all(c.path.parent().unwrap()).unwrap();
        std::fs::write(&c.path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(c.active_until(T0).unwrap(), Some(T0 + 30 * MIN));
        assert!(matches!(
            c.check(T0 + MIN),
            Err(BackendError::CoolingDown { until_ms }) if until_ms == T0 + 30 * MIN
        ));
        // The cut is kept: the pause does not move along with the clock.
        assert_eq!(c.check(T0 + 30 * MIN), Ok(()));
        let fresh = Cooldown::new(c.path.clone());
        assert_eq!(fresh.check(T0 + 31 * MIN), Ok(()));
        // The hit counts as made when it was read, not ten days ahead: two
        // hours later the next one is a first hit again.
        assert_eq!(fresh.record_hit(T0 + 120 * MIN), T0 + 125 * MIN);
    }

    #[test]
    fn utc_fallback_is_labelled() {
        assert_eq!(format_clock(90 * 60, None), "01:30 UTC");
        assert_eq!(format_clock(90 * 60, Some((4, 30))), "04:30");
    }

    #[test]
    fn clock_is_hh_mm_rounded_up_to_the_minute() {
        let on_minute = T0 - T0 % MIN;
        let a = clock(on_minute);
        assert_eq!(a.len(), 5, "{}", a);
        assert_eq!(a.as_bytes()[2], b':', "{}", a);
        assert!(a[..2].parse::<u32>().unwrap() < 24 && a[3..].parse::<u32>().unwrap() < 60);
        assert_eq!(clock(on_minute + 1), clock(on_minute + MIN));
        assert_ne!(clock(on_minute), clock(on_minute + 1));
    }

    #[cfg(windows)]
    #[test]
    fn clock_is_local_time_on_windows() {
        let before = crate::session::now_ms();
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", "(Get-Date).ToString('HH:mm')"])
            .output()
            .unwrap();
        let after = crate::session::now_ms();
        let local = String::from_utf8(out.stdout).unwrap().trim().to_string();
        // Either side of a minute boundary the command may have crossed.
        let expected = [before, after].map(|t| clock(t - t % MIN));
        assert!(expected.contains(&local), "{} not in {:?}", local, expected);
    }

    #[cfg(all(unix, target_pointer_width = "64"))]
    #[test]
    fn clock_is_local_time() {
        let now = crate::session::now_ms();
        let on_minute = now - now % MIN;
        let secs = (on_minute / 1000).to_string();
        // BSD date (macOS) takes `-r <secs>`, GNU date `-d @<secs>`.
        let date = |args: &[&str]| std::process::Command::new("date").args(args).output();
        let out = date(&["-r", &secs, "+%H:%M"])
            .ok()
            .filter(|o| o.status.success())
            .unwrap_or_else(|| date(&["-d", &format!("@{}", secs), "+%H:%M"]).unwrap());
        let local = String::from_utf8(out.stdout).unwrap().trim().to_string();
        assert_eq!(clock(on_minute), local);
    }
}
