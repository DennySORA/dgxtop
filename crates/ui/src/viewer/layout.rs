//! LayoutEngine: size tiers, banner-first allocation and manifest-driven
//! module placement. The engine never names a module: placement derives from
//! the modules' overview cells, priority and minimum sizes, so a new module
//! needs no layout change.
//!
//! Tiers: width ≥ 120 → wide; 80–119 → single column summary +
//! active detail; width < 80 or height < 24 → minimal summary with keyboard
//! navigation; below 40×10 → the "terminal too small" screen. The wide tiers
//! draw the tabs in the header row; the minimal tier has a selector row.
//!
//! Overview grid (D-38): a module's [`OverviewCell`] names its row, column
//! and width weight. A row's modules sit side by side with widths in
//! proportion to their weights, and rows run top to bottom. Every row first
//! gets its minimum height (the lowest-priority rows are hidden when even
//! that does not fit), then rows grow toward their preferred heights (a row
//! with a module the user expanded first, then by priority), and the filling
//! row takes the rest. A row whose modules cannot
//! sit side by side is stacked; a module without a cell gets a full-width
//! row of its own below the others.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use dgxtop_core::commands::ViewKind;
use ratatui::layout::Rect;

use crate::num::saturate_u16;
use crate::registry::{OverviewCell, Slot};

pub const MIN_COLUMNS: u16 = 40;
pub const MIN_ROWS: u16 = 10;
const TWO_COLUMN_WIDTH: u16 = 120;
const SINGLE_COLUMN_WIDTH: u16 = 80;
const FULL_HEIGHT: u16 = 24;
/// Width of the summary sidebar next to a detail module in the wide tier.
const DETAIL_SIDEBAR_WIDTH: u16 = 36;
/// Total width from which a detail view gets the summary sidebar.
const DETAIL_SIDEBAR_MIN_TOTAL: u16 = 156;

/// Terminal size class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SizeTier {
    TooSmall,
    Minimal,
    SingleColumn,
    TwoColumn,
}

pub const fn size_tier(width: u16, height: u16) -> SizeTier {
    if width < MIN_COLUMNS || height < MIN_ROWS {
        SizeTier::TooSmall
    } else if width < SINGLE_COLUMN_WIDTH || height < FULL_HEIGHT {
        SizeTier::Minimal
    } else if width < TWO_COLUMN_WIDTH {
        SizeTier::SingleColumn
    } else {
        SizeTier::TwoColumn
    }
}

/// A module the engine may place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    /// Caller-defined index (registry position).
    pub index: usize,
    pub min_columns: u16,
    pub min_rows: u16,
    pub priority: u8,
    /// Position in the overview grid (`None`: a full-width row of its own).
    pub cell: Option<OverviewCell>,
    /// The user expanded this module: its row grows before the others.
    pub expanded: bool,
}

/// A module placed into an area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub index: usize,
    pub area: Rect,
    pub slot: Slot,
    pub compact: bool,
}

/// Inputs of one layout pass.
pub struct LayoutRequest<'a> {
    pub area: Rect,
    pub view: ViewKind,
    /// Rows each banner needs, highest priority first.
    pub banner_heights: &'a [u16],
    /// Overview candidates (used when `view` is Overview).
    pub overview: &'a [Candidate],
    /// The module of a detail view.
    pub detail: Option<Candidate>,
    /// Lines the summary strip can show.
    pub summary_lines: u16,
    /// Preferred height of a candidate at a width (`None` = fill).
    pub preferred_rows: &'a dyn Fn(usize, u16) -> Option<u16>,
}

/// Result of one layout pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramePlan {
    pub tier: SizeTier,
    pub header: Rect,
    /// One-line view selector (minimal tier); the wide tiers draw their
    /// tabs in the header row.
    pub tabs: Option<Rect>,
    /// One rect per placed banner, in input order.
    pub banners: Vec<Rect>,
    pub dropped_banners: usize,
    pub summary: Option<Rect>,
    pub placements: Vec<Placement>,
    pub hidden: Vec<usize>,
    /// Explanation drawn when the active detail module cannot fit.
    pub body_note: Option<Rect>,
    pub body: Rect,
    pub footer: Rect,
}

