//! Line formatting shared by the loggers.

use dgxtop_core::ports::LogLevel;

const NANOS_PER_SECOND: i128 = 1_000_000_000;
const SECONDS_PER_DAY: i64 = 86_400;

/// Upper-case level name used in log lines.
pub const fn level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "ERROR",
        LogLevel::Warn => "WARN",
        LogLevel::Info => "INFO",
        LogLevel::Debug => "DEBUG",
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` for nanoseconds since the Unix epoch.
pub fn format_utc(utc_ns: i128) -> String {
    let seconds = utc_ns.div_euclid(NANOS_PER_SECOND);
    let millis = utc_ns.rem_euclid(NANOS_PER_SECOND) / 1_000_000;
    let Ok(seconds) = i64::try_from(seconds) else {
        return "invalid-utc".to_owned();
    };
    let days = seconds.div_euclid(SECONDS_PER_DAY);
    let second_of_day = seconds.rem_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        second_of_day / 3600,
        (second_of_day % 3600) / 60,
        second_of_day % 60
    )
}

/// Proleptic Gregorian date of a day count since 1970-01-01 (H. Hinnant's
/// `civil_from_days`), exact for every representable day.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days.saturating_add(719_468);
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
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
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting_matches_reference_dates() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_utc(1_790_000_000_123_000_000),
            "2026-09-21T14:13:20.123Z"
        );
        assert_eq!(
            format_utc(951_782_400_000_000_000),
            "2000-02-29T00:00:00.000Z"
        );
        assert_eq!(format_utc(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(format_utc(i128::MAX), "invalid-utc");
    }
}
