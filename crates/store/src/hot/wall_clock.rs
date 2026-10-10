//! Boot clock to UTC mapping, for archive rows and clock discontinuities.
//!
//! UTC is only an index for display and archive rows; it never produces a
//! duration. A step of the wall clock relative to the boot clock starts a new
//! clock segment and is reported as a discontinuity.

use std::collections::VecDeque;

/// Offset change treated as a wall clock step rather than slewing.
const STEP_TOLERANCE_NS: u128 = 1_000_000_000;
/// Clock segments remembered.
const MAX_ANCHORS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Anchor {
    boot_ns: u64,
    /// `utc_ns - boot_ns` while this segment lasts.
    offset_ns: i128,
}

/// Bounded list of clock segments.
#[derive(Debug, Default)]
pub(crate) struct WallClock {
    anchors: VecDeque<Anchor>,
}

impl WallClock {
    /// Records one `(boot, utc)` observation; returns the boot time of a
    /// discontinuity when the wall clock stepped.
    pub(crate) fn observe(&mut self, boot_ns: u64, utc_ns: i128) -> Option<u64> {
        let offset_ns = utc_ns.saturating_sub(i128::from(boot_ns));
        let Some(current) = self.anchors.back_mut() else {
            self.anchors.push_back(Anchor { boot_ns, offset_ns });
            return None;
        };
        if offset_ns.abs_diff(current.offset_ns) <= STEP_TOLERANCE_NS {
            // Slewing: follow it without cutting a segment.
            current.offset_ns = offset_ns;
            return None;
        }
        if self.anchors.len() >= MAX_ANCHORS {
            self.anchors.pop_front();
        }
        self.anchors.push_back(Anchor { boot_ns, offset_ns });
        Some(boot_ns)
    }

    pub(crate) fn is_known(&self) -> bool {
        !self.anchors.is_empty()
    }

    /// UTC nanoseconds of a boot time, using the clock segment in effect
    /// (the earliest segment for times before every observation).
    pub(crate) fn utc_of(&self, boot_ns: u64) -> Option<i128> {
        let anchor = self
            .anchors
            .iter()
            .rev()
            .find(|anchor| anchor.boot_ns <= boot_ns)
            .or_else(|| self.anchors.front())?;
        Some(i128::from(boot_ns).saturating_add(anchor.offset_ns))
    }
}

/// Days since 1970-01-01 to a civil date (proleptic Gregorian).
fn civil_from_days(days: i64) -> Option<(i64, u32, u32)> {
    let shifted = days.checked_add(719_468)?;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * month_index + 2) / 5 + 1).ok()?;
    let month = u32::try_from(if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    })
    .ok()?;
    let year = year_of_era
        .checked_add(era.checked_mul(400)?)?
        .checked_add(i64::from(month <= 2))?;
    Some((year, month, day))
}

/// RFC 3339 UTC text with nanoseconds, e.g. `2026-10-04T12:00:00.000000000Z`.
pub(crate) fn format_utc(utc_ns: i128) -> Option<String> {
    let seconds = utc_ns.div_euclid(1_000_000_000);
    let nanos = utc_ns.rem_euclid(1_000_000_000);
    let days = i64::try_from(seconds.div_euclid(86_400)).ok()?;
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days)?;
    let hour = second_of_day / 3600;
    let minute = second_of_day % 3600 / 60;
    let second = second_of_day % 60;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{nanos:09}Z"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting_matches_known_instants() {
        assert_eq!(format_utc(0).unwrap(), "1970-01-01T00:00:00.000000000Z");
        assert_eq!(
            format_utc(1_790_000_000_123_456_789).unwrap(),
            "2026-09-21T14:13:20.123456789Z"
        );
        assert_eq!(format_utc(-1).unwrap(), "1969-12-31T23:59:59.999999999Z");
        assert_eq!(
            format_utc(951_782_400_000_000_000).unwrap(),
            "2000-02-29T00:00:00.000000000Z"
        );
    }

    #[test]
    fn steps_cut_segments_and_slewing_does_not() {
        let mut clock = WallClock::default();
        assert!(!clock.is_known());
        assert_eq!(clock.observe(1_000, 5_000), None);
        assert_eq!(clock.observe(2_000, 6_100), None);
        assert_eq!(clock.utc_of(2_000), Some(6_100));
        let step_back = -86_400_000_000_000i128;
        assert_eq!(clock.observe(3_000, 7_000 + step_back), Some(3_000));
        assert_eq!(clock.utc_of(2_500), Some(2_500 + 4_100));
        assert_eq!(clock.utc_of(3_500), Some(3_500 + 4_000 + step_back));
        assert_eq!(clock.utc_of(10), Some(10 + 4_100));
    }
}
