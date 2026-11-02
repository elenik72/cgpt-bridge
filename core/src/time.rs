//! UTC date and time from a Unix timestamp, without a date-time dependency.

/// `YYYY-MM-DDTHH:MM:SSZ` (UTC, to the second) for `ms` milliseconds since the
/// Unix epoch.
pub fn format_utc_z(ms: u64) -> String {
    let (y, mo, d, h, mi, s) = ymdhms_utc(ms / 1000);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
}

/// The last second `ymdhms_utc` converts: 9999-12-31T23:59:59Z.
const MAX_SECS: u64 = 253_402_300_799;

/// Convert a POSIX timestamp to UTC (Y, M, D, h, m, s). Gregorian leap years
/// (2000 is one, 2100 is not); no leap-second handling. A time after the year
/// 9999 is cut to its last second, so the year loop stays short for any input.
/// Cheaper than a chrono dependency for one display string.
pub fn ymdhms_utc(t: u64) -> (i32, u32, u32, u32, u32, u32) {
    let t = t.min(MAX_SECS) as i64;
    let sec = (t % 60) as u32;
    let t = t / 60;
    let min = (t % 60) as u32;
    let t = t / 60;
    let hour = (t % 24) as u32;
    let mut days = t / 24;

    let mut year: i32 = 1970;
    loop {
        let dy = if is_leap(year) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        year += 1;
    }
    let months_normal = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let months_leap = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mons = if is_leap(year) {
        months_leap
    } else {
        months_normal
    };
    let mut month = 1u32;
    let mut d = days;
    for &m in &mons {
        if d < m {
            break;
        }
        d -= m;
        month += 1;
    }
    let day = (d as u32) + 1;
    (year, month, day, hour, min, sec)
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_and_a_known_instant_format_as_utc_with_a_z() {
        assert_eq!(format_utc_z(0), "1970-01-01T00:00:00Z");
        // 2026-10-27 12:34:56 UTC, the instant cli/src/plan.rs pins for format_ts.
        assert_eq!(format_utc_z(1_793_104_496_789), "2026-10-27T12:34:56Z");
    }

    #[test]
    fn a_time_past_the_year_9999_is_cut_to_its_last_second_and_does_not_hang() {
        assert_eq!(ymdhms_utc(u64::MAX), (9999, 12, 31, 23, 59, 59));
        assert_eq!(format_utc_z(u64::MAX), "9999-12-31T23:59:59Z");
        assert_eq!(ymdhms_utc(MAX_SECS), (9999, 12, 31, 23, 59, 59));
        assert_eq!(ymdhms_utc(MAX_SECS + 1), (9999, 12, 31, 23, 59, 59));
    }

    #[test]
    fn a_century_that_is_not_a_leap_year_has_no_29_february() {
        // 2100-03-01T00:00:00Z.
        assert_eq!(format_utc_z(4_107_542_400_000), "2100-03-01T00:00:00Z");
    }

    #[test]
    fn a_leap_day_and_a_year_end_are_right() {
        assert_eq!(format_utc_z(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc_z(1_735_689_599_000), "2024-12-31T23:59:59Z");
    }
}
