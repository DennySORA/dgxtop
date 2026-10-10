//! UTC text used for SQLite `*_utc` columns and retention cutoffs.
//!
//! UTC is only an index and a display value (`STORAGE.md` §3); it is never
//! used for durations inside a boot. Stored UTC must be RFC 3339 with a `Z`
//! suffix so that lexical comparison orders it like time, which retention
//! relies on.

use dgxtop_core::clock::NANOS_PER_SECOND;

const SECONDS_PER_DAY: i64 = 86_400;

/// `YYYY-MM-DDTHH:MM:SSZ`, clamped to years 0000..=9999 so the text keeps a
/// fixed width and its lexical order. Shared with the Manager through core so
/// the audit validates exactly the format the Manager produces.
pub fn format_utc_rfc3339(unix_ns: i128) -> String {
    dgxtop_core::clock::format_utc_rfc3339(unix_ns)
}

/// UTC text `days` before `now_unix_ns` (retention cutoff).
pub fn utc_cutoff(now_unix_ns: i128, days: u32) -> String {
    let span = i128::from(days) * i128::from(SECONDS_PER_DAY) * i128::from(NANOS_PER_SECOND);
    format_utc_rfc3339(now_unix_ns.saturating_sub(span))
}

/// `YYYY-MM-DDTHH:MM:SS[.f{1,9}]Z` with plausible field ranges.
pub fn is_rfc3339_utc(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes.len() > 30 {
        return false;
    }
    let separators = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')];
    let digits = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
    let shape_ok = separators
        .iter()
        .all(|(index, expected)| bytes.get(*index) == Some(expected))
        && digits
            .iter()
            .all(|index| bytes.get(*index).is_some_and(u8::is_ascii_digit));
    shape_ok && fields_in_range(bytes) && fraction_and_zone_ok(bytes)
}

/// Nanoseconds since the Unix epoch of RFC 3339 UTC text as the archive
/// stores it (`2026-10-04T12:00:00.123456789Z`, a fraction of 0 to 9
/// digits); `None` for any other text. The history view places archived
/// minutes of every boot on the wall clock with it (D-51).
pub fn parse_utc_rfc3339_ns(text: &str) -> Option<i128> {
    if !is_rfc3339_utc(text) {
        return None;
    }
    let bytes = text.as_bytes();
    let year = i64::from(two_digits(bytes, 0) * 100 + two_digits(bytes, 2));
    let days = days_from_civil(
        year,
        i64::from(two_digits(bytes, 5)),
        i64::from(two_digits(bytes, 8)),
    );
    let seconds = days * SECONDS_PER_DAY
        + i64::from(two_digits(bytes, 11)) * 3600
        + i64::from(two_digits(bytes, 14)) * 60
        + i64::from(two_digits(bytes, 17));
    Some(i128::from(seconds) * i128::from(NANOS_PER_SECOND) + i128::from(fraction_ns(bytes)))
}

