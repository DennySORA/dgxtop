//! One history series: raw gauge steps and exact counter intervals kept
//! apart, segments and gaps. Finished minutes (rollups and bridges) are cut
//! from the raw data once and handed on as archive rows, which feed the
//! Overview's sizes and the archive (D-82); the series keeps none of them
//! (D-90).
//!
//! The raw floor says what is still available: raw history is complete from
//! `raw_floor_ns` on. It is bucket aligned and never passes
//! `finalized_through_ns`, so evicting raw data never loses data whose
//! minute was not handed on yet. Minutes follow the boot's [`MinuteGrid`]
//! (whole UTC minutes); until it is known nothing is final, so no minute is
//! ever cut on a grid that would move. Raw history still ages by boot time
//! meanwhile (retention and memory pressure do not wait for the wall
//! clock): what it drops then has no minute and never will, so it is
//! closed (later samples there are late, minutes begin after it).

use dgxtop_core::budget::{ByteLedger, CapacityError, Partition};
use dgxtop_core::identity::MetricKey;
use dgxtop_core::metric::{CanonicalUnit, Scalar};

use super::chunk::ChunkLog;
use super::gaps::GapList;
use super::grid::MinuteGrid;
use super::output::{ArchiveRow, DropReason, HistoryReport};
use super::records::{BUCKET_NS, CounterSample, GaugeBucket, GaugeSample};
use super::rollup::{self, CounterRollup, RollupSpan};
use super::segment::{SegmentIdentity, SegmentTable};
use super::steps::{RawGauge, StepIter};
use dgxtop_core::budget::accounted::Account;

/// Most minute buckets one finalization pass may produce per series.
pub(crate) const MAX_FINALIZE_BUCKETS: u64 = 1440;
/// Gap entries retained per series.
pub(crate) const MAX_SERIES_GAPS: usize = 256;
/// Bookkeeping bytes of one series besides its key strings.
const SERIES_BASE_BYTES: u64 = 2048;

/// Raw gauge records.
pub(crate) struct GaugeTrack {
    pub raw: ChunkLog<GaugeSample>,
    pub carry: Option<GaugeSample>,
    pub raw_floor_ns: u64,
    pub last: Option<GaugeSample>,
}

/// Raw counter intervals.
pub(crate) struct CounterTrack {
    pub raw: ChunkLog<CounterSample>,
    pub raw_floor_ns: u64,
    pub last_end_ns: Option<u64>,
}

/// Rollups computed for one finalization pass, not handed on yet.
pub(crate) struct Finalization {
    target_ns: u64,
    gauge_buckets: Vec<GaugeBucket>,
    counter: CounterRollup,
}

/// Which raw track capacity eviction should shorten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawTrack {
    Gauge(u64),
    Counter(u64),
}

/// Raw records dropped by one capacity eviction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RawEviction {
    pub records: usize,
    /// The time the dropped records held when no rollup summarizes it
    /// (only before the grid is known): lost history, shown as a gap.
    pub unsummarized: Option<(u64, u64)>,
}

pub(crate) struct Series {
    pub key: MetricKey,
    pub segments: SegmentTable,
    pub gauge: GaugeTrack,
    pub counter: CounterTrack,
    pub gaps: GapList,
    pub finalized_through_ns: u64,
    /// Whether any finalization ran; until then `finalized_through_ns` is
    /// only the creation minute and nothing before it is final.
    finalized_any: bool,
    pub last_activity_ns: u64,
    _overhead: Account,
}

/// When a series opens: the time of the append that creates it and the
/// commit time it is created at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SeriesOpening {
    pub first_time_ns: u64,
    pub now_ns: u64,
}

/// Accounted bytes of one series' fixed state.
pub(crate) fn series_overhead_bytes(key: &MetricKey) -> u64 {
    let strings = key.host_id.as_str().len()
        + key.boot_id.as_str().len()
        + key.entity_id.as_str().len()
        + key.metric_id.as_str().len()
        + key.scope.as_str().len();
    let size = std::mem::size_of::<Series>().saturating_add(strings);
    SERIES_BASE_BYTES.saturating_add(u64::try_from(size).unwrap_or(u64::MAX))
}

