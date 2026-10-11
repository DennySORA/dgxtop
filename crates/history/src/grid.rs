//! The minute grid of one boot: where the minute rollups begin and end.
//!
//! Minutes start where UTC is a whole minute (D-82), so a finished minute
//! is exactly one UTC minute and the Overview's bars start on their local
//! clock boundaries (local offsets are whole minutes in practice; a
//! non-minute offset leaves the local boundaries between two UTC minutes,
//! and each minute is then filed under the local minute it starts in).
//! The grid is fixed once per boot from the first boot-to-UTC pairing and
//! never moves: finished rows must never be cut again, so a later wall
//! clock step keeps the grid and only changes the UTC its minutes are
//! stamped with. On the boot clock the boundaries are `phase + k × 60 s`;
//! the time before the first boundary (`[0, phase)`, only when dgxtop
//! starts within a minute of boot) is a shorter head minute.

use dgxtop_core::tiers::Tier;

use crate::records::BUCKET_NS;

/// Minute boundaries on the boot clock: boot 0 and `phase_ns + k × 60 s`.
/// Where no grid is known yet the holder says so (`Option`): there is no
/// default grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MinuteGrid {
    /// The first boundary after boot 0, below one minute.
    phase_ns: u64,
}

impl MinuteGrid {
    /// Boundaries on whole minutes of the boot clock: the grid of a boot
    /// that fell on a whole UTC minute.
    #[cfg(test)]
    pub(crate) const BOOT_CLOCK: Self = Self { phase_ns: 0 };

    /// The grid whose boundaries fall on whole UTC minutes, given one
    /// boot-to-UTC pairing.
    pub(crate) fn on_utc_minutes(boot_ns: u64, utc_ns: i128) -> Self {
        let offset_ns = utc_ns.saturating_sub(i128::from(boot_ns));
        let phase = offset_ns.saturating_neg().rem_euclid(i128::from(BUCKET_NS));
        Self {
            phase_ns: u64::try_from(phase).unwrap_or(0),
        }
    }

    /// Boot time of the first whole minute after boot 0.
    #[cfg(test)]
    pub(crate) const fn phase_ns(&self) -> u64 {
        self.phase_ns
    }

    /// Start of the minute containing `time_ns`.
    pub(crate) const fn floor(&self, time_ns: u64) -> u64 {
        if time_ns < self.phase_ns {
            0
        } else {
            time_ns - (time_ns - self.phase_ns) % BUCKET_NS
        }
    }

    /// End of the minute starting at (or containing) `start_ns`: the first
    /// boundary after it.
    pub(crate) const fn end_of(&self, start_ns: u64) -> u64 {
        if start_ns < self.phase_ns {
            self.phase_ns
        } else {
            self.floor(start_ns).saturating_add(BUCKET_NS)
        }
    }

    /// First boundary at or after `time_ns`.
    pub(crate) const fn ceil(&self, time_ns: u64) -> u64 {
        if self.floor(time_ns) == time_ns {
            time_ns
        } else {
            self.end_of(time_ns)
        }
    }

    /// Whether `time_ns` is a minute boundary.
    #[cfg(test)]
    pub(crate) const fn is_boundary(&self, time_ns: u64) -> bool {
        self.floor(time_ns) == time_ns
    }
}

