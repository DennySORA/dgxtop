//! Visible counters of the live state (finer than `StoreHealth`).

use dgxtop_core::ports::HistoryOutcome;

/// Monotonic counters exposed through `SharedSnapshot::counters`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreCounters {
    pub commits_total: u64,
    pub duplicate_batches_total: u64,
    /// Conflicts and every `Rejected*` acknowledgement.
    pub rejected_batches_total: u64,
    /// Gauge values that did not enter history (late, out of order, future,
    /// capacity, series or segment limit).
    pub history_values_dropped_total: u64,
    /// Counter intervals that did not enter history (overlap, late, ...).
    pub history_intervals_dropped_total: u64,
    /// New series refused because `store.max_series` series exist.
    pub series_rejected_total: u64,
    /// Records removed because a partition was full (not normal retention).
    pub evicted_records_total: u64,
    pub gaps_total: u64,
    /// `record_gap` calls with `end <= start` (ignored).
    pub invalid_gaps_total: u64,
    /// Archive rows that could not be queued (journal full).
    pub archive_rows_dropped_total: u64,
}

fn add(counter: &mut u64, amount: u64) {
    *counter = counter.saturating_add(amount);
}

impl StoreCounters {
    pub(crate) fn absorb_history(&mut self, outcome: &HistoryOutcome) {
        add(
            &mut self.history_values_dropped_total,
            outcome.values_dropped,
        );
        add(
            &mut self.history_intervals_dropped_total,
            outcome.intervals_dropped,
        );
        add(&mut self.series_rejected_total, outcome.series_rejected);
        add(&mut self.evicted_records_total, outcome.evicted_records);
        add(
            &mut self.archive_rows_dropped_total,
            outcome.archive_rows_dropped,
        );
    }

    pub(crate) fn count_commit(&mut self) {
        add(&mut self.commits_total, 1);
    }

    pub(crate) fn count_duplicate(&mut self) {
        add(&mut self.duplicate_batches_total, 1);
    }

    pub(crate) fn count_rejection(&mut self) {
        add(&mut self.rejected_batches_total, 1);
    }

    pub(crate) fn count_gaps(&mut self, gaps: u64) {
        add(&mut self.gaps_total, gaps);
    }

    pub(crate) fn count_invalid_gap(&mut self) {
        add(&mut self.invalid_gaps_total, 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_outcomes_and_events_add_up() {
        let mut counters = StoreCounters::default();
        let outcome = HistoryOutcome {
            values_dropped: 2,
            intervals_dropped: 3,
            series_rejected: 4,
            evicted_records: 5,
            archive_rows_dropped: 6,
            ..HistoryOutcome::default()
        };
        counters.absorb_history(&outcome);
        counters.absorb_history(&outcome);
        counters.count_commit();
        counters.count_duplicate();
        counters.count_rejection();
        counters.count_gaps(7);
        counters.count_invalid_gap();
        assert_eq!(
            counters,
            StoreCounters {
                commits_total: 1,
                duplicate_batches_total: 1,
                rejected_batches_total: 1,
                history_values_dropped_total: 4,
                history_intervals_dropped_total: 6,
                series_rejected_total: 8,
                evicted_records_total: 10,
                gaps_total: 7,
                invalid_gaps_total: 1,
                archive_rows_dropped_total: 12,
            }
        );
    }

    /// Counters never wrap: a total at its maximum stays there.
    #[test]
    fn counters_saturate() {
        let mut counters = StoreCounters {
            gaps_total: u64::MAX - 1,
            ..StoreCounters::default()
        };
        counters.count_gaps(5);
        assert_eq!(counters.gaps_total, u64::MAX);
    }
}
