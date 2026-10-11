//! Evaluates one pinned series and assembles the result.

use dgxtop_core::clock::NANOS_PER_MILLI;
use dgxtop_core::errors::{ErrorCode, ErrorDetail};
use dgxtop_core::history::{
    HISTORY_SCHEMA_VERSION, HistoryMethod, HistoryPoint, HistoryQuery, HistoryResult, HistoryStat,
};
use dgxtop_core::metric::CanonicalUnit;

use super::counter::{self, CounterInputs};
use super::gauge::{self, GaugeInputs, GaugeStat};
use super::guard::ScanGuard;
use super::plan::{
    Floors, Grid, RegionPlan, Window, choose_grid, fit_cells, grid_alignment, rollup_boundary,
};
use super::points::PointSink;
use crate::grid::MinuteGrid;
use crate::pin::{PinnedCounter, PinnedGauge, PinnedSeries, PinnedTrack};

/// Coarsening attempts when a grid still produced too many points.
const MAX_REGRIDS: usize = 4;

/// Points plus paging state.
struct Evaluation {
    points: Vec<HistoryPoint>,
    truncated: bool,
    next_start_ns: Option<u64>,
    method: HistoryMethod,
}

fn suggestion(query: &HistoryQuery) -> String {
    if query.resolution_ms < 60_000 {
        "retry with resolution_ms >= 60000 (minute rollups) or a shorter window".to_owned()
    } else {
        "retry with a shorter window".to_owned()
    }
}

fn is_rate(unit: CanonicalUnit) -> bool {
    matches!(
        unit,
        CanonicalUnit::BytesPerSecond | CanonicalUnit::CountPerSecond | CanonicalUnit::Watts
    )
}

pub(crate) fn evaluate(
    pin: &PinnedSeries,
    query: &HistoryQuery,
    cap: usize,
    guard: &mut ScanGuard<'_>,
) -> Result<HistoryResult, ErrorDetail> {
    let window = Window {
        start_ns: query.start_boot_ns,
        end_ns: query.end_boot_ns,
    };
    let evaluation = match &pin.track {
        PinnedTrack::Gauge(gauge) => evaluate_gauge(pin, gauge, query, window, cap, guard)?,
        PinnedTrack::Counter(counter) => evaluate_counter(pin, counter, query, window, cap, guard)?,
    };
    Ok(assemble(pin, query, window, cap, evaluation))
}

fn gauge_stat(pin: &PinnedSeries, query: &HistoryQuery) -> Result<Option<GaugeStat>, ErrorDetail> {
    Ok(Some(match query.stat {
        HistoryStat::Raw => return Ok(None),
        HistoryStat::Mean => GaugeStat::Mean,
        HistoryStat::Min => GaugeStat::Min,
        HistoryStat::Max => GaugeStat::Max,
        HistoryStat::Total if is_rate(pin.descriptor.unit) => GaugeStat::Integral,
        HistoryStat::Total => {
            return Err(ErrorDetail::new(
                ErrorCode::ValidationFailed,
                format!(
                    "total is undefined for {} series without counter intervals",
                    pin.descriptor.unit
                ),
            )
            .with_request(query.request_id)
            .with_affected(query.key.series_selector()));
        }
    }))
}

fn evaluate_gauge(
    pin: &PinnedSeries,
    pinned: &PinnedGauge,
    query: &HistoryQuery,
    window: Window,
    cap: usize,
    guard: &mut ScanGuard<'_>,
) -> Result<Evaluation, ErrorDetail> {
    let stat = gauge_stat(pin, query)?;
    let inputs = GaugeInputs {
        raw: pinned.raw.slices(),
        carry: pinned.carry,
        raw_floor_ns: pinned.raw_floor_ns,
        buckets: pinned.buckets.slices(),
        segments: &pin.segments,
        source_filter: query.source_filter.as_ref(),
        gaps: &pin.gaps,
        now_ns: guard.now(),
        grid: pin.query_minutes(),
    };
    let Some(stat) = stat else {
        guard.admit_estimate(
            gauge::raw_scan_estimate(&inputs, window),
            &suggestion(query),
        )?;
        let mut sink = PointSink::new(cap);
        gauge::gauge_raw(&inputs, window, &mut sink, guard)?;
        return Ok(paged(sink, HistoryMethod::TimeWeightedStep));
    };
    let floors = Floors {
        raw_floor_ns: pinned.raw_floor_ns,
        finalized_ns: pin.finalized_through_ns,
        rollup_floor_ns: pinned.rollup_floor_ns,
    };
    let plan = RegionPlan::new(
        window,
        floors,
        rollup_boundary(query.resolution_ms, &floors),
        pin.query_minutes(),
    );
    guard.admit_estimate(
        gauge::gauge_scan_estimate(&inputs, &plan),
        &suggestion(query),
    )?;
    let mut grid = choose_grid(
        query.resolution_ms,
        window,
        &plan,
        (cap, pin.query_minutes()),
    );
    for _ in 0..MAX_REGRIDS {
        let mut sink = PointSink::new(cap);
        match grid {
            Grid::Native => gauge::gauge_native(
                &inputs,
                (window.start_ns, window.end_ns),
                Some(stat),
                &mut sink,
                guard,
            )?,
            Grid::Cells(cell) => {
                gauge::gauge_cells(&inputs, &plan, window, cell, stat, &mut sink, guard)?
            }
        }
        if sink.overflow_start().is_none() {
            return Ok(complete(sink, HistoryMethod::TimeWeightedStep));
        }
        grid = Grid::Cells(coarser(
            grid,
            window,
            (cap, grid_alignment(&plan)),
            pin.query_minutes(),
        ));
    }
    Err(too_many_points(query))
}

