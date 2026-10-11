//! Exact counter totals over pinned history.
//!
//! A point counts only intervals fully inside it: whole buckets, bridges and
//! raw intervals. Each interval has exactly one owner, decided by the region
//! holding its last nanosecond (raw regions own raw intervals, rollup regions
//! own buckets and bridges), so nothing is counted twice and nothing is ever
//! split proportionally.

use dgxtop_core::errors::ErrorDetail;
use dgxtop_core::history::HistoryMethod;
use dgxtop_core::identity::SourceId;

use super::gap_reason;
use super::guard::ScanGuard;
use super::plan::{RegionKind, RegionPlan, Window, cells};
use super::points::{DataPoint, PointSink, SourceSet};
use super::push_null_region;
use crate::chunk::Slices;
use crate::gaps::GapEntry;
use crate::grid::MinuteGrid;
use crate::records::{CounterBucket, CounterSample, CounterTotal};
use crate::segment::SegmentsView;

/// Pinned counter history plus query context.
pub(crate) struct CounterInputs<'p> {
    pub raw: Slices<'p, CounterSample>,
    pub raw_floor_ns: u64,
    pub buckets: Slices<'p, CounterBucket>,
    pub bridges: Slices<'p, CounterSample>,
    pub segments: &'p SegmentsView,
    pub source_filter: Option<&'p SourceId>,
    pub gaps: &'p [GapEntry],
    /// The minutes its cells of whole minutes lie on.
    pub minute_grid: MinuteGrid,
}

impl CounterInputs<'_> {
    fn source_of(&self, segment: u32) -> Option<&SourceId> {
        self.segments
            .get(segment)
            .map(|meta| &meta.identity.source_id)
    }

    fn admits(&self, segment: u32) -> bool {
        self.source_filter
            .is_none_or(|filter| self.source_of(segment) == Some(filter))
    }

    fn null_reason(&self, start_ns: u64, end_ns: u64, fallback: &'static str) -> String {
        gap_reason(self.gaps, start_ns, end_ns).unwrap_or_else(|| fallback.to_owned())
    }

    /// The interval (bridge first, else raw) that crosses `boundary_ns`.
    pub(crate) fn crossing_at(&self, boundary_ns: u64) -> Option<(u64, u64)> {
        let crossing = |log: &Slices<'_, CounterSample>| {
            log.last_starting_before(boundary_ns)
                .filter(|interval| interval.end_ns > boundary_ns)
                .map(|interval| (interval.start_ns, interval.end_ns))
        };
        crossing(&self.bridges).or_else(|| crossing(&self.raw))
    }
}

#[derive(Default)]
struct CounterAccumulator {
    total: CounterTotal,
    covered_ns: u64,
    used_buckets: bool,
    saw_partial: bool,
    filtered: bool,
    sources: SourceSet,
}

impl CounterAccumulator {
    fn admit(&mut self, segment: u32, inputs: &CounterInputs<'_>) -> bool {
        if !inputs.admits(segment) {
            self.filtered = true;
            return false;
        }
        if let Some(source) = inputs.source_of(segment) {
            self.sources.insert(source);
        }
        true
    }

    fn add_interval(&mut self, interval: &CounterSample, inputs: &CounterInputs<'_>) {
        if !self.admit(interval.segment, inputs) {
            return;
        }
        self.total.add_delta(interval.delta);
        self.covered_ns = self
            .covered_ns
            .saturating_add(interval.end_ns.saturating_sub(interval.start_ns));
    }

    fn add_bucket(&mut self, bucket: &CounterBucket, inputs: &CounterInputs<'_>) {
        if bucket.interval_count == 0 || !self.admit(bucket.segment, inputs) {
            return;
        }
        self.total.add_total(&bucket.total);
        self.covered_ns = self.covered_ns.saturating_add(bucket.covered_ns);
        self.used_buckets = true;
    }
}

/// Counts the intervals of `log` owned by `(from, to]` (by end time) that lie
/// fully inside the point `[point_start, point_end)`.
fn count_owned_intervals(
    log: &Slices<'_, CounterSample>,
    (from_ns, to_ns): (u64, u64),
    (point_start, point_end): (u64, u64),
    accumulator: &mut CounterAccumulator,
    inputs: &CounterInputs<'_>,
    guard: &mut ScanGuard<'_>,
) -> Result<(), ErrorDetail> {
    for interval in log.iter_from(log.seek_end_after(from_ns)) {
        if interval.end_ns > to_ns {
            break;
        }
        guard.charge(1)?;
        if interval.start_ns >= point_start && interval.end_ns <= point_end {
            accumulator.add_interval(interval, inputs);
        } else {
            accumulator.saw_partial = true;
        }
    }
    Ok(())
}