/// The fraction after the seconds in nanoseconds (validated text).
fn fraction_ns(bytes: &[u8]) -> u32 {
    let digits = bytes
        .get(20..bytes.len().saturating_sub(1))
        .unwrap_or_default();
    let value = digits
        .iter()
        .fold(0u32, |value, digit| value * 10 + u32::from(digit - b'0'));
    let scale = (digits.len()..9).fold(1u32, |scale, _| scale * 10);
    value * scale
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn two_digits(bytes: &[u8], index: usize) -> u32 {
    let tens = bytes.get(index).map_or(0, |byte| u32::from(byte - b'0'));
    let ones = bytes
        .get(index + 1)
        .map_or(0, |byte| u32::from(byte - b'0'));
    tens * 10 + ones
}

fn fields_in_range(bytes: &[u8]) -> bool {
    (1..=12).contains(&two_digits(bytes, 5))
        && (1..=31).contains(&two_digits(bytes, 8))
        && two_digits(bytes, 11) <= 23
        && two_digits(bytes, 14) <= 59
        && two_digits(bytes, 17) <= 60
}

fn fraction_and_zone_ok(bytes: &[u8]) -> bool {
    match bytes.get(19..) {
        Some([b'Z']) => true,
        Some([b'.', rest @ ..]) => match rest.split_last() {
            Some((b'Z', fraction)) => {
                (1..=9).contains(&fraction.len()) && fraction.iter().all(u8::is_ascii_digit)
            }
            _ => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: i128 = 1_000_000_000;

    #[test]
    fn formats_known_instants() {
        assert_eq!(format_utc_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_utc_rfc3339(1_790_000_000 * SECOND),
            "2026-09-21T14:13:20Z"
        );
        assert_eq!(
            format_utc_rfc3339(951_782_400 * SECOND),
            "2000-02-29T00:00:00Z"
        );
        assert_eq!(format_utc_rfc3339(-SECOND), "1969-12-31T23:59:59Z");
        assert_eq!(format_utc_rfc3339(i128::MIN), "0000-01-01T00:00:00Z");
        assert_eq!(format_utc_rfc3339(i128::MAX), "9999-12-31T23:59:59Z");
    }

    #[test]
    fn cutoff_subtracts_whole_days() {
        assert_eq!(
            utc_cutoff(1_790_000_000 * SECOND, 30),
            "2026-08-22T14:13:20Z"
        );
        assert_eq!(utc_cutoff(0, 1), "1969-12-31T00:00:00Z");
    }

    #[test]
    fn accepts_only_zulu_rfc3339() {
        assert!(is_rfc3339_utc("2026-10-04T12:00:00Z"));
        assert!(is_rfc3339_utc("2026-10-04T12:00:00.5Z"));
        assert!(is_rfc3339_utc("2026-10-04T12:00:00.123456789Z"));
        assert!(!is_rfc3339_utc("2026-10-04T12:00:00.1234567890Z"));
        assert!(!is_rfc3339_utc("2026-10-04T12:00:00+00:00"));
        assert!(!is_rfc3339_utc("2026-10-04 12:00:00Z"));
        assert!(!is_rfc3339_utc("2026-13-04T12:00:00Z"));
        assert!(!is_rfc3339_utc("2026-10-04T24:00:00Z"));
        assert!(!is_rfc3339_utc("2026-10-04T12:00:00.Z"));
        assert!(!is_rfc3339_utc(""));
        assert!(!is_rfc3339_utc("2026-10-04T12:00:00Zjunk"));
    }

    #[test]
    fn parses_what_the_archive_stores_back_to_the_instant() {
        let stored = [
            ("1970-01-01T00:00:00.000000000Z", 0),
            (
                "2026-09-21T14:13:20.123456789Z",
                1_790_000_000 * SECOND + 123_456_789,
            ),
            ("2000-02-29T00:00:00Z", 951_782_400 * SECOND),
            (
                "2026-10-04T12:00:00.5Z",
                1_791_115_200 * SECOND + 500_000_000,
            ),
            ("1969-12-31T23:59:59.999999999Z", -1),
        ];
        for (text, instant) in stored {
            assert_eq!(parse_utc_rfc3339_ns(text), Some(instant), "{text}");
        }
        for instant in [
            0,
            59 * SECOND,
            1_790_000_000 * SECOND,
            4_102_444_800 * SECOND,
        ] {
            let text = format_utc_rfc3339(instant);
            assert_eq!(parse_utc_rfc3339_ns(&text), Some(instant), "{text}");
        }
        assert_eq!(parse_utc_rfc3339_ns("2026-10-04T12:00:00+00:00"), None);
        assert_eq!(parse_utc_rfc3339_ns("not a time"), None);
    }

    #[test]
    fn formatted_text_sorts_like_time() {
        let instants = [
            -5 * SECOND,
            0,
            59 * SECOND,
            86_400 * SECOND,
            1_790_000_000 * SECOND,
        ];
        let texts: Vec<String> = instants.iter().map(|ns| format_utc_rfc3339(*ns)).collect();
        let mut sorted = texts.clone();
        sorted.sort();
        assert_eq!(sorted, texts);
        assert!(texts.iter().all(|text| is_rfc3339_utc(text)));
    }
}