/// Splits `rows` off the top of `area`.
fn take_top(area: &mut Rect, rows: u16) -> Rect {
    let rows = rows.min(area.height);
    let top = Rect::new(area.x, area.y, area.width, rows);
    area.y = area.y.saturating_add(rows);
    area.height -= rows;
    top
}

fn take_bottom(area: &mut Rect, rows: u16) -> Rect {
    let rows = rows.min(area.height);
    area.height -= rows;
    Rect::new(area.x, area.y + area.height, area.width, rows)
}

/// Plans a frame of at least 40×10 (callers handle `TooSmall` themselves).
pub fn plan(request: &LayoutRequest<'_>) -> FramePlan {
    let tier = size_tier(request.area.width, request.area.height);
    let mut remaining = request.area;
    let header = take_top(&mut remaining, 1);
    let footer = take_bottom(&mut remaining, 1);
    let (banner_rects, dropped_banners) = allocate_banners(&mut remaining, request.banner_heights);
    let tabs =
        (tier == SizeTier::Minimal && remaining.height > 1).then(|| take_top(&mut remaining, 1));
    let body = remaining;
    let mut plan = FramePlan {
        tier,
        header,
        tabs,
        banners: banner_rects,
        dropped_banners,
        summary: None,
        placements: Vec::new(),
        hidden: Vec::new(),
        body_note: None,
        body,
        footer,
    };
    match request.view {
        ViewKind::Overview => plan_overview(&mut plan, request),
        _ => plan_detail(&mut plan, request),
    }
    plan
}

/// Banners are allocated before anything else and in priority order; a
/// banner only loses rows when the higher-priority ones already filled the
/// space between header and footer.
fn allocate_banners(remaining: &mut Rect, heights: &[u16]) -> (Vec<Rect>, usize) {
    let mut rects = Vec::new();
    let mut dropped = 0usize;
    for height in heights {
        if remaining.height == 0 {
            dropped += 1;
            continue;
        }
        rects.push(take_top(remaining, (*height).max(1)));
    }
    (rects, dropped)
}

fn plan_overview(plan: &mut FramePlan, request: &LayoutRequest<'_>) {
    let body = plan.body;
    match plan.tier {
        SizeTier::TwoColumn | SizeTier::SingleColumn => plan_grid(plan, request),
        SizeTier::Minimal | SizeTier::TooSmall => {
            plan.summary = (!body.is_empty()).then_some(body);
            plan.hidden
                .extend(request.overview.iter().map(|candidate| candidate.index));
        }
    }
}

fn plan_detail(plan: &mut FramePlan, request: &LayoutRequest<'_>) {
    let mut body = plan.body;
    let compact = plan.tier == SizeTier::Minimal;
    if request.summary_lines == 0 {
        // Nothing to summarize: the detail module gets the whole body.
    } else if plan.tier == SizeTier::TwoColumn && body.width >= DETAIL_SIDEBAR_MIN_TOTAL {
        let main_width = body.width - DETAIL_SIDEBAR_WIDTH - 1;
        let side = Rect::new(
            body.x + main_width + 1,
            body.y,
            DETAIL_SIDEBAR_WIDTH,
            body.height,
        );
        plan.summary = Some(side);
        body.width = main_width;
    } else {
        let summary_rows = match plan.tier {
            SizeTier::SingleColumn => request.summary_lines.clamp(1, 2),
            _ => 1,
        };
        let summary_rows = summary_rows.min(body.height.saturating_sub(1));
        if summary_rows > 0 {
            plan.summary = Some(take_top(&mut body, summary_rows));
        }
    }
    let Some(detail) = request.detail else {
        plan.body_note = Some(body);
        return;
    };
    if detail.min_rows <= body.height && detail.min_columns <= body.width {
        plan.placements.push(Placement {
            index: detail.index,
            area: body,
            slot: Slot::Detail,
            compact,
        });
    } else {
        plan.hidden.push(detail.index);
        plan.body_note = Some(body);
    }
}

