//! The GPU view (D-47 to D-52): one GPU, top to bottom: the GPU selector;
//! four live bars, utilization, memory, temperature and power
//! ([`readings`](super::readings)); the GPU's live and deep information in
//! blocks of rows ([`blocks`](super::blocks)) laid out in columns
//! ([`columns`](super::columns)). A blank row parts the bars from the
//! blocks when every block fits with it. Blocks a short panel cannot show
//! are named in the selector row; the view has no notes (D-52).

use dgxtop_core::view::{GpuCard, GpuView, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::blocks::{Block, block_text, blocks};
use super::columns::{Arrangement, arrange, render_columns};
use super::readings::{render_bar, status_readings};
use crate::format::{clean, display_width};
use crate::modules::common::{Output, RowCursor, draw_line, panel_with_keys};
use crate::num::saturate_u16;
use crate::registry::{DisplayContext, DisplayModuleManifest, controls};

/// Draws the GPU view of `selected` into `area`.
pub(super) fn render_gpu_view(
    manifest: &DisplayModuleManifest,
    context: &DisplayContext,
    model: &ViewModel,
    (gpu, selected): (&GpuView, &GpuCard),
    area: Rect,
    frame: &mut Frame<'_>,
    output: &mut Output,
) {
    let title = format!("{} · {}", clean(&selected.label), clean(&selected.name));
    let inner = panel_with_keys(frame, area, &title, SWITCH_KEYS, context);
    let mut rows = RowCursor::new(inner);
    let Some(selector) = rows.next_row() else {
        return;
    };
    let readings = status_readings(selected);
    let column = readings
        .iter()
        .map(|reading| saturate_u16(display_width(&reading.value)))
        .max()
        .unwrap_or(0);
    for reading in &readings {
        if let Some(bar) = rows.next_row() {
            render_bar(frame, bar, reading, (0, column), context);
        }
    }
    let blocks = blocks(selected, model.header.kernel.as_deref(), context);
    let body = rows
        .rest()
        .unwrap_or(Rect::new(inner.x, inner.bottom(), inner.width, 0));
    let plan = plan_blocks(&blocks, (body.width, body.height));
    let hidden: Vec<&str> = blocks[blocks.len() - plan.arrangement.hidden..]
        .iter()
        .map(|block| block.title)
        .collect();
    let hint = hidden_blocks(
        &hidden,
        usize::from(selector.width).saturating_sub(selector_width(gpu)),
    );
    render_selector(
        frame,
        selector,
        (manifest, model),
        (gpu, selected),
        hint.as_deref(),
        context,
        output,
    );
    let mut cursor = RowCursor::new(body);
    if plan.separator {
        cursor.next_row();
    }
    let block_height = plan.arrangement.height(&blocks);
    if let Some(area) = cursor.take(block_height) {
        render_columns(frame, area, &blocks, &plan.arrangement, context);
    }
    let label = clean(&selected.label);
    super::add_card_labels(output, selected, context);
    for reading in &readings {
        output.label(format!("gpu {label} {} {}", reading.label, reading.value));
    }
    for block in &blocks {
        output.label(format!("gpu {label} {}", block_text(block)));
    }
}

/// How the blocks fill the rows under the bars.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BlockPlan {
    arrangement: Arrangement,
    /// A blank row between the bars and the blocks.
    separator: bool,
}

/// Every block under a blank row when they fit with it, else the blocks
/// that fit without it.
fn plan_blocks(blocks: &[Block], (width, height): (u16, u16)) -> BlockPlan {
    if height > 0 {
        let arrangement = arrange(blocks, width, height - 1);
        if arrangement.hidden == 0 {
            return BlockPlan {
                arrangement,
                separator: true,
            };
        }
    }
    BlockPlan {
        arrangement: arrange(blocks, width, height),
        separator: false,
    }
}

/// What a short panel leaves out: `+3 sections on a taller terminal`
/// and, when `width` has room, their titles; `None` when every block
/// shows.
fn hidden_blocks(titles: &[&str], width: usize) -> Option<String> {
    let count = match titles.len() {
        0 => return None,
        1 => "+1 section on a taller terminal".to_owned(),
        count => format!("+{count} sections on a taller terminal"),
    };
    let named = format!("{count}: {}", titles.join(", "));
    Some(if display_width(&named) <= width {
        named
    } else {
        count
    })
}

