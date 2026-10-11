//! Gauge statistics over pinned history: time-weighted mean, min, max,
//! estimated integral and raw samples.

use dgxtop_core::errors::ErrorDetail;
use dgxtop_core::history::HistoryMethod;
use dgxtop_core::identity::SourceId;
use dgxtop_core::metric::Scalar;

use super::guard::ScanGuard;
use super::plan::{RegionKind, RegionPlan, Window, cells};
use super::points::{DataPoint, PointSink, SourceSet};
use super::{gap_reason, push_null_region};
use crate::chunk::Slices;
use crate::gaps::GapEntry;
use crate::grid::MinuteGrid;
use crate::records::{GaugeBucket, GaugeSample};
use crate::segment::SegmentsView;
use crate::steps::{RawGauge, Step, StepIter};

/// Nanoseconds per second as `f64`, for integrals of rates.
const NANOS_PER_SECOND_F64: f64 = 1e9;

/// Statistic computed over gauge steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GaugeStat {
    Mean,
    Min,
    Max,
    /// `sum(value x seconds)` of a rate gauge, labelled `estimated_integral`.
    Integral,
}

impl GaugeStat {
    fn method(self) -> HistoryMethod {
        match self {
            Self::Integral => HistoryMethod::EstimatedIntegral,
            Self::Mean | Self::Min | Self::Max => HistoryMethod::TimeWeightedStep,
        }
    }

    fn reason(self) -> Option<&'static str> {
        (self == Self::Integral).then_some("time_integral_of_rate")
    }
}

/// Pinned gauge history plus query context.
pub(crate) struct GaugeInputs<'p> {
    pub raw: Slices<'p, GaugeSample>,
    pub carry: Option<GaugeSample>,
    pub raw_floor_ns: u64,
    pub buckets: Slices<'p, GaugeBucket>,
    pub segments: &'p SegmentsView,
    pub source_filter: Option<&'p SourceId>,
    pub gaps: &'p [GapEntry],
    /// Query clock: the newest step is never extended into the future.
    pub now_ns: u64,
    /// The minutes its cells of whole minutes lie on.
    pub minute_grid: MinuteGrid,
}

impl<'p> GaugeInputs<'p> {
    fn raw_gauge(&self) -> RawGauge<'_, 'p> {
        RawGauge {
            slices: &self.raw,
            carry: self.carry,
            floor_ns: self.raw_floor_ns,
        }
    }

    fn source_of(&self, segment: u32) -> Option<&SourceId> {
        self.segments
            .get(segment)
            .map(|meta| &meta.identity.source_id)
    }

    /// Whether a segment passes the source filter.
    fn admits(&self, segment: u32) -> bool {
        self.source_filter
            .is_none_or(|filter| self.source_of(segment) == Some(filter))
    }

    fn null_reason(&self, start_ns: u64, end_ns: u64, fallback: &'static str) -> String {
        gap_reason(self.gaps, start_ns, end_ns).unwrap_or_else(|| fallback.to_owned())
    }
}

/// Time-weighted aggregate of the evidence inside one point.
struct GaugeAccumulator {
    covered_ns: u64,
    weighted_sum: f64,
    minimum: f64,
    maximum: f64,
    sources: SourceSet,
    filtered: bool,
}

impl GaugeAccumulator {
    fn new() -> Self {
        Self {
            covered_ns: 0,
            weighted_sum: 0.0,
            minimum: f64::INFINITY,
            maximum: f64::NEG_INFINITY,
            sources: SourceSet::default(),
            filtered: false,
        }
    }

    fn admit(&mut self, segment: u32, inputs: &GaugeInputs<'_>) -> bool {
        if !inputs.admits(segment) {
            self.filtered = true;
            return false;
        }
        if let Some(source) = inputs.source_of(segment) {
            self.sources.insert(source);
        }
        true
    }

