//! One history series: raw gauge steps and exact counter intervals kept
//! apart, minute rollups derived from them, bridges, segments and gaps.
//!
//! Floors describe what is still available: raw history is complete from
//! `raw_floor_ns` on (older raw data is summarized by finalized rollups), and
//! rollups are complete from `rollup_floor_ns` on. Floors are bucket aligned
//! and the raw floor never passes `finalized_through_ns`, so evicting raw data
//! never loses data that has no rollup yet.

use dgxtop_core::budget::{ByteLedger, CapacityError, Partition};
use dgxtop_core::identity::MetricKey;
use dgxtop_core::metric::{CanonicalUnit, MetricDescriptor, Scalar};

use super::chunk::ChunkLog;
use super::gaps::GapList;
use super::output::{ArchiveRow, DropReason, HistoryReport};
use super::records::{
    BUCKET_NS, CounterBucket, CounterSample, GaugeBucket, GaugeSample, ceil_bucket, floor_bucket,
};
use super::rollup::{self, CounterRollup};
use super::segment::{SegmentIdentity, SegmentTable};
use super::steps::{RawGauge, StepIter};
use dgxtop_core::budget::accounted::Account;

/// Most minute buckets one finalization pass may produce per series.
pub(crate) const MAX_FINALIZE_BUCKETS: u64 = 1440;
/// Gap entries retained per series.
pub(crate) const MAX_SERIES_GAPS: usize = 256;
/// Bookkeeping bytes of one series besides its key strings.
const SERIES_BASE_BYTES: u64 = 2048;

/// Raw gauge records and their rollups.
pub(crate) struct GaugeTrack {
    pub raw: ChunkLog<GaugeSample>,
    pub carry: Option<GaugeSample>,
    pub raw_floor_ns: u64,
    pub last: Option<GaugeSample>,
    pub buckets: ChunkLog<GaugeBucket>,
    pub rollup_floor_ns: u64,
}

/// Raw counter intervals, their buckets and bridges.
pub(crate) struct CounterTrack {
    pub raw: ChunkLog<CounterSample>,
    pub raw_floor_ns: u64,
    pub last_end_ns: Option<u64>,
    pub buckets: ChunkLog<CounterBucket>,
    pub bridges: ChunkLog<CounterSample>,
    pub rollup_floor_ns: u64,
}

impl CounterTrack {
    pub(crate) fn holds_data(&self) -> bool {
        !(self.raw.is_empty() && self.buckets.is_empty() && self.bridges.is_empty())
    }
}

/// Rollups computed for one finalization pass, not yet stored.
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

/// Floors moved by one rollup eviction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RollupEviction {
    pub records: usize,
    pub from_ns: u64,
    pub to_ns: u64,
}