fn count_buckets(
    (from_ns, to_ns): (u64, u64),
    (point_start, point_end): (u64, u64),
    accumulator: &mut CounterAccumulator,
    inputs: &CounterInputs<'_>,
    guard: &mut ScanGuard<'_>,
) -> Result<(), ErrorDetail> {
    let buckets = &inputs.buckets;
    for bucket in buckets.iter_from(buckets.seek_start(from_ns)) {
        if bucket.start_ns >= to_ns {
            break;
        }
        guard.charge(1)?;
        if bucket.start_ns >= point_start && bucket.end_ns <= point_end {
            accumulator.add_bucket(bucket, inputs);
        } else {
            accumulator.saw_partial = true;
        }
    }
    Ok(())
}

/// Evaluates one run of contiguous data portions as one point.
fn flush_run(
    portions: &mut Vec<(u64, u64, RegionKind)>,
    inputs: &CounterInputs<'_>,
    sink: &mut PointSink,
    guard: &mut ScanGuard<'_>,
) -> Result<(), ErrorDetail> {
    let (Some(first), Some(last)) = (portions.first().copied(), portions.last().copied()) else {
        return Ok(());
    };
    let point = (first.0, last.1);
    let mut accumulator = CounterAccumulator::default();
    for (from, to, kind) in portions.drain(..) {
        match kind {
            RegionKind::Raw => {
                count_owned_intervals(
                    &inputs.raw,
                    (from, to),
                    point,
                    &mut accumulator,
                    inputs,
                    guard,
                )?;
            }
            RegionKind::Rollup => {
                count_buckets((from, to), point, &mut accumulator, inputs, guard)?;
                count_owned_intervals(
                    &inputs.bridges,
                    (from, to),
                    point,
                    &mut accumulator,
                    inputs,
                    guard,
                )?;
            }
            RegionKind::BoundaryGap | RegionKind::NoData => {}
        }
    }
    emit(point, accumulator, inputs, sink);
    Ok(())
}

fn emit(
    (start, end): (u64, u64),
    accumulator: CounterAccumulator,
    inputs: &CounterInputs<'_>,
    sink: &mut PointSink,
) {
    if accumulator.covered_ns == 0 {
        if accumulator.filtered {
            sink.push_null(start, end, HistoryMethod::Gap, "source_filtered");
        } else if accumulator.saw_partial {
            sink.push_null(
                start,
                end,
                HistoryMethod::BoundaryGap,
                "interval_crosses_point_boundary",
            );
        } else {
            sink.push_null(
                start,
                end,
                HistoryMethod::Gap,
                &inputs.null_reason(start, end, "no_data"),
            );
        }
        return;
    }
    let Some((value, reason)) = accumulator.total.to_scalar() else {
        sink.push_null(start, end, HistoryMethod::Gap, "non_finite_value");
        return;
    };
    let method = if accumulator.used_buckets {
        HistoryMethod::BucketExact
    } else {
        HistoryMethod::CounterDelta
    };
    sink.push_data(DataPoint {
        start_ns: start,
        end_ns: end,
        value,
        covered_ns: accumulator.covered_ns,
        method,
        sources: accumulator.sources,
        reason,
    });
}

/// Totals on absolute cells of `cell_ns` (see `plan::cells`).
pub(crate) fn counter_cells(
    inputs: &CounterInputs<'_>,
    plan: &RegionPlan,
    window: Window,
    cell_ns: u64,
    sink: &mut PointSink,
    guard: &mut ScanGuard<'_>,
) -> Result<(), ErrorDetail> {
    let mut portions: Vec<(u64, u64, RegionKind)> = Vec::new();
    for (cell_start, cell_end) in cells(window, cell_ns, inputs.minute_grid) {
        for region in plan.regions() {
            if region.end_ns <= cell_start || region.start_ns >= cell_end {
                continue;
            }
            let range = (region.start_ns.max(cell_start), region.end_ns.min(cell_end));
            if region.kind.holds_data() {
                portions.push((range.0, range.1, region.kind));
            } else {
                flush_run(&mut portions, inputs, sink, guard)?;
                push_null_region(region.kind, range, inputs.gaps, sink);
            }
        }
        flush_run(&mut portions, inputs, sink, guard)?;
        if sink.overflow_start().is_some() {
            return Ok(());
        }
    }
    Ok(())
}