impl Series {
    pub(crate) fn new(
        key: MetricKey,
        opening: SeriesOpening,
        grid: Option<MinuteGrid>,
        ledger: &ByteLedger,
    ) -> Result<Self, CapacityError> {
        let SeriesOpening {
            first_time_ns,
            now_ns,
        } = opening;
        let mut overhead = Account::new(ledger, Partition::Aggregates);
        overhead.try_grow(series_overhead_bytes(&key))?;
        Ok(Self {
            key,
            segments: SegmentTable::new(ledger),
            gauge: GaugeTrack {
                raw: ChunkLog::new(ledger, Partition::RawHistory),
                carry: None,
                raw_floor_ns: 0,
                last: None,
            },
            counter: CounterTrack {
                raw: ChunkLog::new(ledger, Partition::RawHistory),
                raw_floor_ns: 0,
                last_end_ns: None,
            },
            gaps: GapList::new(ledger, MAX_SERIES_GAPS),
            finalized_through_ns: grid.map_or(first_time_ns, |grid| grid.floor(first_time_ns)),
            finalized_any: false,
            last_activity_ns: now_ns,
            _overhead: overhead,
        })
    }

    /// Moves the series onto the boot's minutes once they are known (called
    /// once, when the grid is fixed): the creation minute moves onto them
    /// (nothing was final, so no row is cut again).
    pub(crate) fn settle_on_grid(&mut self, grid: MinuteGrid) {
        debug_assert!(!self.finalized_any, "a minute was final before the grid");
        if !self.finalized_any {
            self.finalized_through_ns = grid.floor(self.finalized_through_ns);
        }
    }

    /// Whether a gauge sample (or cut) at `time_ns` may be appended.
    pub(crate) fn admit_sample(&self, time_ns: u64) -> Result<(), DropReason> {
        if time_ns < self.finalized_through_ns {
            return Err(DropReason::Late);
        }
        if self.gauge.last.is_some_and(|last| time_ns <= last.time_ns) {
            return Err(DropReason::OutOfOrder);
        }
        Ok(())
    }

    /// Lets an interval that started before the series' first sample in (a
    /// batch applies its gauge first, which would otherwise make an interval
    /// ending on that minute boundary look late) while no minute is final.
    pub(crate) fn reach_back_to(&mut self, time_ns: u64, grid: Option<MinuteGrid>) {
        if !self.finalized_any {
            let start = grid.map_or(time_ns, |grid| grid.floor(time_ns));
            self.finalized_through_ns = self.finalized_through_ns.min(start);
        }
    }

    /// Whether a counter interval may be appended without double counting.
    pub(crate) fn admit_interval(&self, start_ns: u64, end_ns: u64) -> Result<(), DropReason> {
        if end_ns <= self.finalized_through_ns {
            return Err(DropReason::Late);
        }
        if self.counter.last_end_ns.is_some_and(|last| start_ns < last) {
            return Err(DropReason::Overlap);
        }
        Ok(())
    }

    /// A cut is only stored after a valid value (consecutive cuts add nothing).
    pub(crate) fn cut_needed(&self) -> bool {
        self.gauge.last.is_some_and(|last| last.value.is_some())
    }

    pub(crate) fn segment(
        &mut self,
        identity: SegmentIdentity,
        unit: CanonicalUnit,
        started_ns: u64,
    ) -> Result<u32, DropReason> {
        self.segments
            .resolve(&self.key, identity, unit, started_ns)
            .map_err(|error| match error {
                super::segment::SegmentError::Limit => DropReason::SegmentLimit,
                super::segment::SegmentError::Capacity(_) => DropReason::Capacity,
            })
    }

    /// Segment ordinal of the newest gauge record (cuts inherit it).
    pub(crate) fn last_gauge_segment(&self) -> u32 {
        self.gauge.last.map_or(0, |last| last.segment)
    }