/// One overview grid row: its modules, left to right, all as tall.
struct GridRow {
    members: Vec<Candidate>,
}

impl GridRow {
    fn min_rows(&self) -> u16 {
        self.members.iter().map(|c| c.min_rows).max().unwrap_or(1)
    }

    fn priority(&self) -> u8 {
        self.members.iter().map(|c| c.priority).max().unwrap_or(0)
    }

    fn fills(&self) -> bool {
        self.members
            .iter()
            .any(|c| c.cell.is_some_and(|cell| cell.fills))
    }

    /// Growth order: rows with an expanded module first, then by priority.
    fn growth_rank(&self) -> (bool, u8) {
        (self.members.iter().any(|c| c.expanded), self.priority())
    }

    /// The tallest preferred height of the members at their widths, never
    /// below the row minimum; `None` when no member states a preference.
    fn preferred_rows(&self, widths: &[u16], request: &LayoutRequest<'_>) -> Option<u16> {
        self.members
            .iter()
            .zip(widths)
            .filter_map(|(member, width)| (request.preferred_rows)(member.index, *width))
            .max()
            .map(|rows| rows.max(self.min_rows()))
    }
}

/// Overview grid (module docs).
fn plan_grid(plan: &mut FramePlan, request: &LayoutRequest<'_>) {
    let body = plan.body;
    let mut fitting = Vec::new();
    for candidate in request.overview {
        if candidate.min_columns <= body.width {
            fitting.push(*candidate);
        } else {
            plan.hidden.push(candidate.index);
        }
    }
    let mut rows = grid_rows(&fitting, body.width);
    while rows
        .iter()
        .map(GridRow::min_rows)
        .fold(0, u16::saturating_add)
        > body.height
    {
        let lowest = rows
            .iter()
            .enumerate()
            .min_by_key(|(position, row)| (row.priority(), Reverse(*position)))
            .map(|(position, _)| position);
        let Some(lowest) = lowest else { break };
        let dropped = rows.remove(lowest);
        plan.hidden
            .extend(dropped.members.iter().map(|member| member.index));
    }
    let widths: Vec<Vec<u16>> = rows
        .iter()
        .map(|row| member_widths(&row.members, body.width))
        .collect();
    let heights = row_heights(&rows, &widths, body.height, request);
    let mut y = body.y;
    for ((row, widths), height) in rows.iter().zip(&widths).zip(heights) {
        let mut x = body.x;
        for (member, width) in row.members.iter().zip(widths) {
            place(plan, *member, Rect::new(x, y, *width, height));
            x = x.saturating_add(*width);
        }
        y = y.saturating_add(height);
    }
}

/// Groups candidates into rows: by their cell's row (left to right by
/// column), a row that cannot sit side by side split into stacked rows, and
/// one full-width row per candidate without a cell, by priority, last.
fn grid_rows(candidates: &[Candidate], width: u16) -> Vec<GridRow> {
    let mut by_row: BTreeMap<u8, Vec<Candidate>> = BTreeMap::new();
    let mut unplaced: Vec<Candidate> = Vec::new();
    for candidate in candidates {
        match candidate.cell {
            Some(cell) => by_row.entry(cell.row).or_default().push(*candidate),
            None => unplaced.push(*candidate),
        }
    }
    unplaced.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.index.cmp(&b.index)));
    let mut rows = Vec::new();
    for mut members in by_row.into_values() {
        members.sort_by_key(|member| (member.cell.map_or(0, |cell| cell.column), member.index));
        let needed = members
            .iter()
            .map(|member| member.min_columns)
            .fold(0, u16::saturating_add);
        if needed <= width {
            rows.push(GridRow { members });
        } else {
            rows.extend(members.into_iter().map(|member| GridRow {
                members: vec![member],
            }));
        }
    }
    rows.extend(unplaced.into_iter().map(|member| GridRow {
        members: vec![member],
    }));
    rows
}

