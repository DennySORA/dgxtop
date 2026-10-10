//! Injectable time.
//!
//! `boot_ns` is Linux `CLOCK_BOOTTIME` (includes suspend) and is the only clock
//! used for ages, deadlines and sample deltas, and only within one boot. UTC is
//! for display and cross-boot indexing; it may jump and never produces a rate.

use std::time::Duration;

use crate::identity::BootId;

/// Nanoseconds per millisecond.
pub const NANOS_PER_MILLI: u64 = 1_000_000;
/// Nanoseconds per second.
pub const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Source of boot identity, boot clock and wall clock. Implemented with
/// `clock_gettime(CLOCK_BOOTTIME)` in the runtime crate and by a fake in tests.
pub trait TimeProvider: Send + Sync {
    /// Current kernel boot identity.
    fn boot_id(&self) -> BootId;
    /// Monotonic boot clock in nanoseconds (includes suspend).
    fn now_boot_ns(&self) -> u64;
    /// Wall clock in nanoseconds since the Unix epoch; may step backwards.
    fn now_utc_ns(&self) -> i128;
}

/// Converts milliseconds to nanoseconds, saturating at `u64::MAX`.
pub const fn ms_to_ns(milliseconds: u64) -> u64 {
    milliseconds.saturating_mul(NANOS_PER_MILLI)
}

/// Converts a nanosecond count to whole milliseconds (floor).
pub const fn ns_to_ms(nanoseconds: u64) -> u64 {
    nanoseconds / NANOS_PER_MILLI
}

/// Duration until `deadline_boot_ns`, zero if already due.
pub fn until(now_boot_ns: u64, deadline_boot_ns: u64) -> Duration {
    Duration::from_nanos(deadline_boot_ns.saturating_sub(now_boot_ns))
}

/// Returns the first `base + k*period` strictly greater than `now` (k >= 1 when
/// `now >= base`). Missed slots are skipped, never replayed.
pub fn next_due_after(base_boot_ns: u64, period_ns: u64, now_boot_ns: u64) -> u64 {
    if period_ns == 0 {
        return now_boot_ns.saturating_add(1);
    }
    if now_boot_ns < base_boot_ns {
        return base_boot_ns;
    }
    let elapsed_periods = (now_boot_ns - base_boot_ns) / period_ns;
    base_boot_ns.saturating_add(elapsed_periods.saturating_add(1).saturating_mul(period_ns))
}

/// Deterministic phase offset in `[0, period)` derived from a stable label, so
/// that hosts do not all fire on the same instant.
pub fn stable_phase(label: &str, period_ns: u64) -> u64 {
    if period_ns == 0 {
        return 0;
    }
    // FNV-1a: stable across runs and platforms.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in label.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash % period_ns
}

/// Truncated exponential backoff (1, 2, 4 ... capped) with deterministic jitter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    base_ms: u64,
    max_ms: u64,
    jitter_permille: u64,
    attempt: u32,
    seed: u64,
}

impl Backoff {
    /// `jitter_permille` of 100 means +/-10 %.
    pub fn new(base_ms: u64, max_ms: u64, jitter_permille: u64, seed: u64) -> Self {
        Self {
            base_ms: base_ms.max(1),
            max_ms: max_ms.max(base_ms.max(1)),
            jitter_permille: jitter_permille.min(1000),
            attempt: 0,
            seed,
        }
    }

    /// Next delay in milliseconds; advances the attempt counter.
    pub fn next_delay_ms(&mut self) -> u64 {
        let exponent = self.attempt.min(32);
        let nominal = self
            .base_ms
            .saturating_mul(1u64.checked_shl(exponent).unwrap_or(u64::MAX))
            .min(self.max_ms);
        self.attempt = self.attempt.saturating_add(1);
        self.seed = splitmix64(self.seed);
        let span = nominal.saturating_mul(self.jitter_permille) / 1000;
        if span == 0 {
            return nominal;
        }
        let offset = self.seed % (span.saturating_mul(2).saturating_add(1));
        nominal
            .saturating_sub(span)
            .saturating_add(offset)
            .min(self.max_ms.saturating_add(span))
    }

