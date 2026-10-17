//! The GPU view (D-47 to D-52): one GPU, top to bottom: the GPU selector;
//! four live bars, utilization, memory, temperature and power
//! ([`readings`](super::readings)); the GPU's live and deep information in
//! blocks of rows ([`blocks`](super::blocks)) laid out in columns
//! ([`columns`](super::columns)). The selector and the bars stay put; a
//! divider joined to the panel's borders parts them from the blocks when
//! every block fits under it (D-116). Blocks a short panel cannot show at
//! once go on further pages under the divider, which `↑`/`↓` turn (D-110):
//! the page scrolled to is the context's `offset`, and the divider says at
//! its right which page shows, what the next holds and the keys turning
//! pages; the view has no notes (D-52).

use std::ops::Range;

use dgxtop_core::view::{GpuCard, GpuView, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::blocks::{Block, block_text, blocks};
use super::columns::{Arrangement, arrange, render_columns};
use super::readings::{render_bar, status_readings};
use crate::format::{clean, display_width, gpu_with_model};
use crate::modules::common::{
    Output, RowCursor, draw_divider, draw_line, fitting_keys, page_divider_forms,
    styled_panel_with_keys,
};
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
    let readings = status_readings(selected);
    let blocks = blocks(selected, model.header.kernel.as_deref(), context);
    let inner = ratatui::widgets::Block::bordered().inner(area);
    let body_rows = inner
        .height
        .saturating_sub(1 + saturate_u16(readings.len()));
    let pages = paginate(&blocks, (inner.width, body_rows));
    let page = usize::from(context.offset).min(pages.count() - 1);
    let title = format!("{} · {}", clean(&selected.label), clean(&selected.name));
    // The keys turning pages are on the divider (D-116).
    let keys = fitting_keys(area.width, &title, SWITCH_KEYS);
    // The GPU's name in its color (D-123).
    let theme = &context.theme;
    let style = theme.gpu_style(&selected.label, theme.section_title_style(&title));
    let inner = styled_panel_with_keys(frame, area, (&title, style), keys, context);
    let mut rows = RowCursor::new(inner);
    let Some(selector) = rows.next_row() else {
        return;
    };
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
    let body = rows
        .rest()
        .unwrap_or(Rect::new(inner.x, inner.bottom(), inner.width, 0));
    let plan = pages.plan(page, &blocks, (body.width, body.height));
    let hidden = pages.hidden_note(&blocks, &plan, selector_room(selector, gpu));
    render_selector(
        frame,
        selector,
        (manifest, model),
        (gpu, selected),
        hidden.as_deref(),
        context,
        output,
    );
    publish_pages(output, (manifest, model), body, (page, pages.count()));
    let mut cursor = RowCursor::new(body);
    if plan.divider
        && let Some(row) = cursor.next_row()
    {
        let next = pages.titles(page + 1, &blocks);
        let forms = page_divider_forms((page, pages.count()), &next, "section");
        draw_divider(frame.buffer_mut(), row, &forms, context);
    }
    let shown = &blocks[plan.range.clone()];
    let block_height = plan.arrangement.height(shown).min(cursor.remaining());
    if let Some(area) = cursor.take(block_height) {
        render_columns(frame, area, shown, &plan.arrangement, context);
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

/// Tells the controller the pages before and after the one shown, on the
/// sections' rows, so the keys turn pages from what this frame showed.
fn publish_pages(
    output: &mut Output,
    (manifest, model): (&DisplayModuleManifest, &ViewModel),
    body: Rect,
    (page, count): (usize, usize),
) {
    for _ in 0..page {
        output.target(model, manifest, controls::PAGE_ABOVE, body);
    }
    for _ in page + 1..count {
        output.target(model, manifest, controls::PAGE_BELOW, body);
    }
}

/// Where a page's blocks go under the bars.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BlockPlan {
    /// The blocks of the page, as positions in the block list.
    range: Range<usize>,
    /// Their places; `hidden` counts the blocks of the page left out.
    arrangement: Arrangement,
    /// The divider between the bars and the blocks (D-116).
    divider: bool,
}

/// Every block under the divider when they fit with it, else the blocks
/// that fit without it.
fn plan_blocks(blocks: &[Block], (width, height): (u16, u16)) -> BlockPlan {
    if height > 0 {
        let arrangement = arrange(blocks, width, height - DIVIDER_ROWS);
        if arrangement.hidden == 0 {
            return BlockPlan {
                range: 0..blocks.len(),
                arrangement,
                divider: true,
            };
        }
    }
    BlockPlan {
        range: 0..blocks.len(),
        arrangement: arrange(blocks, width, height),
        divider: false,
    }
}

/// Rows of the divider over the blocks.
const DIVIDER_ROWS: u16 = 1;

/// The blocks in pages (module docs): all on one page when they fit or
/// when no row is left for the divider and a block, else pages under the
/// divider, each starting where the one before stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pages {
    /// The first block of each page.
    starts: Vec<usize>,
}

