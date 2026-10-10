//! Store settings derived from the validated configuration.

use dgxtop_core::clock::{NANOS_PER_SECOND, ms_to_ns};
use dgxtop_core::config::{RuntimeConfig, SamplingConfig, StoreConfig};
use dgxtop_core::quality::Namespace;

use crate::candidates::CandidateSettings;
use crate::history::state::HistorySettings;
use crate::query::QueryLimits;

/// Sampling cadence used for stale deadlines and the finalization horizon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Timing {
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

impl Timing {
    pub(crate) fn from_sampling(sampling: &SamplingConfig) -> Self {
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

/// Everything the hot store needs from `store`, `sampling` and `runtime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StoreSettings {
    pub history: HistorySettings,
    pub candidates: CandidateSettings,
    pub limits: QueryLimits,
    pub max_processes: usize,
    pub max_process_gpu_links: usize,
    /// Canonical keys not committed for this long leave the snapshot.
    pub key_ttl_ns: u64,
    /// Process keys leave the snapshot sooner (`ProcessKey` TTL cleanup).
    pub process_key_ttl_ns: u64,
}

fn seconds(value: u32) -> u64 {
    u64::from(value).saturating_mul(NANOS_PER_SECOND)
}

fn to_usize(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

impl StoreSettings {
    pub(crate) fn new(
        store: &StoreConfig,
        timing: &Timing,
        runtime: &RuntimeConfig,
        namespace: Namespace,
        archive_enabled: bool,
    ) -> Self {
        let raw_history_ns = seconds(store.raw_history_seconds.max(1));
        let aggregate_retention_ns =
            seconds(store.aggregate_retention_hours.max(1)).saturating_mul(3600);
        Self {
            history: HistorySettings {
                raw_history_ns,
                aggregate_retention_ns,
                max_series: to_usize(store.max_series.max(1)),
                finalize_grace_ns: timing.finalize_grace_ns,
                gap_merge_tolerance_ns: timing.fast_period_ns,
                archive_enabled,
            },
            candidates: CandidateSettings {
                retention_ns: seconds(store.raw_candidate_seconds.max(1)),
                max_sources: to_usize(store.max_sources.max(1)),
                namespace,
            },
            limits: QueryLimits::new(
                store.max_history_points,
                u64::from(runtime.max_history_result_bytes),
                u64::from(store.max_query_scan_records),
            ),
            max_processes: to_usize(store.max_processes),
            max_process_gpu_links: to_usize(store.max_process_gpu_links),
            key_ttl_ns: aggregate_retention_ns,
            process_key_ttl_ns: raw_history_ns,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::config::Config;

    #[test]
    fn defaults_give_a_grace_covering_the_slowest_group() {
        let config = Config::defaults().unwrap();
        let timing = Timing::from_sampling(&config.sampling);
        assert_eq!(timing.fast_period_ns, NANOS_PER_SECOND);
        assert_eq!(
            timing.finalize_grace_ns,
            15 * NANOS_PER_SECOND + 500_000_000
        );
        let settings = StoreSettings::new(
            &config.store,
            &timing,
            &config.runtime,
            Namespace::Live,
            false,
        );
        assert_eq!(settings.history.raw_history_ns, 300 * NANOS_PER_SECOND);
        assert_eq!(
            settings.history.aggregate_retention_ns,
            24 * 3600 * NANOS_PER_SECOND
        );
        assert_eq!(settings.history.max_series, 512);
        assert_eq!(settings.candidates.retention_ns, 60 * NANOS_PER_SECOND);
        assert_eq!(settings.limits.max_points, 2000);
        assert_eq!(settings.limits.max_scan_records, 200_000);
    }
}