/// Widths of a row's members: in proportion to their weights, each at least
/// its minimum where the row allows; the last member absorbs the remainder.
fn member_widths(members: &[Candidate], width: u16) -> Vec<u16> {
    let weights: Vec<u32> = members
        .iter()
        .map(|member| u32::from(member.cell.map_or(1, |cell| cell.weight.max(1))))
        .collect();
    let total = weights.iter().sum::<u32>().max(1);
    let mut widths: Vec<u16> = weights
        .iter()
        .map(|weight| saturate_u16(usize::try_from(u32::from(width) * weight / total).unwrap_or(0)))
        .collect();
    for position in 0..members.len() {
        while widths[position] < members[position].min_columns {
            let donor = (0..members.len())
                .filter(|other| *other != position && widths[*other] > members[*other].min_columns)
                .max_by_key(|other| widths[*other] - members[*other].min_columns);
            let Some(donor) = donor else { break };
            widths[donor] -= 1;
            widths[position] += 1;
        }
    }
    let used = widths.iter().copied().fold(0, u16::saturating_add);
    if let Some(last) = widths.last_mut() {
        *last = last.saturating_add(width.saturating_sub(used));
    }
    widths
}

/// Row heights out of `available`: minimums first, then growth toward the
/// preferred heights by priority (filling rows wait), then the rest to the
/// filling rows, or to the last row when none fills.
fn row_heights(
    rows: &[GridRow],
    widths: &[Vec<u16>],
    available: u16,
    request: &LayoutRequest<'_>,
) -> Vec<u16> {
    let mut heights: Vec<u16> = rows.iter().map(GridRow::min_rows).collect();
    let mut spare = available.saturating_sub(heights.iter().copied().fold(0, u16::saturating_add));
    let mut by_rank: Vec<usize> = (0..rows.len()).collect();
    by_rank.sort_by(|a, b| {
        rows[*b]
            .growth_rank()
            .cmp(&rows[*a].growth_rank())
            .then(a.cmp(b))
    });
    for position in by_rank {
        if rows[position].fills() {
            continue;
        }
        if let Some(wanted) = rows[position].preferred_rows(&widths[position], request) {
            let grow = wanted.saturating_sub(heights[position]).min(spare);
            heights[position] += grow;
            spare -= grow;
        }
    }
    let fillers: Vec<usize> = (0..rows.len())
        .filter(|position| rows[*position].fills())
        .collect();
    let receivers = if fillers.is_empty() {
        (rows.len().saturating_sub(1)..rows.len()).collect()
    } else {
        fillers
    };
    if let Some(last) = receivers.last().copied() {
        let share = spare / saturate_u16(receivers.len()).max(1);
        for position in &receivers {
            heights[*position] += share;
            spare -= share;
        }
        heights[last] += spare;
    }
    heights
}

