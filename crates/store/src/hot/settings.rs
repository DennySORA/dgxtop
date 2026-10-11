//! Store settings derived from the validated configuration.

use dgxtop_core::clock::NANOS_PER_SECOND;
use dgxtop_core::config::{RuntimeConfig, StoreConfig};
use dgxtop_core::quality::Namespace;
use dgxtop_core::sampling::SamplingTiming;

use crate::candidates::CandidateSettings;
use crate::history::state::HistorySettings;
use crate::query::QueryLimits;

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
        timing: &SamplingTiming,
        runtime: &RuntimeConfig,
        namespace: Namespace,
        archive_enabled: bool,
    ) -> Self {
        let raw_history_ns = store.raw_history_ns();
        let aggregate_retention_ns = store.aggregate_retention_ns();
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
    fn defaults_follow_the_store_configuration() {
        let config = Config::defaults().unwrap();
        let timing = SamplingTiming::from_sampling(&config.sampling);
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
