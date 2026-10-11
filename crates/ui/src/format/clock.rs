//! Wall-clock times in local time (D-76), from UTC and the offset the host
//! had at startup.

use dgxtop_core::clock::civil_from_days;

const NANOS_PER_SECOND: i64 = 1_000_000_000;
const SECONDS_PER_DAY: i64 = 86_400;

/// `2026-10-11 14:03:22`.
pub fn format_local_datetime(utc_ns: i64, offset_s: i32) -> String {
    let (date, time) = parts(utc_ns, offset_s);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        date.0, date.1, date.2, time.0, time.1, time.2
    )
}

/// `10-11 14:03`.
pub fn format_local_short(utc_ns: i64, offset_s: i32) -> String {
    let (date, time) = parts(utc_ns, offset_s);
    format!("{:02}-{:02} {:02}:{:02}", date.1, date.2, time.0, time.1)
}

fn parts(utc_ns: i64, offset_s: i32) -> ((i64, i64, i64), (i64, i64, i64)) {
    let seconds = utc_ns
        .div_euclid(NANOS_PER_SECOND)
        .saturating_add(i64::from(offset_s));
    let days = seconds.div_euclid(SECONDS_PER_DAY);
    let second_of_day = seconds.rem_euclid(SECONDS_PER_DAY);
    (
        civil_from_days(days),
        (
            second_of_day / 3600,
            (second_of_day / 60) % 60,
            second_of_day % 60,
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-04T00:00:00Z.
    const SUNDAY_UTC: i64 = 1_791_072_000_000_000_000;

    #[test]
    fn times_read_in_local_time() {
        assert_eq!(format_local_datetime(SUNDAY_UTC, 0), "2026-10-04 00:00:00");
        assert_eq!(
            format_local_datetime(SUNDAY_UTC, 8 * 3600),
            "2026-10-04 08:00:00"
        );
        assert_eq!(
            format_local_datetime(SUNDAY_UTC, -3600 - 1800),
            "2026-10-03 22:30:00",
            "west of UTC the day before"
        );
        assert_eq!(
            format_local_short(SUNDAY_UTC + 61 * NANOS_PER_SECOND, 0),
            "10-04 00:01"
        );
        assert_eq!(format_local_datetime(-1, 0), "1969-12-31 23:59:59");
    }
}