fn place(plan: &mut FramePlan, candidate: Candidate, area: Rect) {
    plan.placements.push(Placement {
        index: candidate.index,
        area,
        slot: Slot::Overview,
        compact: area.height <= candidate.min_rows,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(
        index: usize,
        min_rows: u16,
        priority: u8,
        cell: Option<OverviewCell>,
    ) -> Candidate {
        Candidate {
            index,
            min_columns: 36,
            min_rows,
            priority,
            cell,
            expanded: false,
        }
    }

    /// The built-in overview in the reference grid: 0 cpu and 2 memory on
    /// top, 1 GPUs (top priority), 4 disk and 3 network, then 5 GPU
    /// processes filling.
    fn overview() -> Vec<Candidate> {
        vec![
            candidate(0, 4, 90, Some(OverviewCell::at(0, 0, 58))),
            candidate(1, 4, 95, Some(OverviewCell::at(1, 0, 100))),
            Candidate {
                min_columns: 30,
                ..candidate(2, 3, 80, Some(OverviewCell::at(0, 1, 42)))
            },
            candidate(3, 4, 55, Some(OverviewCell::at(2, 1, 50))),
            candidate(4, 4, 60, Some(OverviewCell::at(2, 0, 50))),
            Candidate {
                min_columns: 40,
                ..candidate(5, 5, 75, Some(OverviewCell::at(3, 0, 100).filling()))
            },
        ]
    }

    fn reference_preferences(index: usize, _: u16) -> Option<u16> {
        match index {
            0 => Some(8),
            1 => Some(12),
            2 => Some(9),
            3 => Some(8),
            4 => Some(10),
            _ => None,
        }
    }

    fn area_of(plan: &FramePlan, index: usize) -> Rect {
        plan.placements
            .iter()
            .find(|placement| placement.index == index)
            .map(|placement| placement.area)
            .unwrap_or_else(|| panic!("module {index} not placed: {plan:?}"))
    }

    fn request<'a>(
        area: Rect,
        view: ViewKind,
        banners: &'a [u16],
        candidates: &'a [Candidate],
        preferred: &'a dyn Fn(usize, u16) -> Option<u16>,
    ) -> LayoutRequest<'a> {
        LayoutRequest {
            area,
            view,
            banner_heights: banners,
            overview: candidates,
            detail: Some(candidate(5, 5, 75, None)),
            summary_lines: 6,
            preferred_rows: preferred,
        }
    }

    fn overview_plan(
        area: Rect,
        banners: &[u16],
        preferred: &dyn Fn(usize, u16) -> Option<u16>,
    ) -> FramePlan {
        let candidates = overview();
        super::plan(&request(
            area,
            ViewKind::Overview,
            banners,
            &candidates,
            preferred,
        ))
    }

    fn no_preference(_: usize, _: u16) -> Option<u16> {
        None
    }

    #[test]
    fn tiers_follow_the_spec_boundaries() {
        assert_eq!(size_tier(160, 50), SizeTier::TwoColumn);
        assert_eq!(size_tier(120, 24), SizeTier::TwoColumn);
        assert_eq!(size_tier(119, 30), SizeTier::SingleColumn);
        assert_eq!(size_tier(100, 30), SizeTier::SingleColumn);
        assert_eq!(size_tier(80, 24), SizeTier::SingleColumn);
        assert_eq!(size_tier(79, 30), SizeTier::Minimal);
        assert_eq!(size_tier(160, 23), SizeTier::Minimal);
        assert_eq!(size_tier(40, 10), SizeTier::Minimal);
        assert_eq!(size_tier(39, 10), SizeTier::TooSmall);
        assert_eq!(size_tier(40, 9), SizeTier::TooSmall);
        assert_eq!(size_tier(20, 5), SizeTier::TooSmall);
    }

    #[test]
    fn a_module_without_a_cell_gets_a_full_width_row_below_the_grid() {
        let mut candidates = overview();
        candidates.push(candidate(10, 3, 10, None));
        let planned = super::plan(&request(
            Rect::new(0, 0, 160, 50),
            ViewKind::Overview,
            &[],
            &candidates,
            &reference_preferences,
        ));
        let extra = area_of(&planned, 10);
        assert_eq!((extra.x, extra.width, extra.height), (0, 160, 3));
        assert_eq!(extra.y, area_of(&planned, 5).bottom());
        assert_eq!(extra.bottom(), planned.body.bottom());
    }

    #[test]
    fn banners_are_allocated_first_and_shrink_the_body() {
        let banners = [5, 1];
        let planned = overview_plan(Rect::new(0, 0, 80, 24), &banners, &no_preference);
        assert_eq!(planned.banners.len(), 2);
        assert_eq!(planned.banners[0].height, 5);
        assert_eq!(planned.dropped_banners, 0);
        assert!(planned.hidden.len() > 1);
        let tall = overview_plan(Rect::new(0, 0, 80, 24), &[30, 3], &no_preference);
        assert_eq!(tall.banners.len(), 1);
        assert_eq!(tall.banners[0].height, 22);
        assert_eq!(tall.dropped_banners, 1);
    }
}
