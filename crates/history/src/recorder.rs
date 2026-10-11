//! The history's side of the state's commits (`HistoryIngest`, D-79).
//!
//! The live state owns a [`HistoryRecorder`] as a `Box<dyn HistoryIngest>`
//! and calls it on its actor thread inside every commit, gap and
//! maintenance step, so history admission, eviction and gaps happen in the
//! same step as before the split; only the outcome the snapshot needs
//! (counters, health, recent gaps) crosses back. Finished rows stay here,
//! in the archive queue, until the actor drains them as journals; the
//! Overview series' finished minutes also go into their sizes (D-82), and
//! the journals carry the archive deletions those sizes allow. The first
//! boot-to-UTC pairing fixes the boot's minute grid (whole UTC minutes);
//! no minute is finished before it, so every finished minute already has
//! its place on the wall clock.

use std::fmt;

use dgxtop_core::canonical::GapRecord;
use dgxtop_core::ports::{ArchiveJournal, HistoryAppends, HistoryIngest, HistoryOutcome};

use crate::archive::ArchiveQueue;
use crate::grid::MinuteGrid;
use crate::index::HistoryWriter;
use crate::output::{ArchiveRow, HistoryReport};
use crate::overview::OverviewTiers;
use crate::overview::rows::overview_minutes;
use crate::wall_clock::WallClock;

