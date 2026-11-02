//! The gateway's log: one line per event on stderr, which the launcher
//! redirects to `gateway.log`.

use std::fmt::Display;

/// Writes one log line: the UTC time to the second (`YYYY-MM-DDTHH:MM:SSZ`), a
/// space, then `msg`.
pub fn log(msg: impl Display) {
    eprintln!("{}", stamped(crate::session::now_ms() as u64, msg));
}

fn stamped(ms: u64, msg: impl Display) -> String {
    format!("{} {}", cgpt_bridge_core::time::format_utc_z(ms), msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_starts_with_the_utc_time_and_keeps_the_text_after_it() {
        // 2026-10-27 12:34:56 UTC.
        let line = stamped(
            1_793_104_496_789,
            "cgpt-gateway: listening on http://127.0.0.1:8765",
        );
        assert_eq!(
            line,
            "2026-10-27T12:34:56Z cgpt-gateway: listening on http://127.0.0.1:8765"
        );
    }

    #[test]
    fn the_time_is_taken_when_the_line_is_written() {
        let line = stamped(crate::session::now_ms() as u64, "x");
        let (stamp, rest) = line.split_once(' ').unwrap();
        assert_eq!(rest, "x");
        let b = stamp.as_bytes();
        assert_eq!(b.len(), 20, "{stamp}");
        assert!(b[..4].iter().all(u8::is_ascii_digit) && b[4] == b'-' && b[10] == b'T');
        assert_eq!(b[19], b'Z');
    }
}