/// UTC start of the minute a finished minute ending at `end_utc_ns` is
/// filed under: the whole UTC minute ending nearest its end. On the grid
/// that is its own minute (a boot's head minute: the minute boot fell in),
/// so one finished minute is exactly one UTC minute. After a wall clock
/// step by part of a minute the grid stays, its minutes are a part of a
/// minute off and each is filed in the nearest UTC minute; a step back
/// files the minutes it repeats in the same UTC minutes again. Minutes are
/// floored by the 1 minute size's own rule (one rounding rule for every
/// minute on the wall clock).
pub(crate) fn utc_minute_ending_near(end_utc_ns: i64) -> i64 {
    let minute = i64::try_from(BUCKET_NS).unwrap_or(i64::MAX);
    Tier::OneMinute
        .align(end_utc_ns.saturating_add(minute / 2), 0)
        .saturating_sub(minute)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::testing::UTC_ON_A_WHOLE_MINUTE_NS;

    const SECOND: u64 = 1_000_000_000;

    #[test]
    fn the_boot_grid_cuts_whole_boot_minutes() {
        let grid = MinuteGrid::BOOT_CLOCK;
        assert_eq!(grid.floor(0), 0);
        assert_eq!(grid.floor(BUCKET_NS - 1), 0);
        assert_eq!(grid.floor(BUCKET_NS), BUCKET_NS);
        assert_eq!(grid.ceil(0), 0);
        assert_eq!(grid.ceil(1), BUCKET_NS);
        assert_eq!(grid.ceil(BUCKET_NS), BUCKET_NS);
        assert_eq!(grid.ceil(u64::MAX), u64::MAX);
        assert_eq!(grid.end_of(0), BUCKET_NS);
    }

    /// A boot at 12:00:37 UTC: its minutes start at boot 23 s (12:01:00)
    /// and every 60 s after; the 23 s before are a head minute.
    #[test]
    fn a_boot_that_started_mid_minute_has_minutes_on_whole_utc_minutes() {
        let noon_utc_ns = UTC_ON_A_WHOLE_MINUTE_NS;
        let boot_utc_ns = noon_utc_ns + 37 * i128::from(SECOND);
        // Paired 1000 s after boot.
        let grid = MinuteGrid::on_utc_minutes(1_000 * SECOND, boot_utc_ns + 1_000 * 1_000_000_000);
        assert_eq!(grid.phase_ns(), 23 * SECOND);
        for boundary in [23, 83, 143, 1_043].map(|seconds| seconds * SECOND) {
            assert!(grid.is_boundary(boundary));
            let utc = boot_utc_ns + i128::from(boundary);
            assert_eq!(utc % (60 * i128::from(SECOND)), 0, "{boundary}");
        }
        assert_eq!(grid.floor(10 * SECOND), 0, "the head minute");
        assert_eq!(grid.end_of(0), 23 * SECOND);
        assert_eq!(grid.floor(82 * SECOND), 23 * SECOND);
        assert_eq!(grid.end_of(23 * SECOND), 83 * SECOND);
        assert_eq!(grid.end_of(50 * SECOND), 83 * SECOND);
        assert_eq!(grid.ceil(24 * SECOND), 83 * SECOND);
        assert_eq!(grid.ceil(SECOND), 23 * SECOND);
        assert!(grid.is_boundary(0) && !grid.is_boundary(60 * SECOND));
    }

    #[test]
    fn a_minute_is_filed_in_the_utc_minute_it_ends_nearest() {
        let minute = i64::try_from(BUCKET_NS).unwrap();
        let noon = i64::try_from(UTC_ON_A_WHOLE_MINUTE_NS).unwrap();
        assert_eq!(utc_minute_ending_near(noon + minute), noon);
        // Slewed or stepped by part of a minute either way.
        assert_eq!(utc_minute_ending_near(noon + minute - 2), noon);
        assert_eq!(
            utc_minute_ending_near(noon + minute + 29 * 1_000_000_000),
            noon
        );
        assert_eq!(
            utc_minute_ending_near(noon + minute + 31 * 1_000_000_000),
            noon + minute
        );
        assert_eq!(utc_minute_ending_near(0), -minute, "before 1970");
        assert_eq!(utc_minute_ending_near(-minute - 1), -2 * minute);
        assert_eq!(
            utc_minute_ending_near(i64::MAX),
            i64::MAX / minute * minute - minute
        );
    }

    #[test]
    fn a_pairing_on_a_whole_minute_or_before_1970_still_lands_on_minutes() {
        assert_eq!(MinuteGrid::on_utc_minutes(0, 0).phase_ns(), 0);
        let before_epoch = MinuteGrid::on_utc_minutes(5 * SECOND, -i128::from(SECOND));
        // UTC -1 s at boot 5 s: UTC 0 (a whole minute) at boot 6 s.
        assert_eq!(before_epoch.phase_ns(), 6 * SECOND);
    }
}