impl Pages {
    fn count(&self) -> usize {
        self.starts.len()
    }

    /// The blocks of `page`, as positions in a list of `total`.
    fn range(&self, page: usize, total: usize) -> Range<usize> {
        let start = self.starts.get(page).copied().unwrap_or(0);
        let end = self.starts.get(page + 1).copied().unwrap_or(total);
        start..end
    }

    /// How `page` fills `width` × `height`: on several pages the blocks of
    /// the page under the divider, a block taller than the rows alone on
    /// its page and cut at the bottom.
    fn plan(&self, page: usize, blocks: &[Block], (width, height): (u16, u16)) -> BlockPlan {
        if self.count() == 1 {
            return plan_blocks(blocks, (width, height));
        }
        let range = self.range(page, blocks.len());
        let rows = height.saturating_sub(DIVIDER_ROWS);
        let mut arrangement = arrange(&blocks[range.clone()], width, rows);
        if arrangement.columns.is_empty() {
            arrangement = Arrangement {
                columns: std::iter::once(0..range.len()).collect(),
                column_width: width,
                gap_rows: 0,
                hidden: 0,
            };
        }
        BlockPlan {
            range,
            arrangement,
            divider: true,
        }
    }

    /// The titles of `page`'s blocks; none past the last page.
    fn titles(&self, page: usize, blocks: &[Block]) -> Vec<String> {
        if page >= self.count() {
            return Vec::new();
        }
        blocks[self.range(page, blocks.len())]
            .iter()
            .map(|block| block.title.to_owned())
            .collect()
    }

    /// The selector row's note, as much as `width` holds: on one page the
    /// blocks with no row left (D-110); the divider says the rest.
    fn hidden_note(&self, blocks: &[Block], plan: &BlockPlan, width: usize) -> Option<String> {
        if self.count() > 1 {
            return None;
        }
        let hidden = &blocks[blocks.len() - plan.arrangement.hidden..];
        let titles: Vec<&str> = hidden.iter().map(|block| block.title).collect();
        hidden_blocks(&titles, width)
    }
}

/// Splits `blocks` into pages of `width` × `height` (see [`Pages`]).
fn paginate(blocks: &[Block], (width, height): (u16, u16)) -> Pages {
    if height <= DIVIDER_ROWS || plan_blocks(blocks, (width, height)).arrangement.hidden == 0 {
        return Pages { starts: vec![0] };
    }
    let rows = height - DIVIDER_ROWS;
    let mut starts = vec![0];
    let mut start = 0;
    loop {
        let rest = &blocks[start..];
        let shown = rest.len() - arrange(rest, width, rows).hidden;
        start += shown.max(1);
        if start >= blocks.len() {
            return Pages { starts };
        }
        starts.push(start);
    }
}

/// What a panel with no row for the blocks leaves out: `+3 sections on a
/// taller terminal` and, when `width` has room, their titles; `None` when
/// every block shows.
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