/// The keys acting on the view, at the right of its title (D-70).
const SWITCH_KEYS: &str = "h/l/←/→ switch GPU";
/// Between the hint and what the panel leaves out.
const HIDDEN_GAP: &str = "   ";

/// Columns the selector takes before what the panel leaves out.
fn selector_width(gpu: &GpuView) -> usize {
    let labels: usize = gpu
        .cards
        .iter()
        .map(|card| display_width(&clean(&card.label)) + 3)
        .sum();
    labels + display_width(HIDDEN_GAP)
}

/// `[GPU 0]  GPU 1`, then what the panel could not show.
fn render_selector(
    frame: &mut Frame<'_>,
    row: Rect,
    (manifest, model): (&DisplayModuleManifest, &ViewModel),
    (gpu, selected): (&GpuView, &GpuCard),
    hidden: Option<&str>,
    context: &DisplayContext,
    output: &mut Output,
) {
    let mut x = row.x;
    for card in &gpu.cards {
        let is_selected = card.entity_id == selected.entity_id;
        let text = if is_selected {
            format!("[{}]", clean(&card.label))
        } else {
            format!(" {} ", clean(&card.label))
        };
        let width = saturate_u16(display_width(&text));
        if x.saturating_add(width) > row.right() {
            break;
        }
        let style = if is_selected {
            context.theme.selected_row_style()
        } else {
            context.theme.label_style()
        };
        frame.buffer_mut().set_string(x, row.y, &text, style);
        output.entity_target(
            model,
            manifest,
            controls::GPU_CARD,
            Rect::new(x, row.y, width, 1),
            card.entity_id.clone(),
        );
        x = x.saturating_add(width).saturating_add(1);
    }
    let mut spans = Vec::new();
    if let Some(hidden) = hidden {
        spans.push(Span::raw(HIDDEN_GAP));
        spans.push(Span::styled(hidden.to_owned(), context.theme.label_style()));
    }
    if x < row.right() {
        let rest = Rect::new(x, row.y, row.right() - x, 1);
        draw_line(frame.buffer_mut(), rest, &Line::from(spans));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::modules::gpu::blocks::BlockRow;

    /// Blocks of `heights` rows each, their titles included.
    fn blocks(heights: &[usize]) -> Vec<Block> {
        heights
            .iter()
            .map(|height| Block {
                title: "Block",
                headings: None,
                rows: (1..*height)
                    .map(|_| BlockRow {
                        key: "key",
                        cells: vec![vec![Span::raw("value")]],
                    })
                    .collect(),
            })
            .collect()
    }

    /// The RTX 5080's blocks.
    const GPU_BLOCKS: [usize; 12] = [5, 4, 5, 9, 6, 6, 7, 5, 6, 8, 7, 4];

    #[test]
    fn a_short_panel_shows_the_blocks_that_fit_without_a_separator() {
        // 80×24: 14 rows under the bars; one column.
        let plan = plan_blocks(&blocks(&GPU_BLOCKS), (78, 14));
        assert!(!plan.separator);
        assert_eq!(
            (
                plan.arrangement.columns.len(),
                plan.arrangement.columns[0].clone()
            ),
            (1, 0..3)
        );
        assert_eq!(plan.arrangement.hidden, 9);
        // No rows at all never underflows.
        let plan = plan_blocks(&blocks(&[2]), (78, 0));
        assert!(!plan.separator);
        assert_eq!(plan.arrangement.hidden, 1);
    }

    #[test]
    fn the_selector_names_the_sections_a_short_panel_leaves_out() {
        assert_eq!(hidden_blocks(&[], 80), None);
        assert_eq!(
            hidden_blocks(&["Device"], 80).as_deref(),
            Some("+1 section on a taller terminal: Device")
        );
        let two = ["PCIe", "Device"];
        assert_eq!(
            hidden_blocks(&two, 46).as_deref(),
            Some("+2 sections on a taller terminal: PCIe, Device")
        );
        // Without room for the titles, the count alone.
        assert_eq!(
            hidden_blocks(&two, 45).as_deref(),
            Some("+2 sections on a taller terminal")
        );
    }
}
