//! The GPU view's information columns (D-50). The blocks keep their reading
//! order top to bottom, split into as few columns as fit the panel's
//! height (one to four, each at least [`MIN_COLUMN_WIDTH`] wide), balanced
//! so the columns end close together, with a blank row between blocks when
//! every column has room for it. A panel too short for every block fills
//! its columns in order and leaves the rest out. Each block is its title
//! over a rule (a table names its value columns, `now` and `max`, in the
//! rule), then its rows, the keys aligned down the column.

use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::blocks::{Block, BlockRow, Value};
use crate::format::{display_width, pad_right, truncate_to_width};
use crate::modules::common::draw_line;
use crate::num::saturate_u16;
use crate::registry::DisplayContext;

/// Narrowest column: a key and most values whole.
const MIN_COLUMN_WIDTH: u16 = 44;
/// Columns between two columns of blocks.
const COLUMN_GAP: u16 = 3;
/// Most columns a panel shows.
const MAX_COLUMNS: u16 = 4;
/// Columns between a table's two values.
const CELL_GAP: usize = 3;

/// Where the blocks go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Arrangement {
    /// Each column's blocks, as positions in the block list.
    pub(super) columns: Vec<Range<usize>>,
    pub(super) column_width: u16,
    /// Blank rows between two blocks of a column.
    pub(super) gap_rows: u16,
    /// Blocks without room, from the end of the list.
    pub(super) hidden: usize,
}

impl Arrangement {
    /// Rows the tallest column takes.
    pub(super) fn height(&self, blocks: &[Block]) -> u16 {
        self.columns
            .iter()
            .map(|range| column_height(blocks, range.clone(), self.gap_rows))
            .max()
            .unwrap_or(0)
    }
}

/// Places `blocks` in `width` × `height` (module docs).
pub(super) fn arrange(blocks: &[Block], width: u16, height: u16) -> Arrangement {
    let most =
        (width.saturating_add(COLUMN_GAP) / (MIN_COLUMN_WIDTH + COLUMN_GAP)).clamp(1, MAX_COLUMNS);
    let width_of = |count: u16| (width.saturating_sub(COLUMN_GAP * (count - 1)) / count).max(1);
    for count in 1..=most {
        let columns = balanced(blocks, count);
        let fits = |gap| {
            columns
                .iter()
                .all(|range| column_height(blocks, range.clone(), gap) <= height)
        };
        if fits(0) {
            return Arrangement {
                column_width: width_of(count),
                gap_rows: u16::from(fits(1)),
                columns,
                hidden: 0,
            };
        }
    }
    let columns = filled(blocks, most, height);
    let shown = columns.last().map_or(0, |range| range.end);
    Arrangement {
        columns,
        column_width: width_of(most),
        gap_rows: 0,
        hidden: blocks.len() - shown,
    }
}

/// Rows the blocks of `range` take with `gap` rows between them.
fn column_height(blocks: &[Block], range: Range<usize>, gap: u16) -> u16 {
    let count = saturate_u16(range.len());
    blocks[range]
        .iter()
        .map(Block::height)
        .fold(0u16, u16::saturating_add)
        .saturating_add(gap.saturating_mul(count.saturating_sub(1)))
}

/// `blocks` in order into at most `count` columns whose tallest is as short
/// as it can be.
fn balanced(blocks: &[Block], count: u16) -> Vec<Range<usize>> {
    let total = blocks.len();
    if total == 0 {
        return Vec::new();
    }
    let groups = usize::from(count).min(total);
    let mut prefix = vec![0u32; total + 1];
    for (index, block) in blocks.iter().enumerate() {
        prefix[index + 1] = prefix[index] + u32::from(block.height());
    }
    // best[g][i]: the shortest tallest column of the first `i` blocks in
    // `g` columns; cut[g][i]: where the last of those columns starts.
    let mut best = vec![vec![u32::MAX; total + 1]; groups + 1];
    let mut cut = vec![vec![0usize; total + 1]; groups + 1];
    best[0][0] = 0;
    for group in 1..=groups {
        for end in group..=total {
            for start in (group - 1)..end {
                let tallest = best[group - 1][start].max(prefix[end] - prefix[start]);
                if tallest < best[group][end] {
                    best[group][end] = tallest;
                    cut[group][end] = start;
                }
            }
        }
    }
    let mut columns = Vec::with_capacity(groups);
    let mut end = total;
    for group in (1..=groups).rev() {
        let start = cut[group][end];
        columns.push(start..end);
        end = start;
    }
    columns.reverse();
    columns
}