/// One point per raw interval inside `[from, to)`; intervals crossing a
/// window edge are boundary gaps, never split.
pub(crate) fn counter_native(
    inputs: &CounterInputs<'_>,
    (from_ns, to_ns): (u64, u64),
    sink: &mut PointSink,
    guard: &mut ScanGuard<'_>,
) -> Result<(), ErrorDetail> {
    let raw = &inputs.raw;
    let mut cursor = from_ns;
    for interval in raw.iter_from(raw.seek_end_after(from_ns)) {
        if interval.start_ns >= to_ns {
            break;
        }
        guard.charge(1)?;
        let (start, end) = (interval.start_ns.max(from_ns), interval.end_ns.min(to_ns));
        if start > cursor {
            sink.push_null(
                cursor,
                start,
                HistoryMethod::Gap,
                &inputs.null_reason(cursor, start, "no_data"),
            );
        }
        if interval.start_ns < from_ns || interval.end_ns > to_ns {
            sink.push_null(
                start,
                end,
                HistoryMethod::BoundaryGap,
                "interval_crosses_window_boundary",
            );
        } else if !inputs.admits(interval.segment) {
            sink.push_null(start, end, HistoryMethod::Gap, "source_filtered");
        } else {
            let mut sources = SourceSet::default();
            if let Some(source) = inputs.source_of(interval.segment) {
                sources.insert(source);
            }
            sink.push_data(DataPoint {
                start_ns: start,
                end_ns: end,
                value: interval.delta,
                covered_ns: end - start,
                method: HistoryMethod::CounterDelta,
                sources,
                reason: None,
            });
        }
        cursor = cursor.max(end);
        if sink.overflow_start().is_some() {
            return Ok(());
        }
    }
    if cursor < to_ns {
        sink.push_null(
            cursor,
            to_ns,
            HistoryMethod::Gap,
            &inputs.null_reason(cursor, to_ns, "no_data"),
        );
    }
    Ok(())
}

/// Raw intervals; time before the raw floor is expired raw history.
pub(crate) fn counter_raw(
    inputs: &CounterInputs<'_>,
    window: Window,
    sink: &mut PointSink,
    guard: &mut ScanGuard<'_>,
) -> Result<(), ErrorDetail> {
    let floor = inputs.raw_floor_ns;
    if window.start_ns < floor {
        sink.push_null(
            window.start_ns,
            floor.min(window.end_ns),
            HistoryMethod::Gap,
            "raw_history_expired",
        );
    }
    let from = window.start_ns.max(floor);
    if from < window.end_ns {
        counter_native(inputs, (from, window.end_ns), sink, guard)?;
    }
    Ok(())
}

/// Records a counter evaluation would scan (pre-check against the budget).
pub(crate) fn counter_scan_estimate(inputs: &CounterInputs<'_>, plan: &RegionPlan) -> u64 {
    plan.regions()
        .iter()
        .map(|region| match region.kind {
            RegionKind::Raw => inputs.raw.count_ending_in(region.start_ns, region.end_ns),
            RegionKind::Rollup => inputs
                .buckets
                .count_starting_in(region.start_ns, region.end_ns)
                .saturating_add(
                    inputs
                        .bridges
                        .count_ending_in(region.start_ns, region.end_ns),
                ),
            RegionKind::BoundaryGap | RegionKind::NoData => 0,
        })
        .fold(0u64, |sum, count| {
            sum.saturating_add(u64::try_from(count).unwrap_or(u64::MAX))
        })
}

/// Records a raw-interval evaluation would scan.
pub(crate) fn counter_raw_estimate(inputs: &CounterInputs<'_>, window: Window) -> u64 {
    let from = window.start_ns.max(inputs.raw_floor_ns);
    let count = inputs
        .raw
        .count_ending_in(from, window.end_ns)
        .saturating_add(1);
    u64::try_from(count).unwrap_or(u64::MAX)
}