    pub(crate) fn append_sample(
        &mut self,
        record: GaugeSample,
        now_ns: u64,
    ) -> Result<(), DropReason> {
        self.gauge
            .raw
            .try_push(record)
            .map_err(|_| DropReason::Capacity)?;
        self.gauge.last = Some(record);
        self.last_activity_ns = self.last_activity_ns.max(now_ns);
        Ok(())
    }

    pub(crate) fn append_interval(
        &mut self,
        record: CounterSample,
        now_ns: u64,
    ) -> Result<(), DropReason> {
        self.counter
            .raw
            .try_push(record)
            .map_err(|_| DropReason::Capacity)?;
        self.counter.last_end_ns = Some(record.end_ns);
        self.last_activity_ns = self.last_activity_ns.max(now_ns);
        Ok(())
    }

    /// Computes the rollups for the minutes of `grid` that ended before
    /// `horizon_ns` (the caller has the grid only once it is fixed).
    pub(crate) fn plan_finalization(
        &self,
        horizon_ns: u64,
        grid: MinuteGrid,
    ) -> Option<Finalization> {
        let horizon = grid.floor(horizon_ns);
        let from = self.finalized_through_ns;
        if horizon <= from {
            return None;
        }
        let most = grid.floor(from.saturating_add(MAX_FINALIZE_BUCKETS.saturating_mul(BUCKET_NS)));
        let target = horizon.min(most);
        let gauge_slices = self.gauge.raw.slices();
        let raw = RawGauge {
            slices: &gauge_slices,
            carry: self.gauge.carry,
            floor_ns: self.gauge.raw_floor_ns,
        };
        let steps =
            StepIter::covering(raw, from, u64::MAX).take_while(|step| step.start_ns < target);
        let samples = gauge_slices
            .iter_from(gauge_slices.seek_start(from.max(self.gauge.raw_floor_ns)))
            .take_while(|record| record.time_ns < target);
        let span = RollupSpan {
            from_ns: from,
            to_ns: target,
            grid,
        };
        let gauge_buckets = rollup::gauge_buckets(steps, samples, span);
        let counter_slices = self.counter.raw.slices();
        let intervals = counter_slices
            .iter_from(counter_slices.seek_end_after(from))
            .take_while(|interval| interval.end_ns <= target);
        let counter = rollup::counter_rollup(intervals, span);
        Some(Finalization {
            target_ns: target,
            gauge_buckets,
            counter,
        })
    }

    /// The minutes not finalized yet, from the raw steps up to `now_ns`
    /// (D-82: the Overview's latest bar holds them too). Computed for a
    /// read, never stored: later samples may still change them.
    pub(crate) fn provisional_gauge_buckets(
        &self,
        now_ns: u64,
        grid: MinuteGrid,
    ) -> Vec<GaugeBucket> {
        let from = self.finalized_through_ns;
        if now_ns <= from {
            return Vec::new();
        }
        let gauge_slices = self.gauge.raw.slices();
        let raw = RawGauge {
            slices: &gauge_slices,
            carry: self.gauge.carry,
            floor_ns: self.gauge.raw_floor_ns,
        };
        let steps = StepIter::covering(raw, from, now_ns).take_while(|step| step.start_ns < now_ns);
        let samples = gauge_slices
            .iter_from(gauge_slices.seek_start(from.max(self.gauge.raw_floor_ns)))
            .take_while(|record| record.time_ns < now_ns);
        let span = RollupSpan {
            from_ns: from,
            to_ns: now_ns,
            grid,
        };
        rollup::gauge_buckets(steps, samples, span)
    }