    #[allow(clippy::cast_precision_loss)]
    fn add_step(&mut self, step: &Step, covered_ns: u64, inputs: &GaugeInputs<'_>) {
        if covered_ns == 0 || !self.admit(step.segment, inputs) {
            return;
        }
        let value = step.value.to_f64_lossy();
        self.covered_ns = self.covered_ns.saturating_add(covered_ns);
        self.weighted_sum += value * covered_ns as f64;
        self.minimum = self.minimum.min(value);
        self.maximum = self.maximum.max(value);
    }

    fn add_bucket(&mut self, bucket: &GaugeBucket, inputs: &GaugeInputs<'_>) {
        if bucket.covered_ns == 0 || !self.admit(bucket.segment, inputs) {
            return;
        }
        self.covered_ns = self.covered_ns.saturating_add(bucket.covered_ns);
        self.weighted_sum += bucket.weighted_sum;
        self.minimum = self.minimum.min(bucket.minimum);
        self.maximum = self.maximum.max(bucket.maximum);
    }

    #[allow(clippy::cast_precision_loss)]
    fn value(&self, stat: GaugeStat) -> Option<Scalar> {
        let value = match stat {
            GaugeStat::Mean => self.weighted_sum / self.covered_ns as f64,
            GaugeStat::Min => self.minimum,
            GaugeStat::Max => self.maximum,
            GaugeStat::Integral => self.weighted_sum / NANOS_PER_SECOND_F64,
        };
        Scalar::from_f64(value).ok()
    }
}

/// Streams steps over increasing ranges of one raw region.
struct StepCursor<'s, 'a> {
    steps: StepIter<'s, 'a>,
    pending: Option<Step>,
    charged: usize,
    last_finished_stale: bool,
}

impl<'s, 'a> StepCursor<'s, 'a> {
    fn new(raw: RawGauge<'s, 'a>, from_ns: u64, now_ns: u64) -> Self {
        Self {
            steps: StepIter::covering(raw, from_ns, now_ns),
            pending: None,
            charged: 0,
            last_finished_stale: false,
        }
    }

    fn charge(&mut self, guard: &mut ScanGuard<'_>) -> Result<(), ErrorDetail> {
        let consumed = self.steps.consumed();
        let delta = consumed.saturating_sub(self.charged);
        self.charged = consumed;
        guard.charge(u64::try_from(delta).unwrap_or(u64::MAX))
    }

    fn accumulate(
        &mut self,
        (from_ns, to_ns): (u64, u64),
        accumulator: &mut GaugeAccumulator,
        inputs: &GaugeInputs<'_>,
        guard: &mut ScanGuard<'_>,
    ) -> Result<(), ErrorDetail> {
        loop {
            if self.pending.is_none() {
                self.pending = self.steps.next();
                self.charge(guard)?;
            }
            let Some(step) = self.pending else {
                return Ok(());
            };
            if step.end_ns <= from_ns {
                self.finish(step);
                continue;
            }
            if step.start_ns >= to_ns {
                return Ok(());
            }
            let covered = step.end_ns.min(to_ns) - step.start_ns.max(from_ns);
            accumulator.add_step(&step, covered, inputs);
            if step.end_ns > to_ns {
                return Ok(());
            }
            self.finish(step);
        }
    }

    fn finish(&mut self, step: Step) {
        self.last_finished_stale = step.ended_by_deadline;
        self.pending = None;
    }
}

fn accumulate_buckets(
    (from_ns, to_ns): (u64, u64),
    accumulator: &mut GaugeAccumulator,
    inputs: &GaugeInputs<'_>,
    guard: &mut ScanGuard<'_>,
) -> Result<(), ErrorDetail> {
    let buckets = &inputs.buckets;
    for bucket in buckets.iter_from(buckets.seek_start(from_ns)) {
        if bucket.start_ns >= to_ns {
            break;
        }
        guard.charge(1)?;
        if bucket.end_ns <= to_ns {
            accumulator.add_bucket(bucket, inputs);
        }
    }
    Ok(())
}

