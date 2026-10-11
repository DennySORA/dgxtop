//! Sampling cadence derived from the validated configuration.
//!
//! The live state needs it for stale deadlines and source periods, the
//! history for its finalization horizon and gap merging (D-79); both derive
//! it here so the two never disagree on when a minute is final.

use crate::clock::ms_to_ns;
use crate::config::SamplingConfig;

/// Sampling cadence used for stale deadlines and the finalization horizon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SamplingTiming {
    /// `sampling.interval_ms`: period of the fast read groups.
    pub fast_period_ns: u64,
    /// `sampling.capacity_interval_ms`.
    pub capacity_period_ns: u64,
    /// `sampling.slow_health_interval_ms`.
    pub slow_period_ns: u64,
    pub stale_factor: u32,
    /// Minutes older than `now - grace` are rolled up. A fresh sample is
    /// never older than its read group's stale window plus the read
    /// deadline, so finalized minutes never change afterwards.
    pub finalize_grace_ns: u64,
}

impl SamplingTiming {
    pub fn from_sampling(sampling: &SamplingConfig) -> Self {
        let fast = ms_to_ns(u64::from(sampling.interval_ms.max(1)));
        let capacity = ms_to_ns(u64::from(sampling.capacity_interval_ms.max(1)));
        let slow = ms_to_ns(u64::from(sampling.slow_health_interval_ms.max(1)));
        let stale_factor = sampling.stale_factor.max(1);
        let slowest = fast.max(capacity).max(slow);
        let grace = u64::from(stale_factor)
            .saturating_mul(slowest)
            .saturating_add(ms_to_ns(u64::from(sampling.read_deadline_ms)));
        Self {
            fast_period_ns: fast,
            capacity_period_ns: capacity,
            slow_period_ns: slow,
            stale_factor,
            finalize_grace_ns: grace,
        }
    }
}

/// Stale deadline of a sample: `time + stale_factor x period`, saturating.
pub fn stale_deadline(time_ns: u64, stale_factor: u32, period_ns: u64) -> u64 {
    time_ns.saturating_add(u64::from(stale_factor.max(1)).saturating_mul(period_ns))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::NANOS_PER_SECOND;
    use crate::config::Config;

    #[test]
    fn defaults_give_a_grace_covering_the_slowest_group() {
        let config = Config::defaults().unwrap();
        let timing = SamplingTiming::from_sampling(&config.sampling);
        assert_eq!(timing.fast_period_ns, NANOS_PER_SECOND);
        assert_eq!(
            timing.finalize_grace_ns,
            15 * NANOS_PER_SECOND + 500_000_000
        );
    }

    #[test]
    fn stale_deadline_saturates_and_never_uses_a_zero_factor() {
        assert_eq!(stale_deadline(10, 3, 5), 25);
        assert_eq!(stale_deadline(10, 0, 5), 15);
        assert_eq!(stale_deadline(u64::MAX - 1, 3, 5), u64::MAX);
    }
}
