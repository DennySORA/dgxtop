//! What the Overview's sizes may hold (D-95): the history, not the store,
//! decides which series go. Two byte budgets bound them: the cache's memory
//! ([`CACHE_BUDGET_BYTES`], counted like the archive queue's rows: the
//! structs plus their strings) and the pages the tiers store has in use
//! ([`STORE_BUDGET_BYTES`], as each append reports them). Past either, the
//! series given a minute longest ago give way through the cache's eviction,
//! which deletes them from the store and the archive too.

use std::mem::size_of;

use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::identity::MetricKey;
use dgxtop_core::overview_history::TierStoreUsage;

const MIB: usize = 1024 * 1024;
/// The cache's memory: every size of every series, with what each fills
/// and the minutes waiting. A full series (every size at its 300 buckets,
/// with the queues' spare room) takes about 350 KiB; the host's charts and
/// every GPU's, disk's and interface's take far less.
pub(crate) const CACHE_BUDGET_BYTES: usize = 32 * MIB;
/// The tiers store's pages in use past which series give way: room for the
/// WAL below the 64 MiB page cap of a small database, which refuses a
/// write past it (`DiskFull`) and never deletes on its own.
pub(crate) const STORE_BUDGET_BYTES: u64 = 40 * 1024 * 1024;
/// Series one append answer past the store budget lets go at most.
const MAX_GIVING_WAY: u64 = 8;

/// Heap bytes of a series' identity.
pub(crate) fn series_bytes(series: &SeriesInEveryBoot) -> usize {
    series.host_id.as_str().len()
        + series.entity_id.as_str().len()
        + series.metric_id.as_str().len()
        + series.scope.as_str().len()
}

/// Heap bytes of a boot's key of a series.
pub(crate) fn key_bytes(key: &MetricKey) -> usize {
    size_of::<MetricKey>()
        + key.host_id.as_str().len()
        + key.boot_id.as_str().len()
        + key.entity_id.as_str().len()
        + key.metric_id.as_str().len()
        + key.scope.as_str().len()
}

/// How many series give way after an append left the store at `usage`:
/// none within `budget_bytes`, else the excess over a series' mean size
/// (at least one, at most [`MAX_GIVING_WAY`]).
pub(crate) fn series_giving_way(usage: TierStoreUsage, budget_bytes: u64) -> usize {
    if usage.used_bytes <= budget_bytes || usage.series == 0 {
        return 0;
    }
    let mean = (usage.used_bytes / usage.series).max(1);
    let excess = usage.used_bytes - budget_bytes;
    let count = excess
        .div_ceil(mean)
        .clamp(1, MAX_GIVING_WAY.min(usage.series));
    usize::try_from(count).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(used_bytes: u64, series: u64) -> TierStoreUsage {
        TierStoreUsage { used_bytes, series }
    }

    /// Within the budget nothing goes; past it the excess over a series'
    /// mean size, one at least and a few at most, never more than held.
    #[test]
    fn the_excess_over_the_budget_gives_way() {
        assert_eq!(series_giving_way(usage(100, 10), 100), 0);
        assert_eq!(series_giving_way(usage(101, 10), 100), 1, "one at least");
        assert_eq!(series_giving_way(usage(130, 10), 100), 3);
        assert_eq!(series_giving_way(usage(1_000, 10), 100), 8, "a few at most");
        assert_eq!(
            series_giving_way(usage(1_000, 2), 100),
            2,
            "never more than held"
        );
        assert_eq!(series_giving_way(usage(1_000, 0), 100), 0, "an empty store");
    }
}