/// Write side of the history, handed to the live state at composition.
pub struct HistoryRecorder {
    writer: HistoryWriter,
    archive: ArchiveQueue,
    clock: WallClock,
    archive_enabled: bool,
    /// Snapshot revision of the last ingest or publish; journals carry it.
    revision: u64,
    overview: OverviewTiers,
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
            now_boot_ns: 0,
        }
    }

    /// The Overview minutes among `rows` into their sizes, on the wall
    /// clock (known: the grid, and so every finished minute, comes only
    /// from the first pairing); one it cannot place becomes a gap in
    /// `report`, never a silent hole in the bars.
    fn feed_overview(&mut self, rows: &[ArchiveRow], report: &mut HistoryReport) {
        let minutes = overview_minutes(rows, &self.clock);
        debug_assert!(
            self.clock.is_known() || minutes.placed.is_empty() && minutes.unplaced.is_empty(),
            "a minute was finished before the boot was paired with UTC"
        );
        if !minutes.placed.is_empty() {
            self.overview.feed(minutes.placed);
        }
        for gap in minutes.unplaced {
            let noted = self.writer.record_gap(gap);
            report.new_gaps.extend(noted.new_gaps);
            report.gaps_added = report.gaps_added.saturating_add(noted.gaps_added);
        }
    }

    /// Queues the report's finished rows and condenses it for the state.
    fn outcome(&mut self, mut report: HistoryReport) -> HistoryOutcome {
        let rows = std::mem::take(&mut report.archive_rows);
        self.feed_overview(&rows, &mut report);
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

    /// The first pairing fixes the boot's minutes; a later step keeps them
    /// (finished rows are never cut again) and only moves their UTC stamps.
    /// This is the only way to the grid, so a finished minute always has
    /// its place on the wall clock.
    fn observe_wall_clock(&mut self, boot_ns: u64, utc_ns: i128) {
        if !self.clock.is_known() {
            self.writer
                .set_minute_grid(MinuteGrid::on_utc_minutes(boot_ns, utc_ns));
        }
        if let Some(discontinuity) = self.clock.observe(boot_ns, utc_ns) {
            self.writer.note_discontinuity(discontinuity);
        }
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
        self.archive.drop_pruned(&prunes, &self.clock);
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
    use dgxtop_core::testing::{UTC_ON_A_WHOLE_MINUTE_NS, metric_key, source_id};

    use crate::History;

    const SECOND: u64 = 1_000_000_000;
    const MINUTE: u64 = 60 * SECOND;
    /// 20 s into a UTC minute: the boot's minutes start at boot 40 s, not
    /// on the boot clock's own minutes.
    const UTC_AT_BOOT: i128 = UTC_ON_A_WHOLE_MINUTE_NS + 20 * SECOND as i128;

    /// A query clock at boot `now_ns` on the wall clock the tests pair.
    fn clock_at(now_ns: u64) -> std::sync::Arc<dgxtop_core::testing::FakeClock> {
        use dgxtop_core::clock::TimeProvider;

        let clock = dgxtop_core::testing::FakeClock::new(now_ns);
        let utc = UTC_AT_BOOT + i128::from(now_ns);
        clock.step_utc_ns(i64::try_from(utc - clock.now_utc_ns()).unwrap());
        clock
    }

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

    /// A pairing that arrives late: the minutes wait (none is cut on a
    /// grid that would move), then are cut on whole UTC minutes, the
    /// boot's head minute ending on the first one, and journaled with the
    /// published revision.
    #[test]
    fn minutes_wait_for_the_wall_clock_and_start_on_whole_utc_minutes() {
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
        // Boot was 20 s into a UTC minute: whole minutes at boot 40 s, 100 s…
        let late = 5 * MINUTE;
        history
            .recorder
            .observe_wall_clock(late, UTC_AT_BOOT + i128::from(late));
        history.recorder.maintain(late);
        history.recorder.set_revision(7);
        let journal = history.recorder.take_archive_journal(1 << 20).unwrap();
        assert_eq!(journal.revision, 7);
        let spans: Vec<(u64, u64)> = journal
            .buckets
            .iter()
            .map(|bucket| (bucket.start_boot_ns, bucket.end_boot_ns))
            .collect();
        assert_eq!(
            spans,
            vec![
                (0, 40 * SECOND),
                (40 * SECOND, 100 * SECOND),
                (100 * SECOND, 160 * SECOND)
            ]
        );
        for bucket in &journal.buckets[1..] {
            assert!(bucket.start_utc.ends_with(":00.000000000Z"), "{bucket:?}");
            assert!(bucket.end_utc.ends_with(":00.000000000Z"), "{bucket:?}");
            assert!(bucket.covered_ns <= MINUTE);
        }
        assert!(journal.buckets[0].end_utc.ends_with(":00.000000000Z"));
        assert_eq!(journal.buckets[0].covered_ns, 3 * SECOND);
    }

    /// A boot 20 s into a UTC minute, on a local clock an hour and a half
    /// ahead: every 1 minute bar starts on a whole local minute and holds
    /// exactly that minute, so a value that changes on a minute boundary
    /// is never mixed into the neighbouring bar (D-82).
    #[test]
    fn minute_bars_start_and_end_on_local_clock_minutes() {
        use dgxtop_core::history::SeriesInEveryBoot;
        use dgxtop_core::overview_history::{OverviewAnswer, OverviewRequest};
        use dgxtop_core::ports::{CancelFlag, OverviewReader};
        use dgxtop_core::process_history::Tier;

        let (mut history, _ledger) = history(false);
        let offset_s = 5_400;
        history.index.overview().set_utc_offset(offset_s);
        history.recorder.observe_wall_clock(0, UTC_AT_BOOT);
        let utc_at_boot = i64::try_from(UTC_AT_BOOT).unwrap();
        let minute = i64::try_from(MINUTE).unwrap();
        // UTC minute of the sample at boot `seconds`, counted from boot's.
        let utc_minute = |seconds: u64| {
            let utc = utc_at_boot + i64::try_from(seconds * SECOND).unwrap();
            utc.div_euclid(minute) - utc_at_boot.div_euclid(minute)
        };
        let samples = 6 * 60 / 2;
        for step in 0..samples {
            let seconds = step * 2;
            let value = if utc_minute(seconds) % 2 == 0 {
                10.0
            } else {
                20.0
            };
            history
                .recorder
                .ingest(one_gauge(seconds, value), seconds * SECOND, step + 1);
        }
        let now = 6 * MINUTE;
        history.recorder.maintain(now);
        let clock = clock_at(now);
        let shift = i64::from(offset_s) * 1_000_000_000;
        let from = Tier::OneMinute.align(utc_at_boot, shift);
        let request = OverviewRequest::Bars {
            series: SeriesInEveryBoot::from(&metric_key("gpu:0", ids::GPU_TEMPERATURE, "gpu")),
            tier: Tier::OneMinute,
            from_utc_ns: from,
            end_utc_ns: from + 10 * minute,
            at_utc_ns: utc_at_boot + i64::try_from(now).unwrap(),
        };
        let answer = OverviewReader::read(
            &history.index,
            &request,
            u64::MAX,
            &CancelFlag::new(),
            clock.as_ref(),
        )
        .unwrap();
        let OverviewAnswer::Bars(bars) = answer else {
            panic!("bars asked");
        };
        assert!(bars.bars.len() >= 6, "{bars:?}");
        for bar in &bars.bars {
            assert_eq!((bar.start_utc_ns + shift).rem_euclid(minute), 0, "{bar:?}");
            assert!(bar.covered_ns <= MINUTE, "{bar:?}");
            let index = (bar.start_utc_ns - utc_at_boot.div_euclid(minute) * minute) / minute;
            let expected = if index % 2 == 0 { 10.0 } else { 20.0 };
            assert_eq!(bar.mean(), Some(expected), "{bar:?}");
        }
        let held: u64 = bars.bars.iter().map(|bar| bar.covered_ns).sum();
        assert_eq!(held, now, "finalized and unfinalized minutes, once");
    }

    /// A wall clock step keeps the boot's grid: minutes cut later still
    /// start where the first pairing put whole minutes.
    #[test]
    fn a_wall_clock_step_keeps_the_minute_grid() {
        let (mut history, _ledger) = history(false);
        history.recorder.observe_wall_clock(0, UTC_AT_BOOT);
        history
            .recorder
            .observe_wall_clock(SECOND, UTC_AT_BOOT + 7 * i128::from(SECOND));
        assert_eq!(
            history.index.read().minute_grid(),
            Some(MinuteGrid::on_utc_minutes(0, UTC_AT_BOOT))
        );
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
        let clock = clock_at(now);
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

    /// A finished Overview minute the wall clock cannot place (here: its
    /// UTC is past what an Overview bucket holds) is never dropped in
    /// silence: it is reported as a gap of its series.
    #[test]
    fn an_overview_minute_that_cannot_be_placed_is_reported_as_a_gap() {
        let (mut history, _ledger) = history(false);
        let minute = i128::from(MINUTE);
        let past_the_buckets = (i128::from(i64::MAX) / minute + 1) * minute;
        history.recorder.observe_wall_clock(0, past_the_buckets);
        for second in 0..70 {
            history
                .recorder
                .ingest(one_gauge(second, 40.0), second * SECOND, second + 1);
        }
        let outcome = history.recorder.maintain(2 * MINUTE);
        let unplaced: Vec<&GapRecord> = outcome
            .new_gaps
            .iter()
            .filter(|gap| gap.reason == "overview_unplaced")
            .collect();
        assert_eq!(unplaced.len(), 1, "{:?}", outcome.new_gaps);
        assert_eq!(
            (unplaced[0].start_boot_ns, unplaced[0].end_boot_ns),
            (0, MINUTE)
        );
        assert_eq!(
            unplaced[0].key,
            Some(metric_key("gpu:0", ids::GPU_TEMPERATURE, "gpu"))
        );
        assert_eq!(outcome.gaps_added, 1);
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
