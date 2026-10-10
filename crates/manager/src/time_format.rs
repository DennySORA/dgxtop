//! UTC display strings for audit records.
//!
//! Audit rows carry `*_utc` text next to the boot clock. UTC is display and
//! cross-boot indexing only (it may step), so a small dependency-free
//! formatter is enough; no duration is ever derived from these strings.

/// 0000-01-01T00:00:00Z in nanoseconds since the Unix epoch.
const MIN_RFC3339_NS: i128 = -62_167_219_200 * 1_000_000_000;
/// 9999-12-31T23:59:59.999Z in nanoseconds since the Unix epoch.
const MAX_RFC3339_NS: i128 = 253_402_300_799_999 * 1_000_000;

/// Formats Unix-epoch nanoseconds (negative before 1970) as RFC 3339 UTC
/// `YYYY-MM-DDTHH:MM:SS.mmmZ`, the form the audit repository accepts.
///
/// Floor division keeps instants before 1970 on the right calendar day.
/// Instants outside the four-digit years RFC 3339 can express are clamped
/// to its range: the wall clock is untrusted display data, never a duration.
pub(crate) fn format_utc(utc_ns: i128) -> String {
    let clamped = utc_ns.clamp(MIN_RFC3339_NS, MAX_RFC3339_NS);
    let total_ms = clamped.div_euclid(1_000_000);
    let millis = total_ms.rem_euclid(1000);
    let total_seconds = total_ms.div_euclid(1000);
    let days = total_seconds.div_euclid(86_400);
    let second_of_day = total_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3600;
    let minute = (second_of_day % 3600) / 60;
    let second = second_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Days since 1970-01-01 to a proleptic Gregorian date (H. Hinnant's
/// `civil_from_days`), computed in `i128` so no narrowing cast is needed.
fn civil_from_days(days: i128) -> (i128, i128, i128) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i128::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_instants() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00.000Z");
        // 2000-02-29T12:34:56.789Z (leap day).
        assert_eq!(
            format_utc(951_827_696_789_000_000),
            "2000-02-29T12:34:56.789Z"
        );
        // 2026-10-04T00:00:00Z.
        assert_eq!(
            format_utc(1_791_072_000_000_000_000),
            "2026-10-04T00:00:00.000Z"
        );
    }

    #[test]
    fn instants_outside_four_digit_years_are_clamped() {
        assert_eq!(format_utc(i128::MIN), "0000-01-01T00:00:00.000Z");
        assert_eq!(format_utc(i128::MAX), "9999-12-31T23:59:59.999Z");
    }
}