/// `blocks` in order into `count` columns of `height` rows, each taking
/// blocks while they fit; the first block that fits no column ends it.
fn filled(blocks: &[Block], count: u16, height: u16) -> Vec<Range<usize>> {
    let mut columns = Vec::new();
    let mut start = 0;
    for _ in 0..count {
        let mut end = start;
        let mut used = 0u16;
        while end < blocks.len() && used.saturating_add(blocks[end].height()) <= height {
            used += blocks[end].height();
            end += 1;
        }
        if end == start {
            break;
        }
        columns.push(start..end);
        start = end;
    }
    columns
}

/// Draws the columns of `arrangement` from the top of `area`.
pub(super) fn render_columns(
    frame: &mut Frame<'_>,
    area: Rect,
    blocks: &[Block],
    arrangement: &Arrangement,
    context: &DisplayContext,
) {
    for (index, range) in arrangement.columns.iter().enumerate() {
        let x = area.x + saturate_u16(index) * (arrangement.column_width + COLUMN_GAP);
        if x >= area.right() {
            break;
        }
        let width = arrangement.column_width.min(area.right() - x);
        let column = &blocks[range.clone()];
        let key_width = column
            .iter()
            .flat_map(|block| &block.rows)
            .map(|row| display_width(row.key))
            .max()
            .unwrap_or(0)
            + 2;
        let mut y = area.y;
        for block in column {
            for line in block_lines(block, (key_width, width), context) {
                if y >= area.bottom() {
                    return;
                }
                draw_line(frame.buffer_mut(), Rect::new(x, y, width, 1), &line);
                y += 1;
            }
            y = y.saturating_add(arrangement.gap_rows);
        }
    }
}

/// A block's title rule and rows, keys `key_width` wide, in `width`.
fn block_lines(
    block: &Block,
    (key_width, width): (usize, u16),
    context: &DisplayContext,
) -> Vec<Line<'static>> {
    let first_width = block
        .rows
        .iter()
        .filter(|row| row.cells.len() > 1)
        .filter_map(|row| row.cells.first())
        .map(value_width)
        .max()
        .unwrap_or(0);
    let second_x = key_width + first_width + CELL_GAP;
    let mut lines = vec![heading(block, (key_width, second_x), width, context)];
    lines.extend(
        block
            .rows
            .iter()
            .map(|row| row_line(row, (key_width, second_x), width, context)),
    );
    lines
}

/// `Clocks ───── now ─── max ──`: the title, a rule to `width`, a table's
/// headings at its value columns.
fn heading(
    block: &Block,
    (first_x, second_x): (usize, usize),
    width: u16,
    context: &DisplayContext,
) -> Line<'static> {
    let width = usize::from(width);
    let mut rule: Vec<String> = vec!["─".to_owned(); width];
    if let Some([first, second]) = block.headings {
        for (x, heading) in [(first_x, first), (second_x, second)] {
            let start = x.saturating_sub(1);
            for (offset, text) in std::iter::once(" ".to_owned())
                .chain(heading.chars().map(String::from))
                .chain(std::iter::once(" ".to_owned()))
                .enumerate()
            {
                if let Some(cell) = rule.get_mut(start + offset) {
                    *cell = text;
                }
            }
        }
    }
    let title_end = (display_width(block.title) + 1).min(width);
    let theme = &context.theme;
    let mut spans = vec![
        Span::styled(block.title, theme.title_style()),
        Span::raw(" "),
    ];
    let mut run = String::new();
    let mut in_heading = false;
    for cell in rule.into_iter().skip(title_end) {
        let is_rule = cell == "─";
        if is_rule == in_heading && !run.is_empty() {
            spans.push(rule_span(std::mem::take(&mut run), in_heading, context));
        }
        in_heading = !is_rule;
        run.push_str(&cell);
    }
    if !run.is_empty() {
        spans.push(rule_span(run, in_heading, context));
    }
    Line::from(spans)
}

