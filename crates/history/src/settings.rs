//! History settings derived from the validated configuration.

use dgxtop_core::config::{RuntimeConfig, StoreConfig};
use dgxtop_core::sampling::SamplingTiming;

use crate::query::QueryLimits;
use crate::state::HistorySettings;

fn to_usize(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// Retention and admission of the in-memory history; `archive_enabled`
/// makes finalization queue finished rows for the archive.
pub(crate) fn history_settings(
    store: &StoreConfig,
    timing: &SamplingTiming,
    archive_enabled: bool,
) -> HistorySettings {
    HistorySettings {
        raw_history_ns: store.raw_history_ns(),
        aggregate_retention_ns: store.aggregate_retention_ns(),
        max_series: to_usize(store.max_series.max(1)),
        finalize_grace_ns: timing.finalize_grace_ns,
        gap_merge_tolerance_ns: timing.fast_period_ns,
        archive_enabled,
    }
}

/// Static limits of the query engine.
pub(crate) fn query_limits(store: &StoreConfig, runtime: &RuntimeConfig) -> QueryLimits {
    QueryLimits::new(
        store.max_history_points,
        u64::from(runtime.max_history_result_bytes),
        u64::from(store.max_query_scan_records),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::clock::NANOS_PER_SECOND;
    use dgxtop_core::config::Config;

    #[test]
    fn defaults_keep_five_minutes_raw_and_a_day_of_minutes() {
        let config = Config::defaults().unwrap();
        let timing = SamplingTiming::from_sampling(&config.sampling);
        let settings = history_settings(&config.store, &timing, false);
        assert_eq!(settings.raw_history_ns, 300 * NANOS_PER_SECOND);
        assert_eq!(
            settings.aggregate_retention_ns,
            24 * 3600 * NANOS_PER_SECOND
        );
        assert_eq!(settings.max_series, 512);
        assert_eq!(settings.finalize_grace_ns, timing.finalize_grace_ns);
        assert_eq!(settings.gap_merge_tolerance_ns, timing.fast_period_ns);
        let limits = query_limits(&config.store, &config.runtime);
        assert_eq!(limits.max_points, 2000);
        assert_eq!(limits.max_scan_records, 200_000);
    }
}