    /// Hands computed rollups on as finished rows (the archive and the
    /// Overview's sizes take them, D-82); the series keeps none (D-90).
    pub(crate) fn commit_finalization(
        &mut self,
        finalization: Finalization,
        report: &mut HistoryReport,
    ) {
        for bucket in finalization.gauge_buckets {
            if let Some(segment) = self.segments.get(bucket.segment) {
                report.archive_rows.push(ArchiveRow::GaugeBucket {
                    segment: segment.clone(),
                    bucket,
                });
            }
        }
        for bucket in finalization.counter.buckets {
            if let Some(segment) = self.segments.get(bucket.segment) {
                report.archive_rows.push(ArchiveRow::CounterBucket {
                    segment: segment.clone(),
                    bucket,
                });
            }
        }
        for bridge in finalization.counter.bridges {
            if let Some(segment) = self.segments.get(bridge.segment) {
                report.archive_rows.push(ArchiveRow::Bridge {
                    segment: segment.clone(),
                    bridge,
                });
            }
        }
        self.finalized_through_ns = finalization.target_ns;
        self.finalized_any = true;
    }

    /// Moves the gauge raw floor (bucket aligned, never past the finalized
    /// horizon), keeping the newest evicted record as the carry so the step
    /// crossing the floor stays available.
    fn advance_gauge_raw_floor(&mut self, target_ns: u64, grid: Option<MinuteGrid>) -> usize {
        let target = self.raw_floor_target(target_ns, grid);
        if target <= self.gauge.raw_floor_ns {
            return 0;
        }
        let old_floor = self.gauge.raw_floor_ns;
        let newest_evicted = {
            let slices = self.gauge.raw.slices();
            slices
                .last_starting_before(target)
                .copied()
                .filter(|record| record.time_ns >= old_floor)
                .or(self.gauge.carry)
        };
        self.gauge.carry = newest_evicted
            .filter(|record| record.value.is_some() && record.stale_deadline_ns > target);
        self.gauge.raw_floor_ns = target;
        if grid.is_none() {
            self.close_unsummarized(target);
        }
        self.gauge
            .raw
            .drop_front_while(|last| last.time_ns < target)
    }

    fn advance_counter_raw_floor(&mut self, target_ns: u64, grid: Option<MinuteGrid>) -> usize {
        let target = self.raw_floor_target(target_ns, grid);
        if target <= self.counter.raw_floor_ns {
            return 0;
        }
        self.counter.raw_floor_ns = target;
        if grid.is_none() {
            self.close_unsummarized(target);
        }
        self.counter
            .raw
            .drop_front_while(|last| last.end_ns <= target)
    }

    /// Where a raw floor may move for `target_ns`: on the grid, a minute
    /// start no later than the finalized horizon (what goes was handed on
    /// as finished minutes); before the grid, `target_ns` itself, because no minute
    /// can be cut yet and raw history must not wait for the wall clock.
    fn raw_floor_target(&self, target_ns: u64, grid: Option<MinuteGrid>) -> u64 {
        match grid {
            Some(grid) => grid.floor(target_ns).min(self.finalized_through_ns),
            None => target_ns,
        }
    }

    /// Raw data dropped before the grid is known has no minute and never
    /// will: a sample there is late from now on.
    fn close_unsummarized(&mut self, floor_ns: u64) {
        self.finalized_through_ns = self.finalized_through_ns.max(floor_ns);
    }

    /// Normal raw retention.
    pub(crate) fn advance_raw_floors(&mut self, target_ns: u64, grid: Option<MinuteGrid>) -> usize {
        self.advance_gauge_raw_floor(target_ns, grid)
            .saturating_add(self.advance_counter_raw_floor(target_ns, grid))
    }

    /// Gap retention: gaps ending by `target_ns` (on the grid, by its
    /// minute) are dropped.
    pub(crate) fn age_gaps(&mut self, target_ns: u64, grid: Option<MinuteGrid>) -> usize {
        let target = grid.map_or(target_ns, |grid| grid.floor(target_ns));
        self.gaps.drop_ending_by(target)
    }

    /// Whether appending under `identity` would open a new segment.
    pub(crate) fn needs_new_segment(
        &self,
        identity: &SegmentIdentity,
        unit: CanonicalUnit,
    ) -> bool {
        self.segments.would_create(identity, unit)
    }

