//! Which evidence covers which part of a query window, and the point grid.
//!
//! Before the rollup boundary, whole minute buckets fully inside the window
//! come from rollups; a partial bucket at a window edge comes from raw data
//! when raw still covers it and is otherwise a `boundary_gap` (never
//! reconstructed proportionally). From the boundary on, raw data is used.

use dgxtop_core::clock::{NANOS_PER_MILLI, ms_to_ns};

use crate::history::records::{BUCKET_NS, floor_bucket};

/// Half-open query window `[start, end)` with `end > start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window {
    pub start_ns: u64,
    pub end_ns: u64,
}

impl Window {
    pub(crate) fn span_ns(&self) -> u64 {
        self.end_ns.saturating_sub(self.start_ns)
    }
}

/// Availability floors of one track (all bucket aligned).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Floors {
    /// Raw data is complete from here on.
    pub raw_floor_ns: u64,
    /// Rollups exist for every bucket before this.
    pub finalized_ns: u64,
    /// Rollups were retained from here on.
    pub rollup_floor_ns: u64,
}

/// Evidence kind of one region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegionKind {
    Raw,
    Rollup,
    /// Part of a minute bucket whose raw data is gone: not countable.
    BoundaryGap,
    /// Before the retained history.
    NoData,
}

impl RegionKind {
    pub(crate) fn holds_data(self) -> bool {
        matches!(self, Self::Raw | Self::Rollup)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Region {
    pub start_ns: u64,
    pub end_ns: u64,
    pub kind: RegionKind,
}

/// Rollup/raw boundary: coarse queries use every finalized minute; fine
/// queries use raw data wherever it still exists.
pub(crate) fn rollup_boundary(resolution_ms: u32, floors: &Floors) -> u64 {
    if ms_to_ns(u64::from(resolution_ms)) >= BUCKET_NS {
        floors.finalized_ns.max(floors.raw_floor_ns)
    } else {
        floors.raw_floor_ns
    }
}

fn partial_bucket_kind(bucket_start: u64, floors: &Floors) -> RegionKind {
    if bucket_start >= floors.raw_floor_ns {
        RegionKind::Raw
    } else if bucket_start >= floors.rollup_floor_ns {
        RegionKind::BoundaryGap
    } else {
        RegionKind::NoData
    }
}

/// Contiguous regions covering the window, in time order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegionPlan {
    regions: Vec<Region>,
    window: Window,
    boundary_ns: u64,
}

impl RegionPlan {
    pub(crate) fn new(window: Window, floors: Floors, boundary_ns: u64) -> Self {
        let mut plan = Self {
            regions: Vec::new(),
            window,
            boundary_ns,
        };
        let end = window.end_ns;
        let mut cursor = window.start_ns;
        if cursor < boundary_ns {
            if !cursor.is_multiple_of(BUCKET_NS) {
                let bucket = floor_bucket(cursor);
                let head_end = bucket.saturating_add(BUCKET_NS).min(end);
                plan.push(cursor, head_end, partial_bucket_kind(bucket, &floors));
                cursor = head_end;
            }
            let body_end = boundary_ns.min(floor_bucket(end)).max(cursor);
            if cursor < body_end {
                let lower = floors.raw_floor_ns.min(floors.rollup_floor_ns);
                let no_data_end = lower.clamp(cursor, body_end);
                let raw_end = floors.rollup_floor_ns.clamp(no_data_end, body_end);
                plan.push(cursor, no_data_end, RegionKind::NoData);
                plan.push(no_data_end, raw_end, RegionKind::Raw);
                plan.push(raw_end, body_end, RegionKind::Rollup);
                cursor = body_end;
            }
            if cursor < end && cursor < boundary_ns {
                let tail_end = end.min(boundary_ns);
                plan.push(
                    cursor,
                    tail_end,
                    partial_bucket_kind(floor_bucket(cursor), &floors),
                );
                cursor = tail_end;
            }
        }
        if cursor < end {
            plan.push(cursor, end, RegionKind::Raw);
        }
        plan
    }

    fn push(&mut self, start_ns: u64, end_ns: u64, kind: RegionKind) {
        if start_ns >= end_ns {
            return;
        }
        if let Some(last) = self.regions.last_mut()
            && last.kind == kind
            && last.end_ns == start_ns
        {
            last.end_ns = end_ns;
            return;
        }
        self.regions.push(Region {
            start_ns,
            end_ns,
            kind,
        });
    }

    pub(crate) fn regions(&self) -> &[Region] {
        &self.regions
    }

    /// Whether part of the window lies before the rollup boundary (the grid
    /// must then be minute aligned).
    pub(crate) fn reaches_rollups(&self) -> bool {
        self.window.start_ns < self.boundary_ns
    }

    #[cfg(test)]
    pub(crate) fn kind_at(&self, time_ns: u64) -> Option<RegionKind> {
        self.regions
            .iter()
            .find(|region| region.start_ns <= time_ns && time_ns < region.end_ns)
            .map(|region| region.kind)
    }

