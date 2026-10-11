//! What the process history's long-term store keeps (D-94): the history
//! decides, the store executes. Each size of a run keeps its latest
//! [`Tier::capacity`] buckets (D-77: older time lives in the coarser
//! sizes); the 5 s buckets are the short-term file's, never stored; the
//! runs the book lets go are forgotten; and past [`STORED_BUDGET_BYTES`]
//! the ended runs last seen longest ago give way (D-77: no run expires by
//! age). The startup load asks for the runs the book lists and the buckets
//! it holds in memory.

use std::collections::BTreeSet;

use dgxtop_core::identity::ProcessKey;
use dgxtop_core::process_history::{
    BucketRequest, KeepLatest, MAX_LISTED_RUNS, ProcessHistoryLoadRequest, RunBucketRow, StoreUsage,
};
use dgxtop_core::tiers::Tier;

const MIB: u64 = 1024 * 1024;
/// Database pages the runs may use: below the small database's page cap
/// (about 48 MiB, room left for the WAL in its 64 MiB), so runs give way
/// before an append fails for a full database.
pub(crate) const STORED_BUDGET_BYTES: u64 = 40 * MIB;
/// Ended runs whose buckets the startup load reads, the last seen first;
/// the others are read when opened.
const ENDED_RUNS_LOADED_WITH_BUCKETS: usize = 32;
/// Runs a database over its budget gives way at most at once: the next
/// append's usage says whether more must go.
const MAX_RUNS_GIVING_WAY: usize = 50;

/// What the startup load reads.
pub(crate) fn startup_request() -> ProcessHistoryLoadRequest {
    ProcessHistoryLoadRequest {
        runs: MAX_LISTED_RUNS,
        ended_runs_with_buckets: ENDED_RUNS_LOADED_WITH_BUCKETS,
        buckets: held_buckets(),
    }
}

/// The buckets of a run the history holds: of every stored size, coarsest
/// first, as many as the size keeps.
pub(crate) fn held_buckets() -> BucketRequest {
    BucketRequest {
        latest: Tier::ALL
            .into_iter()
            .filter(|tier| is_stored(*tier))
            .rev()
            .map(|tier| (tier, tier.capacity()))
            .collect(),
    }
}

/// Whether the long-term store keeps buckets of `tier`: every size of a
/// minute and more; the 5 s buckets live in the short-term file.
pub(crate) fn is_stored(tier: Tier) -> bool {
    tier != Tier::Recent
}

/// Each size of a run that `rows` write to keeps its latest buckets.
pub(crate) fn keep_latest_of(rows: &[RunBucketRow]) -> Vec<KeepLatest> {
    let touched: BTreeSet<(&ProcessKey, Tier)> =
        rows.iter().map(|row| (&row.key, row.tier)).collect();
    touched
        .into_iter()
        .map(|(key, tier)| KeepLatest {
            key: key.clone(),
            tier,
            buckets: tier.capacity(),
        })
        .collect()
}

/// How many of the ended runs give way for the store to come back within
/// `budget_bytes`: the excess over a run's mean size, at least one, at
/// most [`MAX_RUNS_GIVING_WAY`]; none within the budget.
pub(crate) fn runs_giving_way(usage: StoreUsage, budget_bytes: u64) -> usize {
    let excess = usage.used_bytes.saturating_sub(budget_bytes);
    if excess == 0 || usage.runs == 0 {
        return 0;
    }
    let per_run = (usage.used_bytes / usage.runs).max(1);
    let runs = usize::try_from(excess.div_ceil(per_run)).unwrap_or(usize::MAX);
    runs.clamp(1, MAX_RUNS_GIVING_WAY)
}

/// The runs the store keeps when it holds more than the book lists (runs
/// a lost journal never forgot).
pub(crate) fn runs_to_keep(usage: StoreUsage) -> Option<usize> {
    let listed = u64::try_from(MAX_LISTED_RUNS).unwrap_or(u64::MAX);
    (usage.runs > listed).then_some(MAX_LISTED_RUNS)
}

#[cfg(test)]
mod tests {
    use dgxtop_core::process_history::RunBucket;
    use dgxtop_core::testing::test_process_key;

    use super::*;

    #[test]
    fn the_load_reads_every_stored_size_coarsest_first() {
        let request = startup_request();
        assert_eq!(request.runs, MAX_LISTED_RUNS);
        assert_eq!(request.ended_runs_with_buckets, 32);
        let sizes: Vec<Tier> = request
            .buckets
            .latest
            .iter()
            .map(|(tier, _)| *tier)
            .collect();
        assert_eq!(sizes.first(), Some(&Tier::OneDay));
        assert_eq!(sizes.last(), Some(&Tier::OneMinute));
        assert!(
            !sizes.contains(&Tier::Recent),
            "the 5 s buckets are not stored"
        );
        assert_eq!(sizes.len(), Tier::ALL.len() - 1);
        assert!(
            request
                .buckets
                .latest
                .iter()
                .all(|(tier, latest)| *latest == tier.capacity())
        );
    }

    #[test]
    fn each_size_written_keeps_its_latest_buckets_once() {
        let row = |pid: i64, tier: Tier, start: i64| RunBucketRow {
            key: test_process_key(pid),
            tier,
            bucket: RunBucket::new(start),
        };
        let rows = [
            row(1, Tier::OneMinute, 0),
            row(1, Tier::OneMinute, 60),
            row(1, Tier::OneHour, 0),
            row(2, Tier::OneMinute, 0),
        ];
        let kept: Vec<(i32, Tier, usize)> = keep_latest_of(&rows)
            .into_iter()
            .map(|keep| (keep.key.pid.get(), keep.tier, keep.buckets))
            .collect();
        assert_eq!(
            kept,
            vec![
                (1, Tier::OneMinute, 300),
                (1, Tier::OneHour, 300),
                (2, Tier::OneMinute, 300)
            ]
        );
        assert!(keep_latest_of(&[]).is_empty());
    }

    #[test]
    fn runs_give_way_for_the_excess_within_a_batch() {
        let usage = |used_bytes: u64, runs: u64| StoreUsage { used_bytes, runs };
        assert_eq!(
            runs_giving_way(usage(40 * MIB, 100), 40 * MIB),
            0,
            "at the budget"
        );
        assert_eq!(
            runs_giving_way(usage(41 * MIB, 0), 40 * MIB),
            0,
            "no run to give"
        );
        // 1 MiB a run: 2 MiB over is two runs.
        assert_eq!(runs_giving_way(usage(42 * MIB, 42), 40 * MIB), 2);
        assert_eq!(
            runs_giving_way(usage(40 * MIB + 1, 40), 40 * MIB),
            1,
            "at least one"
        );
        assert_eq!(
            runs_giving_way(usage(400 * MIB, 400), 40 * MIB),
            MAX_RUNS_GIVING_WAY,
            "a batch at most"
        );
    }

    #[test]
    fn runs_beyond_the_listed_ones_are_cut() {
        let usage = |runs: u64| StoreUsage {
            used_bytes: MIB,
            runs,
        };
        assert_eq!(runs_to_keep(usage(512)), None);
        assert_eq!(runs_to_keep(usage(513)), Some(MAX_LISTED_RUNS));
        assert_eq!(runs_to_keep(usage(0)), None);
    }
}
