//! The journals a full store queue refused, kept for the next turn and
//! bounded (D-79: a byte budget, the oldest go first): one copy of each
//! run, the latest (every live run changes each minute, so copies would
//! pile up while the store is stuck), and the closed buckets within
//! [`BACKLOG_BUDGET_BYTES`], the oldest giving way. The deletions are
//! merged: a run forgotten is not written again, a run written after it
//! was forgotten is kept.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use dgxtop_core::identity::ProcessKey;
use dgxtop_core::process_history::{KeepLatest, ProcessHistoryJournal, RunBucketRow, RunInfo};
use dgxtop_core::tiers::Tier;

/// Bytes the refused buckets may hold.
pub(crate) const BACKLOG_BUDGET_BYTES: usize = 4 * 1024 * 1024;

/// The refused journals, merged (module docs).
#[derive(Debug)]
pub(crate) struct Backlog {
    runs: BTreeMap<ProcessKey, RunInfo>,
    buckets: VecDeque<RunBucketRow>,
    bucket_bytes: usize,
    keep_latest: BTreeMap<(ProcessKey, Tier), usize>,
    forget: BTreeSet<ProcessKey>,
    keep_runs: Option<usize>,
    budget_bytes: usize,
}

impl Default for Backlog {
    fn default() -> Self {
        Self::with_budget(BACKLOG_BUDGET_BYTES)
    }
}

impl Backlog {
    /// A backlog whose buckets keep within `budget_bytes`.
    pub(crate) fn with_budget(budget_bytes: usize) -> Self {
        Self {
            runs: BTreeMap::new(),
            buckets: VecDeque::new(),
            bucket_bytes: 0,
            keep_latest: BTreeMap::new(),
            forget: BTreeSet::new(),
            keep_runs: None,
            budget_bytes,
        }
    }

    /// Takes in `journal`, newer than everything held.
    pub(crate) fn absorb(&mut self, journal: ProcessHistoryJournal) {
        for key in journal.forget {
            self.runs.remove(&key);
            self.buckets.retain(|row| row.key != key);
            self.keep_latest.retain(|(kept, _), _| *kept != key);
            self.forget.insert(key);
        }
        self.bucket_bytes = self.buckets.iter().map(row_bytes).sum();
        for run in journal.runs {
            self.forget.remove(&run.key);
            self.runs.insert(run.key.clone(), run);
        }
        for row in journal.buckets {
            self.bucket_bytes += row_bytes(&row);
            self.buckets.push_back(row);
        }
        for keep in journal.keep_latest {
            self.keep_latest.insert((keep.key, keep.tier), keep.buckets);
        }
        self.keep_runs = journal.keep_runs.or(self.keep_runs);
        self.bound();
    }

    /// Everything held, as one journal; the backlog is empty after.
    pub(crate) fn take(&mut self) -> ProcessHistoryJournal {
        let taken = std::mem::replace(self, Self::with_budget(self.budget_bytes));
        ProcessHistoryJournal {
            runs: taken.runs.into_values().collect(),
            buckets: taken.buckets.into(),
            keep_latest: taken
                .keep_latest
                .into_iter()
                .map(|((key, tier), buckets)| KeepLatest { key, tier, buckets })
                .collect(),
            forget: taken.forget.into_iter().collect(),
            keep_runs: taken.keep_runs,
        }
    }

    /// Whether it deletes runs.
    pub(crate) fn has_deletions(&self) -> bool {
        !self.forget.is_empty() || self.keep_runs.is_some()
    }

    /// Bytes its buckets hold.
    #[cfg(test)]
    pub(crate) fn bucket_bytes(&self) -> usize {
        self.bucket_bytes
    }

    fn bound(&mut self) {
        while self.bucket_bytes > self.budget_bytes {
            let Some(oldest) = self.buckets.pop_front() else {
                break;
            };
            self.bucket_bytes = self.bucket_bytes.saturating_sub(row_bytes(&oldest));
        }
    }
}