    /// Resets to the first delay after a successful probe.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    pub const fn attempt(&self) -> u32 {
        self.attempt
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a wall-clock instant in nanoseconds since the
/// Unix epoch, clamped to years 0000..=9999 so the text keeps a fixed width
/// and its lexical order (audit/archive `*_utc` columns and retention cutoffs).
/// UTC is display/index only; it never yields a duration inside a boot.
pub fn format_utc_rfc3339(unix_ns: i128) -> String {
    const SECONDS_PER_DAY: i64 = 86_400;
    const MIN_UNIX_SECONDS: i64 = -62_167_219_200;
    const MAX_UNIX_SECONDS: i64 = 253_402_300_799;
    let seconds = unix_ns.div_euclid(i128::from(NANOS_PER_SECOND));
    let clamped = i64::try_from(seconds)
        .unwrap_or(if seconds < 0 {
            MIN_UNIX_SECONDS
        } else {
            MAX_UNIX_SECONDS
        })
        .clamp(MIN_UNIX_SECONDS, MAX_UNIX_SECONDS);
    let days = clamped.div_euclid(SECONDS_PER_DAY);
    let second_of_day = clamped.rem_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3600,
        (second_of_day / 60) % 60,
        second_of_day % 60
    )
}

/// Gregorian date of a day count since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
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

/// SplitMix64 step; small deterministic PRNG for jitter and fixtures.
pub const fn splitmix64(state: u64) -> u64 {
    let mut z = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_due_skips_missed_slots_without_replay() {
        // base 0, period 1000: at 3500 the next slot is 4000, not 1000/2000/3000.
        assert_eq!(next_due_after(0, 1000, 3500), 4000);
        assert_eq!(next_due_after(0, 1000, 4000), 5000);
        assert_eq!(next_due_after(500, 1000, 100), 500);
        // A clock jump far ahead schedules exactly one future slot.
        assert_eq!(next_due_after(0, 1000, 1_000_000_500), 1_000_001_000);
    }

    #[test]
    fn phase_is_stable_and_bounded() {
        let first = stable_phase("nvml/fast", 1_000_000_000);
        assert_eq!(first, stable_phase("nvml/fast", 1_000_000_000));
        assert!(first < 1_000_000_000);
        assert_eq!(stable_phase("x", 0), 0);
    }

    #[test]
    fn backoff_doubles_to_cap_with_bounded_jitter() {
        let mut backoff = Backoff::new(1000, 60_000, 100, 7);
        let nominal = [1000u64, 2000, 4000, 8000, 16000, 32000, 60000, 60000];
        for expected in nominal {
            let delay = backoff.next_delay_ms();
            let span = expected / 10;
            assert!(
                delay >= expected - span && delay <= expected + span,
                "delay {delay} outside {expected}+/-{span}"
            );
        }
        backoff.reset();
        assert_eq!(backoff.attempt(), 0);
        let mut deterministic_a = Backoff::new(1000, 60_000, 100, 42);
        let mut deterministic_b = Backoff::new(1000, 60_000, 100, 42);
        assert_eq!(
            deterministic_a.next_delay_ms(),
            deterministic_b.next_delay_ms()
        );
    }

    #[test]
    fn utc_formatting_is_fixed_width_and_ordered() {
        assert_eq!(format_utc_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_utc_rfc3339(1_790_000_000 * 1_000_000_000),
            "2026-09-21T14:13:20Z"
        );
        assert_eq!(format_utc_rfc3339(-1), "1969-12-31T23:59:59Z");
        assert_eq!(format_utc_rfc3339(i128::MAX), "9999-12-31T23:59:59Z");
        assert_eq!(format_utc_rfc3339(i128::MIN), "0000-01-01T00:00:00Z");
        assert!(format_utc_rfc3339(1_000_000_000) < format_utc_rfc3339(2_000_000_000));
    }
}