/// Data accumulated for one contiguous run of data regions inside a cell.
struct Run {
    start_ns: u64,
    end_ns: u64,
    accumulator: GaugeAccumulator,
}

fn flush_run(
    run: Option<Run>,
    stat: GaugeStat,
    stale: bool,
    inputs: &GaugeInputs<'_>,
    sink: &mut PointSink,
) {
    let Some(run) = run else {
        return;
    };
    let (start, end) = (run.start_ns, run.end_ns);
    if run.accumulator.covered_ns == 0 {
        let fallback = if run.accumulator.filtered {
            "source_filtered"
        } else if stale {
            "stale"
        } else {
            "no_data"
        };
        sink.push_null(
            start,
            end,
            HistoryMethod::Gap,
            &inputs.null_reason(start, end, fallback),
        );
        return;
    }
    match run.accumulator.value(stat) {
        Some(value) => sink.push_data(DataPoint {
            start_ns: start,
            end_ns: end,
            value,
            covered_ns: run.accumulator.covered_ns,
            method: stat.method(),
            sources: run.accumulator.sources,
            reason: stat.reason(),
        }),
        None => sink.push_null(start, end, HistoryMethod::Gap, "non_finite_value"),
    }
}

/// Points on absolute cells of `cell_ns` (see `plan::cells`).
pub(crate) fn gauge_cells(
    inputs: &GaugeInputs<'_>,
    plan: &RegionPlan,
    window: Window,
    cell_ns: u64,
    stat: GaugeStat,
    sink: &mut PointSink,
    guard: &mut ScanGuard<'_>,
) -> Result<(), ErrorDetail> {
    let mut raw_cursor: Option<(usize, StepCursor<'_, '_>)> = None;
    for (cell_start, cell_end) in cells(window, cell_ns, inputs.minute_grid) {
        let mut run: Option<Run> = None;
        for (index, region) in plan.regions().iter().enumerate() {
            if region.end_ns <= cell_start || region.start_ns >= cell_end {
                continue;
            }
            let range = (region.start_ns.max(cell_start), region.end_ns.min(cell_end));
            if !region.kind.holds_data() {
                let stale = raw_cursor
                    .as_ref()
                    .is_some_and(|(_, cursor)| cursor.last_finished_stale);
                flush_run(run.take(), stat, stale, inputs, sink);
                push_null_region(region.kind, range, inputs.gaps, sink);
                continue;
            }
            let current = run.get_or_insert_with(|| Run {
                start_ns: range.0,
                end_ns: range.1,
                accumulator: GaugeAccumulator::new(),
            });
            current.end_ns = range.1;
            if region.kind == RegionKind::Rollup {
                accumulate_buckets(range, &mut current.accumulator, inputs, guard)?;
                continue;
            }
            if raw_cursor
                .as_ref()
                .is_none_or(|(cursor_region, _)| *cursor_region != index)
            {
                raw_cursor = Some((
                    index,
                    StepCursor::new(inputs.raw_gauge(), region.start_ns, inputs.now_ns),
                ));
            }
            if let Some((_, cursor)) = raw_cursor.as_mut() {
                cursor.accumulate(range, &mut current.accumulator, inputs, guard)?;
            }
        }
        let stale = raw_cursor
            .as_ref()
            .is_some_and(|(_, cursor)| cursor.last_finished_stale);
        flush_run(run.take(), stat, stale, inputs, sink);
        if sink.overflow_start().is_some() {
            return Ok(());
        }
    }
    Ok(())
}