/// What a refused bucket costs: the row and its key's texts.
fn row_bytes(row: &RunBucketRow) -> usize {
    let key = &row.key;
    std::mem::size_of::<RunBucketRow>()
        + key.host_id.as_str().len()
        + key.boot_id.as_str().len()
        + key.pid_namespace.as_str().len()
}

#[cfg(test)]
mod tests {
    use dgxtop_core::process_history::{RunBucket, RunDetails};
    use dgxtop_core::testing::test_process_key;

    use super::*;

    fn run(pid: i64, last_seen_utc_ns: i64) -> RunInfo {
        RunInfo {
            key: test_process_key(pid),
            name: "acme_llm.train".to_owned(),
            command: String::new(),
            cwd: String::new(),
            uid: None,
            gpus: Vec::new(),
            details: RunDetails::default(),
            started_utc_ns: 0,
            last_seen_utc_ns,
            end: None,
        }
    }

    fn row(pid: i64, start_utc_ns: i64) -> RunBucketRow {
        RunBucketRow {
            key: test_process_key(pid),
            tier: Tier::OneMinute,
            bucket: RunBucket::new(start_utc_ns),
        }
    }

    fn journal(runs: Vec<RunInfo>, buckets: Vec<RunBucketRow>) -> ProcessHistoryJournal {
        ProcessHistoryJournal {
            runs,
            buckets,
            ..ProcessHistoryJournal::default()
        }
    }

    #[test]
    fn each_run_is_kept_once_the_latest_copy() {
        let mut backlog = Backlog::default();
        for seen in 1..=100 {
            backlog.absorb(journal(vec![run(1, seen), run(2, seen)], Vec::new()));
        }
        let taken = backlog.take();
        let runs: Vec<(i32, i64)> = taken
            .runs
            .iter()
            .map(|run| (run.key.pid.get(), run.last_seen_utc_ns))
            .collect();
        assert_eq!(runs, vec![(1, 100), (2, 100)]);
        assert!(backlog.take().is_empty(), "taken whole");
    }

    #[test]
    fn buckets_keep_within_the_byte_budget_the_oldest_going_first() {
        let one = row_bytes(&row(1, 0));
        let mut backlog = Backlog::with_budget(3 * one);
        for start in 0..10 {
            backlog.absorb(journal(Vec::new(), vec![row(1, start)]));
        }
        assert!(backlog.bucket_bytes() <= 3 * one);
        let starts: Vec<i64> = backlog
            .take()
            .buckets
            .iter()
            .map(|row| row.bucket.start_utc_ns)
            .collect();
        assert_eq!(starts, vec![7, 8, 9]);
    }

    #[test]
    fn a_forgotten_run_is_not_written_unless_written_again() {
        let mut backlog = Backlog::default();
        backlog.absorb(ProcessHistoryJournal {
            runs: vec![run(1, 1), run(2, 1)],
            buckets: vec![row(1, 0), row(2, 0)],
            keep_latest: vec![KeepLatest {
                key: test_process_key(1),
                tier: Tier::OneMinute,
                buckets: 300,
            }],
            ..ProcessHistoryJournal::default()
        });
        backlog.absorb(ProcessHistoryJournal {
            forget: vec![test_process_key(1), test_process_key(3)],
            keep_runs: Some(512),
            ..ProcessHistoryJournal::default()
        });
        assert!(backlog.has_deletions());
        backlog.absorb(journal(vec![run(3, 2)], Vec::new()));
        let taken = backlog.take();
        let pids = |keys: Vec<&ProcessKey>| -> Vec<i32> {
            keys.into_iter().map(|key| key.pid.get()).collect()
        };
        assert_eq!(
            pids(taken.runs.iter().map(|run| &run.key).collect()),
            vec![2, 3]
        );
        assert_eq!(
            pids(taken.buckets.iter().map(|row| &row.key).collect()),
            vec![2]
        );
        assert!(taken.keep_latest.is_empty());
        assert_eq!(
            pids(taken.forget.iter().collect()),
            vec![1],
            "3 is written again"
        );
        assert_eq!(taken.keep_runs, Some(512));
        assert!(!backlog.has_deletions());
    }
}