    /// Moves a boundary between a non-data and a data region onto a fully
    /// known interval that crosses it (a bridge), so that interval stays
    /// countable instead of being swallowed by the boundary gap.
    pub(crate) fn absorb_crossings(&mut self, crossing: impl Fn(u64) -> Option<(u64, u64)>) {
        for index in 1..self.regions.len() {
            let (left, right) = (self.regions[index - 1], self.regions[index]);
            let boundary = left.end_ns;
            let Some((start, end)) = crossing(boundary) else {
                continue;
            };
            if start >= boundary || end <= boundary {
                continue;
            }
            let moved_to = match (left.kind.holds_data(), right.kind.holds_data()) {
                (false, true) if start >= left.start_ns => start,
                (true, false) if end <= right.end_ns => end,
                _ => continue,
            };
            self.regions[index - 1].end_ns = moved_to;
            self.regions[index].start_ns = moved_to;
        }
        self.regions
            .retain(|region| region.start_ns < region.end_ns);
    }
}

/// How points are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Grid {
    /// One point per raw step or interval (resolution 0, raw data only).
    Native,
    /// Absolute cells `[k*cell, (k+1)*cell)` clipped to the window.
    Cells(u64),
}

/// Points reserved for boundary and gap splits besides the cells.
const EDGE_POINTS: usize = 4;

fn round_up(value: u64, align: u64) -> u64 {
    let align = align.max(1);
    value.div_ceil(align).saturating_mul(align)
}

fn cell_count(window: Window, cell_ns: u64) -> u64 {
    let cell = cell_ns.max(1);
    (window.end_ns.saturating_sub(1) / cell)
        .saturating_sub(window.start_ns / cell)
        .saturating_add(1)
}

/// Smallest aligned cell not below `cell_ns` that keeps the cells within the
/// point cap ("aggregated beyond max_points -> coarser buckets").
pub(crate) fn fit_cells(cell_ns: u64, window: Window, cap: usize, align: u64) -> u64 {
    let usable = u64::try_from(cap.saturating_sub(EDGE_POINTS).max(1)).unwrap_or(u64::MAX);
    let mut cell = round_up(cell_ns.max(1), align);
    if cell_count(window, cell) <= usable {
        return cell;
    }
    cell = cell.max(round_up(window.span_ns().div_ceil(usable), align));
    for _ in 0..64 {
        if cell_count(window, cell) <= usable {
            return cell;
        }
        cell = cell.saturating_add(align.max(1));
    }
    round_up(window.span_ns(), align).max(cell)
}

/// Chooses the grid for an aggregated statistic.
pub(crate) fn choose_grid(
    resolution_ms: u32,
    window: Window,
    plan: &RegionPlan,
    cap: usize,
) -> Grid {
    let rollups = plan.reaches_rollups();
    if resolution_ms == 0 && !rollups {
        return Grid::Native;
    }
    let align = if rollups { BUCKET_NS } else { NANOS_PER_MILLI };
    let requested = if resolution_ms == 0 {
        BUCKET_NS
    } else {
        ms_to_ns(u64::from(resolution_ms))
    };
    Grid::Cells(fit_cells(requested, window, cap, align))
}

/// Alignment of a grid over `plan`.
pub(crate) fn grid_alignment(plan: &RegionPlan) -> u64 {
    if plan.reaches_rollups() {
        BUCKET_NS
    } else {
        NANOS_PER_MILLI
    }
}