pub(crate) struct Series {
    pub key: MetricKey,
    pub descriptor: &'static MetricDescriptor,
    pub segments: SegmentTable,
    pub gauge: GaugeTrack,
    pub counter: CounterTrack,
    pub gaps: GapList,
    pub finalized_through_ns: u64,
    /// Whether any finalization ran; until then `finalized_through_ns` is
    /// only the creation minute and nothing before it is final.
    finalized_any: bool,
    pub last_activity_ns: u64,
    /// Rollups were shortened because the aggregates partition was full.
    pub capacity_truncated: bool,
    _overhead: Account,
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
        descriptor: &'static MetricDescriptor,
        first_time_ns: u64,
        now_ns: u64,
        ledger: &ByteLedger,
    ) -> Result<Self, CapacityError> {
        let mut overhead = Account::new(ledger, Partition::Aggregates);
        overhead.try_grow(series_overhead_bytes(&key))?;
        Ok(Self {
            key,
            descriptor,
            segments: SegmentTable::new(ledger),
            gauge: GaugeTrack {
                raw: ChunkLog::new(ledger, Partition::RawHistory),
                carry: None,
                raw_floor_ns: 0,
                last: None,
                buckets: ChunkLog::new(ledger, Partition::Aggregates),
                rollup_floor_ns: 0,
            },
            counter: CounterTrack {
                raw: ChunkLog::new(ledger, Partition::RawHistory),
                raw_floor_ns: 0,
                last_end_ns: None,
                buckets: ChunkLog::new(ledger, Partition::Aggregates),
                bridges: ChunkLog::new(ledger, Partition::Aggregates),
                rollup_floor_ns: 0,
            },
            gaps: GapList::new(ledger, MAX_SERIES_GAPS),
            finalized_through_ns: floor_bucket(first_time_ns),
            finalized_any: false,
            last_activity_ns: now_ns,
            capacity_truncated: false,
            _overhead: overhead,
        })
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
    pub(crate) fn reach_back_to(&mut self, time_ns: u64) {
        if !self.finalized_any {
            self.finalized_through_ns = self.finalized_through_ns.min(floor_bucket(time_ns));
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

    /// Computes the rollups for the minutes that ended before `horizon_ns`.
    pub(crate) fn plan_finalization(&self, horizon_ns: u64) -> Option<Finalization> {
        let horizon = floor_bucket(horizon_ns);
        let from = self.finalized_through_ns;
        if horizon <= from {
            return None;
        }
        let target =
            horizon.min(from.saturating_add(MAX_FINALIZE_BUCKETS.saturating_mul(BUCKET_NS)));
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
        let gauge_buckets = rollup::gauge_buckets(steps, samples, from, target);
        let counter_slices = self.counter.raw.slices();
        let intervals = counter_slices
            .iter_from(counter_slices.seek_end_after(from))
            .take_while(|interval| interval.end_ns <= target);
        let counter = rollup::counter_rollup(intervals, from, target);
        Some(Finalization {
            target_ns: target,
            gauge_buckets,
            counter,
        })
    }

    /// Aggregates bytes storing `finalization` will reserve.
    pub(crate) fn finalization_cost(&self, finalization: &Finalization) -> u64 {
        self.gauge
            .buckets
            .cost_of_pushes(finalization.gauge_buckets.len())
            .saturating_add(
                self.counter
                    .buckets
                    .cost_of_pushes(finalization.counter.buckets.len()),
            )
            .saturating_add(
                self.counter
                    .bridges
                    .cost_of_pushes(finalization.counter.bridges.len()),
            )
    }

    /// Stores computed rollups. Rows are produced only when an archive is
    /// configured; a bucket that cannot be stored is reported as lost time.
    pub(crate) fn commit_finalization(
        &mut self,
        finalization: Finalization,
        archive: bool,
        report: &mut HistoryReport,
    ) -> Vec<(u64, u64)> {
        let mut lost = Vec::new();
        for bucket in finalization.gauge_buckets {
            if self.gauge.buckets.try_push(bucket).is_err() {
                report.gauge_drops.record(DropReason::Capacity);
                lost.push((bucket.start_ns, bucket.start_ns.saturating_add(BUCKET_NS)));
            } else if archive && let Some(segment) = self.segments.get(bucket.segment) {
                report.archive_rows.push(ArchiveRow::GaugeBucket {
                    segment: segment.clone(),
                    bucket,
                });
            }
        }
        for bucket in finalization.counter.buckets {
            if self.counter.buckets.try_push(bucket).is_err() {
                report.interval_drops.record(DropReason::Capacity);
                lost.push((bucket.start_ns, bucket.start_ns.saturating_add(BUCKET_NS)));
            } else if archive && let Some(segment) = self.segments.get(bucket.segment) {
                report.archive_rows.push(ArchiveRow::CounterBucket {
                    segment: segment.clone(),
                    bucket,
                });
            }
        }
        for bridge in finalization.counter.bridges {
            if self.counter.bridges.try_push(bridge).is_err() {
                report.interval_drops.record(DropReason::Capacity);
                lost.push((bridge.start_ns, bridge.end_ns));
            } else if archive && let Some(segment) = self.segments.get(bridge.segment) {
                report.archive_rows.push(ArchiveRow::Bridge {
                    segment: segment.clone(),
                    bridge,
                });
            }
        }
        self.finalized_through_ns = finalization.target_ns;
        self.finalized_any = true;
        lost
    }

    /// Moves the gauge raw floor (bucket aligned, never past the finalized
    /// horizon), keeping the newest evicted record as the carry so the step
    /// crossing the floor stays available.
    fn advance_gauge_raw_floor(&mut self, target_ns: u64) -> usize {
        let target = floor_bucket(target_ns).min(self.finalized_through_ns);
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
        self.gauge
            .raw
            .drop_front_while(|last| last.time_ns < target)
    }

    fn advance_counter_raw_floor(&mut self, target_ns: u64) -> usize {
        let target = floor_bucket(target_ns).min(self.finalized_through_ns);
        if target <= self.counter.raw_floor_ns {
            return 0;
        }
        self.counter.raw_floor_ns = target;
        self.counter
            .raw
            .drop_front_while(|last| last.end_ns <= target)
    }

    /// Normal raw retention.
    pub(crate) fn advance_raw_floors(&mut self, target_ns: u64) -> usize {
        self.advance_gauge_raw_floor(target_ns)
            .saturating_add(self.advance_counter_raw_floor(target_ns))
    }

    fn advance_gauge_rollup_floor(&mut self, target: u64) -> usize {
        if target <= self.gauge.rollup_floor_ns {
            return 0;
        }
        self.gauge.rollup_floor_ns = target;
        self.gauge
            .buckets
            .drop_front_while(|last| last.start_ns < target)
    }

    fn advance_counter_rollup_floor(&mut self, target: u64) -> usize {
        if target <= self.counter.rollup_floor_ns {
            return 0;
        }
        self.counter.rollup_floor_ns = target;
        self.counter
            .buckets
            .drop_front_while(|last| last.start_ns < target)
            .saturating_add(
                self.counter
                    .bridges
                    .drop_front_while(|last| last.end_ns <= target),
            )
    }

    /// Normal rollup and gap retention. Once retention catches up with a
    /// capacity truncation, the truncation is no longer visible.
    pub(crate) fn advance_rollup_floors(&mut self, target_ns: u64) -> usize {
        let target = floor_bucket(target_ns);
        let dropped = self
            .advance_gauge_rollup_floor(target)
            .saturating_add(self.advance_counter_rollup_floor(target))
            .saturating_add(self.gaps.drop_ending_by(target));
        if target >= self.gauge.rollup_floor_ns.max(self.counter.rollup_floor_ns) {
            self.capacity_truncated = false;
        }
        dropped
    }

    /// Whether appending under `identity` would open a new segment.
    pub(crate) fn needs_new_segment(
        &self,
        identity: &SegmentIdentity,
        unit: CanonicalUnit,
    ) -> bool {
        self.segments.would_create(identity, unit)
    }

    fn evictable_raw_track(&self) -> Option<RawTrack> {
        let finalized = self.finalized_through_ns;
        let gauge = self
            .gauge
            .raw
            .oldest_chunk()
            .and_then(<[GaugeSample]>::last)
            .map(|last| floor_bucket(last.time_ns).saturating_add(BUCKET_NS))
            .filter(|floor| *floor <= finalized)
            .map(RawTrack::Gauge);
        let counter = self
            .counter
            .raw
            .oldest_chunk()
            .and_then(<[CounterSample]>::last)
            .map(|last| ceil_bucket(last.end_ns))
            .filter(|floor| *floor <= finalized)
            .map(RawTrack::Counter);
        match (gauge, counter) {
            (Some(RawTrack::Gauge(g)), Some(RawTrack::Counter(c))) if c < g => counter,
            (Some(_), _) => gauge,
            (None, other) => other,
        }
    }

    /// Floor the next capacity eviction of raw data would set (oldest first).
    pub(crate) fn oldest_evictable_raw_ns(&self) -> Option<u64> {
        self.evictable_raw_track().map(|track| match track {
            RawTrack::Gauge(floor) | RawTrack::Counter(floor) => floor,
        })
    }

    /// Drops the oldest evictable raw chunk; returns the records dropped.
    pub(crate) fn evict_oldest_raw(&mut self) -> usize {
        match self.evictable_raw_track() {
            Some(RawTrack::Gauge(floor)) => self.advance_gauge_raw_floor(floor),
            Some(RawTrack::Counter(floor)) => self.advance_counter_raw_floor(floor),
            None => 0,
        }
    }

    fn rollup_candidates(&self) -> [Option<(u64, u64)>; 3] {
        let gauge = self.gauge.buckets.oldest_chunk().and_then(|chunk| {
            Some((
                chunk.first()?.start_ns,
                chunk.last()?.start_ns.saturating_add(BUCKET_NS),
            ))
        });
        let counter = self.counter.buckets.oldest_chunk().and_then(|chunk| {
            Some((
                chunk.first()?.start_ns,
                chunk.last()?.start_ns.saturating_add(BUCKET_NS),
            ))
        });
        let bridges =
            self.counter.bridges.oldest_chunk().and_then(|chunk| {
                Some((chunk.first()?.start_ns, ceil_bucket(chunk.last()?.end_ns)))
            });
        [gauge, counter, bridges]
    }

    /// Start of the oldest rollup data or gap capacity eviction could drop.
    pub(crate) fn oldest_rollup_ns(&self) -> Option<u64> {
        self.rollup_candidates()
            .iter()
            .flatten()
            .map(|(start, _)| *start)
            .min()
            .or_else(|| self.gaps.oldest_start())
    }

    /// Shortens rollups by their oldest chunk (or drops the oldest gap).
    pub(crate) fn evict_oldest_rollup(&mut self) -> Option<RollupEviction> {
        let candidates = self.rollup_candidates();
        let oldest = candidates
            .iter()
            .enumerate()
            .filter_map(|(track, candidate)| candidate.map(|(start, floor)| (start, floor, track)))
            .min_by_key(|(start, _, _)| *start);
        let Some((_, floor, track)) = oldest else {
            return self.gaps.drop_oldest().then_some(RollupEviction {
                records: 1,
                from_ns: 0,
                to_ns: 0,
            });
        };
        let from = if track == 0 {
            self.gauge.rollup_floor_ns
        } else {
            self.counter.rollup_floor_ns
        };
        let records = if track == 0 {
            self.advance_gauge_rollup_floor(floor)
        } else {
            self.advance_counter_rollup_floor(floor)
        };
        self.capacity_truncated = true;
        Some(RollupEviction {
            records,
            from_ns: from,
            to_ns: floor,
        })
    }

    /// Retires segments no retained record references any more.
    pub(crate) fn retire_unreferenced_segments(&mut self) {
        let floor = [
            self.segments.current_floor(),
            self.gauge.raw.first().map(|record| record.segment),
            self.gauge.carry.map(|record| record.segment),
            self.gauge.buckets.first().map(|bucket| bucket.segment),
            self.counter.raw.first().map(|record| record.segment),
            self.counter.buckets.first().map(|bucket| bucket.segment),
            self.counter.bridges.first().map(|bridge| bridge.segment),
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
            && self.gauge.buckets.is_empty()
            && !self.counter.holds_data()
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
    use dgxtop_core::metric::{descriptor, ids};
    use dgxtop_core::quality::SampleKind;
    use dgxtop_core::sampling::stale_deadline;
    use dgxtop_core::testing::{metric_key, source_id};

    const SECOND: u64 = 1_000_000_000;

    fn series(ledger: &ByteLedger) -> Series {
        let key = metric_key("cpu.package.0", ids::CPU_TEMPERATURE_PACKAGE, "package:0");
        let descriptor = descriptor(ids::CPU_TEMPERATURE_PACKAGE).unwrap();
        Series::new(key, descriptor, 0, 0, ledger).unwrap()
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
        let plan = series.plan_finalization(125 * SECOND).unwrap();
        assert_eq!(plan.target_ns, 120 * SECOND);
        assert_eq!(plan.gauge_buckets.len(), 2);
        let mut report = HistoryReport::default();
        assert!(
            series
                .commit_finalization(plan, true, &mut report)
                .is_empty()
        );
        assert_eq!(report.archive_rows.len(), 2);
        assert_eq!(series.finalized_through_ns, 120 * SECOND);
        assert_eq!(series.admit_sample(119 * SECOND), Err(DropReason::Late));
        assert_eq!(
            series.admit_sample(129 * SECOND),
            Err(DropReason::OutOfOrder)
        );
        assert!(series.plan_finalization(125 * SECOND).is_none());
        let slices_bucket = series.gauge.buckets.first().copied().unwrap();
        assert_eq!(slices_bucket.covered_ns, 60 * SECOND);
        assert_eq!(slices_bucket.sample_count, 60);
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
            series.advance_raw_floors(150 * SECOND),
            0,
            "nothing finalized yet"
        );
        let plan = series.plan_finalization(190 * SECOND).unwrap();
        let mut report = HistoryReport::default();
        series.commit_finalization(plan, false, &mut report);
        assert!(series.advance_raw_floors(150 * SECOND) > 0);
        assert_eq!(series.gauge.raw_floor_ns, 120 * SECOND);
        let carry = series.gauge.carry.unwrap();
        assert_eq!(carry.time_ns, 119 * SECOND);
    }

    #[test]
    fn capacity_eviction_targets_oldest_finalized_raw_then_rollups() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut series = series(&ledger);
        for second in 0..400 {
            push_gauge(&mut series, second, 1.0);
        }
        assert!(series.oldest_evictable_raw_ns().is_none());
        let plan = series.plan_finalization(400 * SECOND).unwrap();
        let mut report = HistoryReport::default();
        series.commit_finalization(plan, false, &mut report);
        let floor = series.oldest_evictable_raw_ns().unwrap();
        assert_eq!(floor, 120 * SECOND);
        assert_eq!(series.evict_oldest_raw(), 64);
        assert_eq!(series.gauge.raw_floor_ns, 120 * SECOND);
        let eviction = series.evict_oldest_rollup().unwrap();
        assert_eq!(eviction.from_ns, 0);
        assert_eq!(eviction.to_ns, 360 * SECOND);
        assert!(series.capacity_truncated);
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
        let descriptor = descriptor(ids::NET_RX_BPS).unwrap();
        let mut series = Series::new(key, descriptor, 60 * SECOND, 60 * SECOND, &ledger).unwrap();
        let start = 58 * SECOND + 581_000_000;
        assert_eq!(
            series.admit_interval(start, 60 * SECOND),
            Err(DropReason::Late)
        );
        series.reach_back_to(start);
        assert_eq!(series.finalized_through_ns, 0);
        assert!(series.admit_interval(start, 60 * SECOND).is_ok());

        let plan = series.plan_finalization(120 * SECOND).unwrap();
        series.commit_finalization(plan, false, &mut HistoryReport::default());
        series.reach_back_to(start);
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
