//! The history's side of the state's commits (`HistoryIngest`, D-79).
//!
//! The live state owns a [`HistoryRecorder`] as a `Box<dyn HistoryIngest>`
//! and calls it on its actor thread inside every commit, gap and
//! maintenance step, so history admission, eviction and gaps happen in the
//! same step as before the split; only the outcome the snapshot needs
//! (counters, health, recent gaps) crosses back. Finished rows stay here,
//! in the archive queue, until the actor drains them as journals; the
//! Overview series' finished minutes also go into their sizes (D-82), and
//! the journals carry the archive deletions those sizes allow.

use std::fmt;

use dgxtop_core::canonical::GapRecord;
use dgxtop_core::ports::{ArchiveJournal, HistoryAppends, HistoryIngest, HistoryOutcome};

use crate::archive::ArchiveQueue;
use crate::index::HistoryWriter;
use crate::output::{ArchiveRow, HistoryReport};
use crate::overview::OverviewTiers;
use crate::overview::rows::{is_overview_row, tier_minutes};
use crate::wall_clock::WallClock;

/// Overview minutes kept while no boot-to-UTC pairing is known yet.
const MAX_UNTIMED_ROWS: usize = 4096;

/// Write side of the history, handed to the live state at composition.
pub struct HistoryRecorder {
    writer: HistoryWriter,
    archive: ArchiveQueue,
    clock: WallClock,
    archive_enabled: bool,
    /// Snapshot revision of the last ingest or publish; journals carry it.
    revision: u64,
    overview: OverviewTiers,
    /// Overview minutes finished before the wall clock was known.
    untimed: Vec<ArchiveRow>,
    now_boot_ns: u64,
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
    pub(crate) fn new(
        writer: HistoryWriter,
        archive: ArchiveQueue,
        archive_enabled: bool,
        overview: OverviewTiers,
    ) -> Self {
        Self {
            writer,
            archive,
            clock: WallClock::default(),
            archive_enabled,
            revision: 0,
            overview,
            untimed: Vec::new(),
            now_boot_ns: 0,
        }
    }

    /// The Overview minutes among `rows` into their sizes, on the wall
    /// clock; kept (bounded) until the clock is known.
    fn feed_overview(&mut self, rows: &[ArchiveRow]) {
        self.untimed
            .extend(rows.iter().filter(|row| is_overview_row(row)).cloned());
        if self.untimed.is_empty() {
            return;
        }
        match tier_minutes(&self.untimed, &self.clock) {
            Some(minutes) => {
                self.untimed.clear();
                self.overview.feed(minutes);
            }
            None => {
                let excess = self.untimed.len().saturating_sub(MAX_UNTIMED_ROWS);
                self.untimed.drain(..excess);
            }
        }
    }

    /// Queues the report's finished rows and condenses it for the state.
    fn outcome(&mut self, mut report: HistoryReport) -> HistoryOutcome {
        let rows = std::mem::take(&mut report.archive_rows);
        self.feed_overview(&rows);
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
        self.now_boot_ns = self.now_boot_ns.max(now_boot_ns);
        let report = self.writer.apply(appends, now_boot_ns, revision);
        self.outcome(report)
    }

    fn record_gap(&mut self, gap: GapRecord) -> HistoryOutcome {
        let report = self.writer.record_gap(gap);
        self.outcome(report)
    }

    fn maintain(&mut self, now_boot_ns: u64) -> HistoryOutcome {
        self.now_boot_ns = self.now_boot_ns.max(now_boot_ns);
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
        self.feed_overview(&[]);
    }

    fn take_archive_journal(&mut self, max_bytes: u64) -> Option<ArchiveJournal> {
        if !self.archive_enabled {
            return None;
        }
        let prunes = if self.archive.knows_identity() {
            self.overview.take_prunes(self.now_boot_ns)
        } else {
            Vec::new()
        };
        self.archive
            .take(max_bytes, &self.clock, self.revision, prunes)
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

    /// The Overview series' finalized minutes reach their sizes on the wall
    /// clock, and a read adds the minutes not finalized yet (D-82): the
    /// bar being filled holds every sample up to now.
    #[test]
    fn overview_bars_hold_finalized_and_unfinalized_minutes() {
        use dgxtop_core::history::SeriesInEveryBoot;
        use dgxtop_core::overview_history::{OverviewAnswer, OverviewRequest};
        use dgxtop_core::ports::{CancelFlag, OverviewReader};
        use dgxtop_core::process_history::Tier;
        use dgxtop_core::testing::FakeClock;

        let (mut history, _ledger) = history(false);
        history.recorder.observe_wall_clock(0, UTC_AT_BOOT);
        // A sample every 2 s holds until the next (it goes stale after 3).
        let samples = 5 * 60 / 2;
        for step in 0..samples {
            let seconds = step * 2;
            history
                .recorder
                .ingest(one_gauge(seconds, 40.0), seconds * SECOND, step + 1);
        }
        let now = 5 * MINUTE;
        history.recorder.maintain(now);
        let clock = FakeClock::new(now);
        clock.step_utc_ns(i64::try_from(now).unwrap());
        let series = SeriesInEveryBoot::from(&metric_key("gpu:0", ids::GPU_TEMPERATURE, "gpu"));
        let utc_at_boot = i64::try_from(UTC_AT_BOOT).unwrap();
        let hour = Tier::OneHour.align(utc_at_boot, 0);
        let request = OverviewRequest::Bars {
            series,
            tier: Tier::OneHour,
            from_utc_ns: hour,
            end_utc_ns: hour + 2 * 3_600 * 1_000_000_000,
            at_utc_ns: utc_at_boot + i64::try_from(now).unwrap(),
        };
        let read = |deadline, cancel: &CancelFlag| {
            OverviewReader::read(&history.index, &request, deadline, cancel, clock.as_ref())
        };
        let answer = read(u64::MAX, &CancelFlag::new()).unwrap();
        let OverviewAnswer::Bars(bars) = answer else {
            panic!("bars asked");
        };
        let held: u64 = bars.bars.iter().map(|bar| bar.covered_ns).sum();
        // From the first sample to now (the last one's step reaches now).
        assert_eq!(held, now, "finalized and unfinalized minutes, once");
        assert!(bars.bars.iter().all(|bar| bar.mean() == Some(40.0)));
        assert!(
            bars.settled_through_utc_ns > utc_at_boot,
            "minutes were finalized"
        );
        let cancelled = CancelFlag::new();
        cancelled.cancel();
        assert!(read(u64::MAX, &cancelled).is_err());
        assert!(read(0, &CancelFlag::new()).is_err(), "past its deadline");
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