/// The keys acting on the view, at the right of its title (D-70), the
/// fullest that fits first: the letters stand in for the arrows on a
/// keyboard without them (D-111). The divider names the keys turning pages
/// (D-116).
const SWITCH_KEYS: &[&str] = &["h/l/←/→ switch GPU", "h/l switch GPU"];
/// Between the hint and what the panel leaves out.
const HIDDEN_GAP: &str = "   ";

/// Columns of `row` left for the note after the selector's chips.
fn selector_room(row: Rect, gpu: &GpuView) -> usize {
    usize::from(row.width).saturating_sub(selector_width(gpu))
}

/// Columns the selector takes before what the panel leaves out.
fn selector_width(gpu: &GpuView) -> usize {
    let labels: usize = gpu
        .cards
        .iter()
        .map(|card| display_width(&chip(card)) + 3)
        .sum();
    labels + display_width(HIDDEN_GAP)
}

/// `GPU 0 · RTX 5080`: a card's label and short model, so the selector
/// tells which card each is (D-122).
fn chip(card: &GpuCard) -> String {
    gpu_with_model(&clean(&card.label), &clean(&card.name))
}

/// `[GPU 0 · RTX 5080]  GPU 1 · RTX 3060`, each in its GPU's color (D-123),
/// then what the panel could not show.
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
            format!("[{}]", chip(card))
        } else {
            format!(" {} ", chip(card))
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
        let style = context.theme.gpu_style(&card.label, style);
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
    fn a_short_panel_shows_the_blocks_that_fit_without_a_divider() {
        // 80×24: 14 rows under the bars; one column.
        let plan = plan_blocks(&blocks(&GPU_BLOCKS), (78, 14));
        assert!(!plan.divider);
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
        assert!(!plan.divider);
        assert_eq!(plan.arrangement.hidden, 1);
    }

    /// Sections that do not fit go on pages (D-110) under the divider
    /// (D-116): each page holds what fits from where the one before
    /// stopped, every section once, in order; the divider names the next
    /// page's sections, the selector row nothing.
    #[test]
    fn sections_too_many_for_the_panel_go_on_pages() {
        let all = blocks(&GPU_BLOCKS);
        // 80×24: 14 rows under the bars, the divider and 13 rows a page,
        // one column.
        let pages = paginate(&all, (78, 14));
        assert!(pages.count() > 1);
        let mut covered = Vec::new();
        for page in 0..pages.count() {
            let plan = pages.plan(page, &all, (78, 14));
            assert!(plan.divider, "page {page} under the divider");
            assert_eq!(plan.arrangement.hidden, 0, "page {page} fits");
            assert!(plan.arrangement.height(&all[plan.range.clone()]) <= 13);
            covered.extend(plan.range);
        }
        assert_eq!(covered, (0..all.len()).collect::<Vec<_>>());
        let next = pages.range(1, all.len()).len();
        assert_eq!(pages.titles(1, &all), vec!["Block"; next]);
        assert!(pages.titles(pages.count(), &all).is_empty());
        let first = pages.plan(0, &all, (78, 14));
        assert_eq!(pages.hidden_note(&all, &first, 200), None);
        // Every section fits: one page, under the divider.
        let roomy = paginate(&all, (200, 40));
        assert_eq!(roomy.count(), 1);
        let plan = roomy.plan(0, &all, (200, 40));
        assert!(plan.divider);
        assert_eq!(roomy.hidden_note(&all, &plan, 200), None);
        // A section taller than the rows is alone on its page, cut.
        let tall = blocks(&[3, 30, 3]);
        let pages = paginate(&tall, (78, 10));
        assert_eq!(pages.starts, vec![0, 1, 2]);
        let plan = pages.plan(1, &tall, (78, 10));
        assert_eq!(plan.range, 1..2);
        assert_eq!(plan.arrangement.columns.len(), 1);
        assert_eq!(plan.arrangement.columns[0], 0..1);
        // No row for the divider and a section: one page, and the selector
        // row names them.
        let none = paginate(&all, (78, 1));
        assert_eq!(none.count(), 1);
        let plan = none.plan(0, &all, (78, 1));
        assert!(
            none.hidden_note(&all, &plan, 40)
                .unwrap()
                .starts_with("+12 sections")
        );
    }

    /// The selector and the bars stay put over the divider joined to the
    /// panel's borders (D-116), which says the page, what the next holds
    /// and the keys turning pages; the title keeps the keys switching the
    /// GPU. Every section on one page leaves a bare divider.
    #[test]
    fn the_divider_names_the_pages_under_the_fixed_bars() {
        use crate::registry::DisplayModule;
        let draw = |offset: u16, (width, height): (u16, u16)| -> Vec<String> {
            let mut context = crate::test_fixtures::display_context();
            context.slot = crate::registry::Slot::Detail;
            context.offset = offset;
            let model =
                crate::test_fixtures::view_model(dgxtop_core::commands::ViewKind::GpuDetail, 1);
            let module = crate::modules::gpu::GpuModule::new().unwrap();
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    module.render(&context, &model, frame.area(), frame);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..height)
                .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
                .collect()
        };
        // The title, the selector and four bars, then the divider.
        const DIVIDER: usize = 6;
        let first = draw(0, (137, 38));
        let text = first.join("\n");
        assert!(first[0].contains("(h/l/←/→ switch GPU)"), "{text}");
        assert!(
            !first[0].contains("page") && !first[1].contains("page"),
            "{text}"
        );
        let divider = &first[DIVIDER];
        assert!(divider.starts_with('├'), "{text}");
        assert!(divider.contains("─ page 1 of 2 · next: "), "{text}");
        assert!(divider.ends_with(" (j/k/↑/↓ page) ┤"), "{text}");
        let second = draw(1, (137, 38));
        assert_eq!(second[..DIVIDER], first[..DIVIDER], "the bars stay put");
        assert!(
            second[DIVIDER].ends_with("─ page 2 of 2 (j/k/↑/↓ page) ┤"),
            "{}",
            second.join("\n")
        );
        let roomy = draw(0, (213, 60));
        assert!(
            roomy[DIVIDER].starts_with("├─")
                && roomy[DIVIDER].trim_matches(['├', '─', '┤']).is_empty(),
            "{}",
            roomy.join("\n")
        );
    }

    /// The title names the GPU in its color, and each GPU's chip is in its
    /// own (D-123), the selected one also highlighted and bold.
    #[test]
    fn the_selector_draws_each_gpu_in_its_color() {
        use crate::registry::DisplayModule;
        let mut context = crate::test_fixtures::display_context();
        context.slot = crate::registry::Slot::Detail;
        let model = crate::test_fixtures::view_model(dgxtop_core::commands::ViewKind::GpuDetail, 1);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(137, 38)).unwrap();
        terminal
            .draw(|frame| {
                let module = crate::modules::gpu::GpuModule::new().unwrap();
                module.render(&context, &model, frame.area(), frame);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let theme = &context.theme;
        let title: String = (0..137).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(title.starts_with("╭ GPU 0 · NVIDIA GB10 "), "{title}");
        assert_eq!(buffer[(2, 0)].fg, theme.gpu_color(0));
        let row: String = (0..137).map(|x| buffer[(x, 1)].symbol()).collect();
        for (number, chip) in [(0, "[GPU 0 · GB10]"), (1, " GPU 1 · GB10 ")] {
            let at = row.find(chip).unwrap_or_else(|| panic!("{chip} in {row}"));
            let x = u16::try_from(row[..at].chars().count()).unwrap() + 1;
            assert_eq!(buffer[(x, 1)].fg, theme.gpu_color(number), "{chip}");
        }
        let selected = &buffer[(2, 1)];
        assert_eq!(selected.bg, theme.highlight_background);
        assert!(selected.modifier.contains(ratatui::style::Modifier::BOLD));
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
