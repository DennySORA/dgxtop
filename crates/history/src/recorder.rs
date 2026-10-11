//! The history's side of the state's commits (`HistoryIngest`, D-79).
//!
//! The live state owns a [`HistoryRecorder`] as a `Box<dyn HistoryIngest>`
//! and calls it on its actor thread inside every commit, gap and
//! maintenance step, so history admission, eviction and gaps happen in the
//! same step as before the split; only the outcome the snapshot needs
//! (counters, health, recent gaps) crosses back. Finished rows stay here,
//! in the archive queue, until the actor drains them as journals.

use std::fmt;

use dgxtop_core::canonical::GapRecord;
use dgxtop_core::ports::{ArchiveJournal, HistoryAppends, HistoryIngest, HistoryOutcome};

use crate::archive::ArchiveQueue;
use crate::index::HistoryWriter;
use crate::output::HistoryReport;
use crate::wall_clock::WallClock;

/// Write side of the history, handed to the live state at composition.
pub struct HistoryRecorder {
    writer: HistoryWriter,
    archive: ArchiveQueue,
    clock: WallClock,
    archive_enabled: bool,
    /// Snapshot revision of the last ingest or publish; journals carry it.
    revision: u64,
}

impl fmt::Debug for HistoryRecorder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HistoryRecorder")
            .field("revision", &self.revision)
            .field("archive_enabled", &self.archive_enabled)
            .finish_non_exhaustive()
    }
}

impl HistoryRecorder {
    pub(crate) fn new(writer: HistoryWriter, archive: ArchiveQueue, archive_enabled: bool) -> Self {
        Self {
            writer,
            archive,
            clock: WallClock::default(),
            archive_enabled,
            revision: 0,
        }
    }

    /// Queues the report's finished rows and condenses it for the state.
    fn outcome(&mut self, mut report: HistoryReport) -> HistoryOutcome {
        let rows = std::mem::take(&mut report.archive_rows);
        let archive_rows_dropped = if self.archive_enabled {
            self.archive.enqueue(rows)
        } else {
            0
        };
        HistoryOutcome {
            values_dropped: report.gauge_drops.total(),
            intervals_dropped: report.interval_drops.total(),
            series_rejected: report
                .gauge_drops
                .series_limit
                .saturating_add(report.interval_drops.series_limit),
            evicted_records: report.evicted_records,
            new_gaps: report.new_gaps,
            gaps_added: report.gaps_added,
            series_count: report.series_count,
            truncated: report.truncated,
            archive_rows_dropped,
        }
    }
}

impl HistoryIngest for HistoryRecorder {
    fn ingest(
        &mut self,
        appends: HistoryAppends,
        now_boot_ns: u64,
        revision: u64,
    ) -> HistoryOutcome {
        self.revision = revision;
        let report = self.writer.apply(appends, now_boot_ns, revision);
        self.outcome(report)
    }

    fn record_gap(&mut self, gap: GapRecord) -> HistoryOutcome {
        let report = self.writer.record_gap(gap);
        self.outcome(report)
    }

    fn maintain(&mut self, now_boot_ns: u64) -> HistoryOutcome {
        let report = self.writer.maintain(now_boot_ns);
        self.outcome(report)
    }

    fn shed_raw(&mut self, now_boot_ns: u64, keep_ns: u64) -> HistoryOutcome {
        let report = self.writer.shed_raw(now_boot_ns, keep_ns);
        self.outcome(report)
    }

    fn set_revision(&mut self, revision: u64) {
        self.revision = revision;
        self.writer.set_revision(revision);
    }

    fn set_finalize_grace(&mut self, grace_ns: u64) {
        self.writer.set_finalize_grace(grace_ns);
    }

    fn observe_wall_clock(&mut self, boot_ns: u64, utc_ns: i128) {
        if let Some(discontinuity) = self.clock.observe(boot_ns, utc_ns) {
            self.writer.note_discontinuity(discontinuity);
        }
    }