fn rule_span(text: String, heading: bool, context: &DisplayContext) -> Span<'static> {
    let style = if heading {
        context.theme.label_style()
    } else {
        context.theme.border_style(false)
    };
    Span::styled(text, style)
}

/// `graphics      180 MHz    3090 MHz`: the key, then the values, cut to
/// `width`.
fn row_line(
    row: &BlockRow,
    (key_width, second_x): (usize, usize),
    width: u16,
    context: &DisplayContext,
) -> Line<'static> {
    let width = usize::from(width);
    let mut spans = vec![Span::styled(
        pad_right(row.key, key_width),
        context.theme.label_style(),
    )];
    let mut used = key_width;
    for (index, cell) in row.cells.iter().enumerate() {
        if index == 1 && second_x > used {
            spans.push(Span::raw(" ".repeat(second_x - used)));
            used = second_x;
        }
        let (fitted, taken) = fitted(cell, width.saturating_sub(used));
        spans.extend(fitted);
        used += taken;
    }
    Line::from(spans)
}

fn value_width(value: &Value) -> usize {
    value.iter().map(Span::width).sum()
}

/// `spans` cut to `room` columns (the last kept span ends in `…`), with
/// the columns they take.
fn fitted(spans: &[Span<'static>], room: usize) -> (Vec<Span<'static>>, usize) {
    let mut kept = Vec::new();
    let mut used = 0;
    for span in spans {
        let width = span.width();
        if used + width <= room {
            kept.push(span.clone());
            used += width;
            continue;
        }
        let rest = room.saturating_sub(used);
        if rest > 0 {
            let text = truncate_to_width(&span.content, rest);
            used += display_width(&text);
            kept.push(Span::styled(text, span.style));
        }
        break;
    }
    (kept, used)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(title: &'static str, rows: usize) -> Block {
        Block {
            title,
            headings: None,
            rows: (0..rows)
                .map(|_| BlockRow {
                    key: "key",
                    cells: vec![vec![Span::raw("value")]],
                })
                .collect(),
        }
    }

    /// Blocks of the RTX 5080's heights: state, utilization, clocks,
    /// memory, PCIe, thermal, power, CUDA, driver, device, PCI & firmware.
    fn gpu_blocks() -> Vec<Block> {
        [5, 4, 5, 9, 6, 6, 6, 5, 6, 8, 7]
            .into_iter()
            .map(|height| block("Block", height - 1))
            .collect()
    }

    #[test]
    fn a_short_panel_fills_its_columns_in_order_and_leaves_the_rest_out() {
        // 100×30: two columns of 21 rows.
        let blocks = gpu_blocks();
        let arrangement = arrange(&blocks, 98, 21);
        assert_eq!(arrangement.columns, vec![0..3, 3..6]);
        assert_eq!(arrangement.hidden, 5);
        assert_eq!(arrangement.column_width, 47);
        // 80×24: one column.
        let narrow = arrange(&blocks, 78, 15);
        assert_eq!((narrow.columns.len(), narrow.columns[0].clone()), (1, 0..3));
        // Not even the first block fits.
        let none = arrange(&blocks, 78, 2);
        assert!(none.columns.is_empty());
        assert_eq!(none.hidden, 11);
        assert!(arrange(&[], 80, 10).columns.is_empty());
    }
}