fn evaluate_counter(
    pin: &PinnedSeries,
    pinned: &PinnedCounter,
    query: &HistoryQuery,
    window: Window,
    cap: usize,
    guard: &mut ScanGuard<'_>,
) -> Result<Evaluation, ErrorDetail> {
    let inputs = CounterInputs {
        raw: pinned.raw.slices(),
        raw_floor_ns: pinned.raw_floor_ns,
        buckets: pinned.buckets.slices(),
        bridges: pinned.bridges.slices(),
        segments: &pin.segments,
        source_filter: query.source_filter.as_ref(),
        gaps: &pin.gaps,
        grid: pin.query_minutes(),
    };
    if query.stat == HistoryStat::Raw {
        guard.admit_estimate(
            counter::counter_raw_estimate(&inputs, window),
            &suggestion(query),
        )?;
        let mut sink = PointSink::new(cap);
        counter::counter_raw(&inputs, window, &mut sink, guard)?;
        return Ok(paged(sink, HistoryMethod::CounterDelta));
    }
    let floors = Floors {
        raw_floor_ns: pinned.raw_floor_ns,
        finalized_ns: pin.finalized_through_ns,
        rollup_floor_ns: pinned.rollup_floor_ns,
    };
    let mut plan = RegionPlan::new(
        window,
        floors,
        rollup_boundary(query.resolution_ms, &floors),
        pin.query_minutes(),
    );
    plan.absorb_crossings(|boundary| inputs.crossing_at(boundary));
    guard.admit_estimate(
        counter::counter_scan_estimate(&inputs, &plan),
        &suggestion(query),
    )?;
    let mut grid = choose_grid(
        query.resolution_ms,
        window,
        &plan,
        (cap, pin.query_minutes()),
    );
    for _ in 0..MAX_REGRIDS {
        let mut sink = PointSink::new(cap);
        match grid {
            Grid::Native => counter::counter_native(
                &inputs,
                (window.start_ns, window.end_ns),
                &mut sink,
                guard,
            )?,
            Grid::Cells(cell) => {
                counter::counter_cells(&inputs, &plan, window, cell, &mut sink, guard)?
            }
        }
        if sink.overflow_start().is_none() {
            return Ok(complete(sink, HistoryMethod::CounterDelta));
        }
        grid = Grid::Cells(coarser(
            grid,
            window,
            (cap, grid_alignment(&plan)),
            pin.query_minutes(),
        ));
    }
    Err(too_many_points(query))
}

/// Next grid after an overflow: native points become cells sized to the
/// cap; cells double.
fn coarser(grid: Grid, window: Window, cap_align: (usize, u64), minutes: MinuteGrid) -> u64 {
    match grid {
        Grid::Native => fit_cells(NANOS_PER_MILLI, window, cap_align, minutes),
        Grid::Cells(cell) => fit_cells(cell.saturating_mul(2), window, cap_align, minutes),
    }
}

fn too_many_points(query: &HistoryQuery) -> ErrorDetail {
    ErrorDetail::new(
        ErrorCode::QueryTooLarge,
        "history query cannot fit max_points even at a coarser resolution; retry with a shorter window",
    )
    .with_request(query.request_id)
    .with_affected(query.key.series_selector())
}

fn complete(sink: PointSink, method: HistoryMethod) -> Evaluation {
    Evaluation {
        points: sink.into_points(),
        truncated: false,
        next_start_ns: None,
        method,
    }
}

/// Raw results are never sampled: beyond the cap they are truncated and the
/// next page starts at the first point that did not fit.
fn paged(sink: PointSink, method: HistoryMethod) -> Evaluation {
    let next_start_ns = sink.overflow_start();
    Evaluation {
        points: sink.into_points(),
        truncated: next_start_ns.is_some(),
        next_start_ns,
        method,
    }
}

fn source_changes(
    pin: &PinnedSeries,
    window: Window,
    method: HistoryMethod,
    cap: usize,
) -> Vec<u64> {
    let counter = method == HistoryMethod::CounterDelta;
    let mut changes: Vec<u64> = pin
        .segments
        .iter()
        .filter(|segment| (segment.identity.method == HistoryMethod::CounterDelta) == counter)
        .filter(|segment| !segment.opens_series)
        .map(|segment| segment.started_ns)
        .filter(|started| *started > window.start_ns && *started < window.end_ns)
        .collect();
    changes.sort_unstable();
    changes.dedup();
    changes.truncate(cap);
    changes
}

fn assemble(
    pin: &PinnedSeries,
    query: &HistoryQuery,
    window: Window,
    cap: usize,
    evaluation: Evaluation,
) -> HistoryResult {
    let span = window.span_ns();
    let covered = evaluation
        .points
        .iter()
        .fold(0u64, |sum, point| sum.saturating_add(point.covered_ns))
        .min(span);
    let mut discontinuities = pin.discontinuities.clone();
    discontinuities.truncate(cap);
    HistoryResult {
        schema_version: HISTORY_SCHEMA_VERSION,
        request_id: query.request_id,
        key: query.key.clone(),
        start_boot_ns: window.start_ns,
        end_boot_ns: window.end_ns,
        covered_ns: covered,
        coverage_ratio: HistoryResult::ratio(covered, span),
        truncated: evaluation.truncated,
        source_changes: source_changes(pin, window, evaluation.method, cap),
        points: evaluation.points,
        clock_discontinuities: discontinuities,
        snapshot_revision: pin.revision,
        next_start_boot_ns: evaluation.next_start_ns,
    }
}