    fn take_archive_journal(&mut self, max_bytes: u64) -> Option<ArchiveJournal> {
        if !self.archive_enabled {
            return None;
        }
        self.archive.take(max_bytes, &self.clock, self.revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::budget::{ByteLedger, Partition};
    use dgxtop_core::config::Config;
    use dgxtop_core::history::HistoryMethod;
    use dgxtop_core::metric::{CanonicalUnit, Scalar, descriptor, ids};
    use dgxtop_core::ports::{GaugeAppend, SegmentIdentity};
    use dgxtop_core::quality::SampleKind;
    use dgxtop_core::testing::{metric_key, source_id};

    use crate::History;

    const SECOND: u64 = 1_000_000_000;
    const MINUTE: u64 = 60 * SECOND;
    const UTC_AT_BOOT: i128 = 1_790_000_000_000_000_000;

    fn history(archive_enabled: bool) -> (History, ByteLedger) {
        let config = Config::defaults().unwrap();
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let history = History::new(
            &config.store,
            &config.sampling,
            &config.runtime,
            &ledger,
            archive_enabled,
        );
        (history, ledger)
    }

    fn gauge(seconds: u64, value: f64) -> GaugeAppend {
        GaugeAppend {
            key: metric_key("gpu:0", ids::GPU_TEMPERATURE, "gpu"),
            descriptor: descriptor(ids::GPU_TEMPERATURE).unwrap(),
            identity: SegmentIdentity {
                source_id: source_id("nvml:0:temp"),
                source_generation: 1,
                policy_revision: 1,
                sample_kind: SampleKind::Measured,
                method: HistoryMethod::TimeWeightedStep,
            },
            unit: CanonicalUnit::DegC,
            time_ns: seconds * SECOND,
            stale_deadline_ns: (seconds + 3) * SECOND,
            value: Scalar::from_f64(value).unwrap(),
        }
    }

    fn one_gauge(seconds: u64, value: f64) -> HistoryAppends {
        HistoryAppends {
            gauges: vec![gauge(seconds, value)],
            ..HistoryAppends::default()
        }
    }

    #[test]
    fn ingest_reports_series_and_drops_and_fills_the_index() {
        let (mut history, _ledger) = history(false);
        let outcome = history.recorder.ingest(one_gauge(1, 40.0), 2 * SECOND, 1);
        assert_eq!(outcome.series_count, 1);
        assert_eq!(outcome.values_dropped, 0);
        let repeated = history.recorder.ingest(one_gauge(1, 41.0), 2 * SECOND, 2);
        assert_eq!(repeated.values_dropped, 1, "not newer than the last sample");
        let future = history.recorder.ingest(one_gauge(9, 42.0), 3 * SECOND, 3);
        assert_eq!(future.values_dropped, 1, "after the commit time");
        assert_eq!(history.index.read().series_keys().len(), 1);
    }

    #[test]
    fn a_visible_gap_is_reported_to_the_state() {
        let (mut history, _ledger) = history(false);
        let gap = GapRecord {
            key: None,
            adapter_id: None,
            start_boot_ns: SECOND,
            end_boot_ns: 2 * SECOND,
            reason: "adapter_timeout".to_owned(),
            affected_count: 1,
        };
        let outcome = history.recorder.record_gap(gap.clone());
        assert_eq!(outcome.gaps_added, 1);
        assert_eq!(outcome.new_gaps, vec![gap]);
        assert!(outcome.changed_visible_state());
    }

    #[test]
    fn archive_off_never_queues_or_hands_out_journals() {
        let (mut history, ledger) = history(false);
        history.recorder.observe_wall_clock(0, UTC_AT_BOOT);
        for minute in 0..3 {
            let seconds = minute * 60 + 1;
            history
                .recorder
                .ingest(one_gauge(seconds, 40.0), (seconds + 1) * SECOND, minute + 1);
        }
        history.recorder.maintain(5 * MINUTE);
        assert!(history.recorder.take_archive_journal(1 << 20).is_none());
        assert_eq!(ledger.used(Partition::Reserve), 0);
    }

    #[test]
    fn journals_wait_for_the_wall_clock_and_carry_the_published_revision() {
        let (mut history, _ledger) = history(true);
        for minute in 0..3 {
            let seconds = minute * 60 + 1;
            history
                .recorder
                .ingest(one_gauge(seconds, 40.0), (seconds + 1) * SECOND, minute + 1);
        }
        let outcome = history.recorder.maintain(5 * MINUTE);
        assert_eq!(outcome.archive_rows_dropped, 0);
        assert!(
            history.recorder.take_archive_journal(1 << 20).is_none(),
            "no UTC mapping yet"
        );
        history.recorder.observe_wall_clock(0, UTC_AT_BOOT);
        history.recorder.set_revision(7);
        let journal = history.recorder.take_archive_journal(1 << 20).unwrap();
        assert_eq!(journal.revision, 7);
        assert!(!journal.buckets.is_empty());
    }

    #[test]
    fn a_wall_clock_step_is_a_discontinuity() {
        let (mut history, _ledger) = history(false);
        history.recorder.observe_wall_clock(SECOND, UTC_AT_BOOT);
        history
            .recorder
            .observe_wall_clock(2 * SECOND, UTC_AT_BOOT - 3600 * i128::from(SECOND));
        let discontinuities: Vec<u64> = history
            .index
            .read()
            .discontinuities
            .iter()
            .copied()
            .collect();
        assert_eq!(discontinuities, vec![2 * SECOND]);
    }
}