/// Cells of `cell_ns` clipped to the window, in order.
pub(crate) fn cells(window: Window, cell_ns: u64) -> impl Iterator<Item = (u64, u64)> {
    let cell = cell_ns.max(1);
    let mut start = window.start_ns;
    std::iter::from_fn(move || {
        if start >= window.end_ns {
            return None;
        }
        let next = (start / cell).saturating_add(1).saturating_mul(cell);
        let end = next.min(window.end_ns);
        let current = (start, end);
        start = end;
        Some(current)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: u64 = BUCKET_NS;
    const SECOND: u64 = 1_000_000_000;

    fn window(start: u64, end: u64) -> Window {
        Window {
            start_ns: start,
            end_ns: end,
        }
    }

    fn kinds(plan: &RegionPlan) -> Vec<(u64, u64, RegionKind)> {
        plan.regions()
            .iter()
            .map(|region| (region.start_ns, region.end_ns, region.kind))
            .collect()
    }

    #[test]
    fn raw_window_is_a_single_raw_region() {
        let floors = Floors {
            raw_floor_ns: 10 * MINUTE,
            finalized_ns: 12 * MINUTE,
            rollup_floor_ns: 0,
        };
        let plan = RegionPlan::new(window(11 * MINUTE, 13 * MINUTE), floors, 10 * MINUTE);
        assert_eq!(
            kinds(&plan),
            vec![(11 * MINUTE, 13 * MINUTE, RegionKind::Raw)]
        );
        assert!(!plan.reaches_rollups());
    }

    #[test]
    fn evicted_raw_edges_become_boundary_gaps() {
        let floors = Floors {
            raw_floor_ns: 10 * MINUTE,
            finalized_ns: 12 * MINUTE,
            rollup_floor_ns: 2 * MINUTE,
        };
        let start = 3 * MINUTE + 30 * SECOND;
        let end = 6 * MINUTE + 10 * SECOND;
        let plan = RegionPlan::new(window(start, end), floors, 10 * MINUTE);
        assert_eq!(
            kinds(&plan),
            vec![
                (start, 4 * MINUTE, RegionKind::BoundaryGap),
                (4 * MINUTE, 6 * MINUTE, RegionKind::Rollup),
                (6 * MINUTE, end, RegionKind::BoundaryGap),
            ]
        );
        assert!(plan.reaches_rollups());
    }

    #[test]
    fn window_spanning_floors_has_every_kind_in_order() {
        let floors = Floors {
            raw_floor_ns: 10 * MINUTE,
            finalized_ns: 12 * MINUTE,
            rollup_floor_ns: 2 * MINUTE,
        };
        let start = 30 * SECOND;
        let end = 14 * MINUTE + 5 * SECOND;
        let fine = RegionPlan::new(window(start, end), floors, rollup_boundary(1000, &floors));
        assert_eq!(
            kinds(&fine),
            vec![
                (start, 2 * MINUTE, RegionKind::NoData),
                (2 * MINUTE, 10 * MINUTE, RegionKind::Rollup),
                (10 * MINUTE, end, RegionKind::Raw),
            ]
        );
        let coarse = RegionPlan::new(window(start, end), floors, rollup_boundary(60_000, &floors));
        assert_eq!(
            kinds(&coarse),
            vec![
                (start, 2 * MINUTE, RegionKind::NoData),
                (2 * MINUTE, 12 * MINUTE, RegionKind::Rollup),
                (12 * MINUTE, end, RegionKind::Raw),
            ]
        );
    }

    #[test]
    fn raw_fills_buckets_before_a_shortened_rollup_floor() {
        let floors = Floors {
            raw_floor_ns: 5 * MINUTE,
            finalized_ns: 12 * MINUTE,
            rollup_floor_ns: 8 * MINUTE,
        };
        let plan = RegionPlan::new(
            window(0, 13 * MINUTE),
            floors,
            rollup_boundary(60_000, &floors),
        );
        assert_eq!(
            kinds(&plan),
            vec![
                (0, 5 * MINUTE, RegionKind::NoData),
                (5 * MINUTE, 8 * MINUTE, RegionKind::Raw),
                (8 * MINUTE, 12 * MINUTE, RegionKind::Rollup),
                (12 * MINUTE, 13 * MINUTE, RegionKind::Raw),
            ]
        );
    }

    #[test]
    fn crossing_interval_moves_the_boundary() {
        let floors = Floors {
            raw_floor_ns: 10 * MINUTE,
            finalized_ns: 12 * MINUTE,
            rollup_floor_ns: 0,
        };
        let start = 3 * MINUTE + 30 * SECOND;
        let mut plan = RegionPlan::new(window(start, 6 * MINUTE), floors, 10 * MINUTE);
        let bridge_start = 4 * MINUTE - SECOND / 2;
        plan.absorb_crossings(|boundary| {
            (boundary == 4 * MINUTE).then_some((bridge_start, 4 * MINUTE + SECOND / 2))
        });
        assert_eq!(
            kinds(&plan),
            vec![
                (start, bridge_start, RegionKind::BoundaryGap),
                (bridge_start, 6 * MINUTE, RegionKind::Rollup),
            ]
        );
        assert_eq!(plan.kind_at(bridge_start), Some(RegionKind::Rollup));
    }

    #[test]
    fn grids_fit_the_point_cap() {
        let day = window(0, 24 * 60 * MINUTE);
        let floors = Floors {
            raw_floor_ns: 0,
            finalized_ns: 0,
            rollup_floor_ns: 0,
        };
        let raw_plan = RegionPlan::new(day, floors, 0);
        assert_eq!(choose_grid(0, day, &raw_plan, 100), Grid::Native);
        let Grid::Cells(cell) = choose_grid(1000, day, &raw_plan, 100) else {
            panic!("expected cells");
        };
        assert!(cell_count(day, cell) <= 96);
        let rollup_floors = Floors {
            raw_floor_ns: 1440 * MINUTE,
            finalized_ns: 1440 * MINUTE,
            rollup_floor_ns: 0,
        };
        let rollup_plan = RegionPlan::new(day, rollup_floors, 1440 * MINUTE);
        let Grid::Cells(cell) = choose_grid(0, day, &rollup_plan, 2000) else {
            panic!("expected cells");
        };
        assert_eq!(cell, MINUTE);
        let Grid::Cells(coarse) = choose_grid(90_000, day, &rollup_plan, 100) else {
            panic!("expected cells");
        };
        assert_eq!(coarse % MINUTE, 0);
        assert!(cell_count(day, coarse) <= 96);
    }

    #[test]
    fn cells_are_absolute_and_clipped() {
        let collected: Vec<(u64, u64)> = cells(window(5, 25), 10).collect();
        assert_eq!(collected, vec![(5, 10), (10, 20), (20, 25)]);
        assert_eq!(cell_count(window(5, 25), 10), 3);
        assert_eq!(fit_cells(1, window(0, 100), 14, 1), 10);
    }
}