    /// The raw track whose oldest chunk eviction drops next, with the
    /// floor that drops it: on the grid only a chunk whose minutes are
    /// final; before the grid any chunk (nothing can be final yet).
    fn evictable_raw_track(&self, grid: Option<MinuteGrid>) -> Option<RawTrack> {
        let finalized = self.finalized_through_ns;
        let summarized = |floor: &u64| grid.is_none() || *floor <= finalized;
        let gauge = self
            .gauge
            .raw
            .oldest_chunk()
            .and_then(<[GaugeSample]>::last)
            .map(|last| match grid {
                Some(grid) => grid.end_of(last.time_ns),
                None => last.time_ns.saturating_add(1),
            })
            .filter(summarized)
            .map(RawTrack::Gauge);
        let counter = self
            .counter
            .raw
            .oldest_chunk()
            .and_then(<[CounterSample]>::last)
            .map(|last| grid.map_or(last.end_ns, |grid| grid.ceil(last.end_ns)))
            .filter(summarized)
            .map(RawTrack::Counter);
        match (gauge, counter) {
            (Some(RawTrack::Gauge(g)), Some(RawTrack::Counter(c))) if c < g => counter,
            (Some(_), _) => gauge,
            (None, other) => other,
        }
    }

    /// Floor the next capacity eviction of raw data would set (oldest first).
    pub(crate) fn oldest_evictable_raw_ns(&self, grid: Option<MinuteGrid>) -> Option<u64> {
        self.evictable_raw_track(grid).map(|track| match track {
            RawTrack::Gauge(floor) | RawTrack::Counter(floor) => floor,
        })
    }

    /// Drops the oldest evictable raw chunk.
    pub(crate) fn evict_oldest_raw(&mut self, grid: Option<MinuteGrid>) -> RawEviction {
        let (from, records, to) = match self.evictable_raw_track(grid) {
            Some(RawTrack::Gauge(floor)) => {
                let from = self.gauge.raw.first().map_or(floor, |first| first.time_ns);
                (from, self.advance_gauge_raw_floor(floor, grid), floor)
            }
            Some(RawTrack::Counter(floor)) => {
                let from = self
                    .counter
                    .raw
                    .first()
                    .map_or(floor, |first| first.start_ns);
                (from, self.advance_counter_raw_floor(floor, grid), floor)
            }
            None => (0, 0, 0),
        };
        let unsummarized = (grid.is_none() && records > 0).then_some((from, to));
        RawEviction {
            records,
            unsummarized,
        }
    }

    /// Start of the oldest gap capacity eviction could drop.
    pub(crate) fn oldest_gap_ns(&self) -> Option<u64> {
        self.gaps.oldest_start()
    }

    /// Drops the oldest gap (capacity eviction).
    pub(crate) fn evict_oldest_gap(&mut self) -> bool {
        self.gaps.drop_oldest()
    }

    /// Retires segments no retained record references any more.
    pub(crate) fn retire_unreferenced_segments(&mut self) {
        let floor = [
            self.segments.current_floor(),
            self.gauge.raw.first().map(|record| record.segment),
            self.gauge.carry.map(|record| record.segment),
            self.counter.raw.first().map(|record| record.segment),
        ]
        .into_iter()
        .flatten()
        .min();
        if let Some(floor) = floor {
            self.segments.retire_before(floor);
        }
    }

    pub(crate) fn holds_no_data(&self) -> bool {
        self.gauge.raw.is_empty()
            && self.gauge.carry.is_none()
            && self.counter.raw.is_empty()
            && self.gaps.is_empty()
    }
}

