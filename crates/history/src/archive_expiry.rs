//! How long the archive keeps its rows, decided here (D-96): the store
//! only deletes what a journal asks for.
//!
//! Rows older than `store.archive_retention_days` go: the minutes that
//! ended before then on the wall clock, and this boot's gaps that ended
//! before then on the boot clock (other boots' gaps go with their last
//! minute). For the Overview series the compression's prunes take their
//! minutes long before (D-82), so the age is their upper bound and what it
//! still deletes are gaps and minutes a failed prune left. Asked at most
//! every ten minutes, like the store did on its own before.

use dgxtop_core::clock::NANOS_PER_SECOND;
use dgxtop_core::ports::ArchiveExpiry;

use crate::wall_clock::WallClock;

/// Shortest boot time between two expiry requests.
pub(crate) const EXPIRY_INTERVAL_NS: u64 = 600 * NANOS_PER_SECOND;
const NANOS_PER_DAY: u64 = 86_400 * NANOS_PER_SECOND;

/// The archive's age limit and when it was last asked for.
#[derive(Debug)]
pub(crate) struct ArchiveExpiryPolicy {
    keep_ns: u64,
    last_request_boot_ns: Option<u64>,
}

impl ArchiveExpiryPolicy {
    pub(crate) fn new(retention_days: u32) -> Self {
        Self {
            keep_ns: u64::from(retention_days).saturating_mul(NANOS_PER_DAY),
            last_request_boot_ns: None,
        }
    }

    /// The expiry to send now, if one is due: the first once the wall
    /// clock is known, then every [`EXPIRY_INTERVAL_NS`].
    pub(crate) fn due(&mut self, now_boot_ns: u64, clock: &WallClock) -> Option<ArchiveExpiry> {
        let waited = self
            .last_request_boot_ns
            .is_none_or(|last| now_boot_ns.saturating_sub(last) >= EXPIRY_INTERVAL_NS);
        if !waited {
            return None;
        }
        let now_utc_ns = clock.utc_of(now_boot_ns)?;
        self.last_request_boot_ns = Some(now_boot_ns);
        let before_utc_ns = now_utc_ns.saturating_sub(i128::from(self.keep_ns));
        Some(ArchiveExpiry {
            before_utc_ns: i64::try_from(before_utc_ns).unwrap_or(i64::MIN),
            current_boot_before_ns: now_boot_ns.saturating_sub(self.keep_ns),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UTC_AT_BOOT: i128 = 1_800_000_000 * 1_000_000_000;

    fn paired_clock() -> WallClock {
        let mut clock = WallClock::default();
        clock.observe(0, UTC_AT_BOOT);
        clock
    }

    #[test]
    fn nothing_is_asked_before_the_wall_clock_is_known() {
        let mut policy = ArchiveExpiryPolicy::new(7);
        assert_eq!(
            policy.due(5 * NANOS_PER_SECOND, &WallClock::default()),
            None
        );
        assert!(policy.due(5 * NANOS_PER_SECOND, &paired_clock()).is_some());
    }

    #[test]
    fn the_cutoffs_are_the_retention_before_now_on_both_clocks() {
        let mut policy = ArchiveExpiryPolicy::new(7);
        let now = 8 * NANOS_PER_DAY;
        let expiry = policy.due(now, &paired_clock()).unwrap();
        assert_eq!(
            i128::from(expiry.before_utc_ns),
            UTC_AT_BOOT + i128::from(NANOS_PER_DAY)
        );
        assert_eq!(expiry.current_boot_before_ns, NANOS_PER_DAY);
    }

    #[test]
    fn a_young_boot_expires_none_of_its_gaps() {
        let mut policy = ArchiveExpiryPolicy::new(7);
        let expiry = policy.due(60 * NANOS_PER_SECOND, &paired_clock()).unwrap();
        assert_eq!(expiry.current_boot_before_ns, 0);
    }

    #[test]
    fn requests_are_ten_minutes_apart() {
        let mut policy = ArchiveExpiryPolicy::new(7);
        let clock = paired_clock();
        let first = NANOS_PER_SECOND;
        assert!(policy.due(first, &clock).is_some());
        assert_eq!(policy.due(first + EXPIRY_INTERVAL_NS - 1, &clock), None);
        assert!(policy.due(first + EXPIRY_INTERVAL_NS, &clock).is_some());
    }
}