/// One point per raw step inside `[from, to)` with gap points in between.
pub(crate) fn gauge_native(
    inputs: &GaugeInputs<'_>,
    (from_ns, to_ns): (u64, u64),
    stat: Option<GaugeStat>,
    sink: &mut PointSink,
    guard: &mut ScanGuard<'_>,
) -> Result<(), ErrorDetail> {
    let mut steps = StepIter::covering(inputs.raw_gauge(), from_ns, inputs.now_ns);
    let mut cursor = from_ns;
    let mut stale = false;
    let mut charged = 0usize;
    while let Some(step) = steps.next() {
        let consumed = steps.consumed();
        guard.charge(u64::try_from(consumed.saturating_sub(charged)).unwrap_or(u64::MAX))?;
        charged = consumed;
        if step.end_ns <= from_ns {
            stale = step.ended_by_deadline;
            continue;
        }
        if step.start_ns >= to_ns {
            break;
        }
        let (start, end) = (step.start_ns.max(from_ns), step.end_ns.min(to_ns));
        if start > cursor {
            let fallback = if stale { "stale" } else { "no_data" };
            sink.push_null(
                cursor,
                start,
                HistoryMethod::Gap,
                &inputs.null_reason(cursor, start, fallback),
            );
        }
        push_step_point(inputs, &step, (start, end), stat, sink);
        cursor = end;
        stale = step.ended_by_deadline;
        if sink.overflow_start().is_some() {
            return Ok(());
        }
    }
    if cursor < to_ns {
        let fallback = if stale { "stale" } else { "no_data" };
        sink.push_null(
            cursor,
            to_ns,
            HistoryMethod::Gap,
            &inputs.null_reason(cursor, to_ns, fallback),
        );
    }
    Ok(())
}

fn push_step_point(
    inputs: &GaugeInputs<'_>,
    step: &Step,
    (start, end): (u64, u64),
    stat: Option<GaugeStat>,
    sink: &mut PointSink,
) {
    if !inputs.admits(step.segment) {
        sink.push_null(start, end, HistoryMethod::Gap, "source_filtered");
        return;
    }
    let mut sources = SourceSet::default();
    if let Some(source) = inputs.source_of(step.segment) {
        sources.insert(source);
    }
    let (value, method, reason) = match stat {
        None => (Some(step.value), HistoryMethod::Raw, None),
        Some(GaugeStat::Integral) => {
            let mut accumulator = GaugeAccumulator::new();
            accumulator.add_step(step, end - start, inputs);
            (
                accumulator.value(GaugeStat::Integral),
                GaugeStat::Integral.method(),
                GaugeStat::Integral.reason(),
            )
        }
        Some(other) => (Some(step.value), other.method(), None),
    };
    match value {
        Some(value) => sink.push_data(DataPoint {
            start_ns: start,
            end_ns: end,
            value,
            covered_ns: end - start,
            method,
            sources,
            reason,
        }),
        None => sink.push_null(start, end, HistoryMethod::Gap, "non_finite_value"),
    }
}

/// Raw samples: the part of the window before the raw floor is reported as
/// expired raw history (rollups are not raw samples).
pub(crate) fn gauge_raw(
    inputs: &GaugeInputs<'_>,
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
        gauge_native(inputs, (from, window.end_ns), None, sink, guard)?;
    }
    Ok(())
}

/// Records a gauge evaluation would scan (pre-check against the budget).
pub(crate) fn gauge_scan_estimate(inputs: &GaugeInputs<'_>, plan: &RegionPlan) -> u64 {
    plan.regions()
        .iter()
        .map(|region| match region.kind {
            RegionKind::Raw => inputs
                .raw
                .count_starting_in(region.start_ns, region.end_ns)
                .saturating_add(2),
            RegionKind::Rollup => inputs
                .buckets
                .count_starting_in(region.start_ns, region.end_ns),
            RegionKind::BoundaryGap | RegionKind::NoData => 0,
        })
        .fold(0u64, |sum, count| {
            sum.saturating_add(u64::try_from(count).unwrap_or(u64::MAX))
        })
}

/// Records a raw-sample evaluation of `[from, to)` would scan.
pub(crate) fn raw_scan_estimate(inputs: &GaugeInputs<'_>, window: Window) -> u64 {
    let from = window.start_ns.max(inputs.raw_floor_ns);
    let count = inputs
        .raw
        .count_starting_in(from, window.end_ns)
        .saturating_add(2);
    u64::try_from(count).unwrap_or(u64::MAX)
}