/// Builds a gauge record from a scalar value (or a cut when `None`).
pub(crate) fn gauge_record(
    time_ns: u64,
    deadline_ns: u64,
    value: Option<Scalar>,
    segment: u32,
) -> GaugeSample {
    GaugeSample {
        time_ns,
        stale_deadline_ns: deadline_ns,
        value,
        segment,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::history::HistoryMethod;
    use dgxtop_core::metric::ids;
    use dgxtop_core::quality::SampleKind;
    use dgxtop_core::sampling::stale_deadline;
    use dgxtop_core::testing::{metric_key, source_id};

    const SECOND: u64 = 1_000_000_000;
    const GRID: MinuteGrid = MinuteGrid::BOOT_CLOCK;
    const AT_BOOT: SeriesOpening = SeriesOpening {
        first_time_ns: 0,
        now_ns: 0,
    };

    fn series(ledger: &ByteLedger) -> Series {
        let key = metric_key("cpu.package.0", ids::CPU_TEMPERATURE_PACKAGE, "package:0");
        Series::new(key, AT_BOOT, Some(GRID), ledger).unwrap()
    }

    fn gauge_identity() -> SegmentIdentity {
        SegmentIdentity {
            source_id: source_id("hwmon:coretemp:temp1"),
            source_generation: 1,
            policy_revision: 1,
            sample_kind: SampleKind::Measured,
            method: HistoryMethod::TimeWeightedStep,
        }
    }

    fn push_gauge(series: &mut Series, seconds: u64, value: f64) {
        let segment = series
            .segment(gauge_identity(), CanonicalUnit::DegC, seconds * SECOND)
            .unwrap();
        let time = seconds * SECOND;
        series.admit_sample(time).unwrap();
        series
            .append_sample(
                gauge_record(
                    time,
                    stale_deadline(time, 3, SECOND),
                    Some(Scalar::from_f64(value).unwrap()),
                    segment,
                ),
                time,
            )
            .unwrap();
    }

    #[test]
    fn finalization_rolls_up_completed_minutes_and_rejects_late_samples() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut series = series(&ledger);
        for second in 0..130 {
            push_gauge(&mut series, second, 50.0);
        }
        let plan = series.plan_finalization(125 * SECOND, GRID).unwrap();
        assert_eq!(plan.target_ns, 120 * SECOND);
        assert_eq!(plan.gauge_buckets.len(), 2);
        let first = plan.gauge_buckets[0];
        assert_eq!(first.covered_ns, 60 * SECOND);
        assert_eq!(first.sample_count, 60);
        let mut report = HistoryReport::default();
        series.commit_finalization(plan, &mut report);
        assert_eq!(report.archive_rows.len(), 2);
        assert_eq!(series.finalized_through_ns, 120 * SECOND);
        assert_eq!(series.admit_sample(119 * SECOND), Err(DropReason::Late));
        assert_eq!(
            series.admit_sample(129 * SECOND),
            Err(DropReason::OutOfOrder)
        );
        assert!(series.plan_finalization(125 * SECOND, GRID).is_none());
    }

    #[test]
    fn raw_floor_never_passes_finalized_and_keeps_a_carry() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut series = series(&ledger);
        for second in 0..200 {
            push_gauge(
                &mut series,
                second,
                f64::from(u32::try_from(second).unwrap()),
            );
        }
        assert_eq!(
            series.advance_raw_floors(150 * SECOND, Some(GRID)),
            0,
            "nothing finalized yet"
        );
        let plan = series.plan_finalization(190 * SECOND, GRID).unwrap();
        let mut report = HistoryReport::default();
        series.commit_finalization(plan, &mut report);
        assert!(series.advance_raw_floors(150 * SECOND, Some(GRID)) > 0);
        assert_eq!(series.gauge.raw_floor_ns, 120 * SECOND);
        let carry = series.gauge.carry.unwrap();
        assert_eq!(carry.time_ns, 119 * SECOND);
    }

    #[test]
    fn capacity_eviction_targets_oldest_finalized_raw() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut series = series(&ledger);
        for second in 0..400 {
            push_gauge(&mut series, second, 1.0);
        }
        assert!(series.oldest_evictable_raw_ns(Some(GRID)).is_none());
        let plan = series.plan_finalization(400 * SECOND, GRID).unwrap();
        let mut report = HistoryReport::default();
        series.commit_finalization(plan, &mut report);
        let floor = series.oldest_evictable_raw_ns(Some(GRID)).unwrap();
        assert_eq!(floor, 120 * SECOND);
        let eviction = series.evict_oldest_raw(Some(GRID));
        assert_eq!(eviction.records, 64);
        assert_eq!(eviction.unsummarized, None, "the minutes were handed on");
        assert_eq!(series.gauge.raw_floor_ns, 120 * SECOND);
        assert_eq!(
            ledger.used(Partition::Aggregates),
            series_overhead_bytes(&series.key) + 320,
            "finished minutes are not kept: the overhead and one segment"
        );
    }

    /// Under memory pressure the oldest raw chunk goes even before the
    /// grid is known; the time it held is reported as lost.
    #[test]
    fn capacity_eviction_drops_the_oldest_raw_chunk_before_the_grid_is_known() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let key = metric_key("cpu.package.0", ids::CPU_TEMPERATURE_PACKAGE, "package:0");
        let mut series = Series::new(key, AT_BOOT, None, &ledger).unwrap();
        for second in 0..200 {
            push_gauge(&mut series, second, 1.0);
        }
        // The first chunk holds seconds 0 to 63.
        let floor = 63 * SECOND + 1;
        assert_eq!(series.oldest_evictable_raw_ns(None), Some(floor));
        let eviction = series.evict_oldest_raw(None);
        assert_eq!(eviction.records, 64);
        assert_eq!(eviction.unsummarized, Some((0, floor)));
        assert_eq!(series.gauge.raw_floor_ns, floor);
        assert_eq!(series.admit_sample(63 * SECOND), Err(DropReason::Late));
    }

    #[test]
    fn intervals_must_not_overlap_or_end_in_finalized_time() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut series = series(&ledger);
        series.admit_interval(0, 10).unwrap();
        series
            .append_interval(
                CounterSample {
                    start_ns: 0,
                    end_ns: 10,
                    delta: Scalar::from_u64(5),
                    segment: 0,
                },
                10,
            )
            .unwrap();
        assert_eq!(series.admit_interval(5, 20), Err(DropReason::Overlap));
        assert!(series.admit_interval(10, 20).is_ok());
        series.finalized_through_ns = 60 * SECOND;
        assert_eq!(
            series.admit_interval(30, 60 * SECOND),
            Err(DropReason::Late)
        );
        assert!(series.admit_interval(59 * SECOND, 61 * SECOND).is_ok());
    }

    #[test]
    fn interval_before_the_creation_minute_is_admitted_until_a_minute_is_final() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let key = metric_key("netif:ns:2", ids::NET_RX_BPS, "interface");
        let mut series = Series::new(
            key,
            SeriesOpening {
                first_time_ns: 60 * SECOND,
                now_ns: 60 * SECOND,
            },
            Some(GRID),
            &ledger,
        )
        .unwrap();
        let start = 58 * SECOND + 581_000_000;
        assert_eq!(
            series.admit_interval(start, 60 * SECOND),
            Err(DropReason::Late)
        );
        series.reach_back_to(start, Some(GRID));
        assert_eq!(series.finalized_through_ns, 0);
        assert!(series.admit_interval(start, 60 * SECOND).is_ok());

        let plan = series.plan_finalization(120 * SECOND, GRID).unwrap();
        series.commit_finalization(plan, &mut HistoryReport::default());
        series.reach_back_to(start, Some(GRID));
        assert_eq!(series.finalized_through_ns, 120 * SECOND);
        assert_eq!(
            series.admit_interval(start, 60 * SECOND),
            Err(DropReason::Late)
        );
    }

    #[test]
    fn overhead_is_accounted_and_released() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let series = series(&ledger);
        assert!(ledger.used(Partition::Aggregates) >= series_overhead_bytes(&series.key));
        drop(series);
        assert_eq!(ledger.used(Partition::Aggregates), 0);
    }
}
