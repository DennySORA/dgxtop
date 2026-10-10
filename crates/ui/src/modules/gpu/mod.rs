//! GPU module: in the overview one panel with, per GPU, a headline, the
//! utilization bar with its chart under it and the memory bar (D-38, D-43);
//! the GPU view shows one GPU: four live bars over its live and deep
//! information in blocks of rows, laid out in columns (D-50, [`view`]).

mod blocks;
mod columns;
mod facts;
mod readings;
mod view;

use dgxtop_core::view::{GpuCard, GpuView, OVERVIEW_GPU_CHART_ROWS, OVERVIEW_GPU_ROWS, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use self::readings::{OVERVIEW_READINGS, overview_readings, render_bar};
use super::ModuleBuildError;
use super::common::{
    ManifestSpec, Output, RowCursor, build_manifest, cell_label, cell_spans, draw_line,
    fraction_ratio, gap, gauge_row, labeled_cell, message, no_data, panel, percent_ratio,
    render_chart, split_columns,
};
use crate::format::{CellDetail, clean, display_width, value_text};
use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, OverviewCell, RenderOutput, Slot, Topic,
    controls,
};

const MAX_CARD_ROWS: u16 = 3;

pub struct GpuModule {
    manifest: DisplayModuleManifest,
}

impl GpuModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: "gpu",
                title: "GPU devices",
                topics: &[Topic::Gpu, Topic::Health, Topic::Sources],
                min_columns: 36,
                min_rows: 4,
                // Above the CPU: the GPU row grows first (D-38).
                priority: 95,
                slots: &[Slot::Overview, Slot::Detail, Slot::Sidebar],
            })?,
        })
    }
}

impl DisplayModule for GpuModule {
    fn manifest(&self) -> &DisplayModuleManifest {
        &self.manifest
    }

    fn overview_cell(&self) -> Option<OverviewCell> {
        Some(OverviewCell::at(1, 0, 100))
    }

    fn preferred_rows(&self, model: &ViewModel, _width: u16, slot: Slot) -> Option<u16> {
        if slot == Slot::Detail {
            return None;
        }
        let Some(gpu) = model.modules.gpu.as_deref() else {
            return Some(3);
        };
        if gpu.cards.is_empty() {
            return Some(3);
        }
        let reason = u16::from(gpu.unavailable_reason.is_some());
        let count = saturate_u16(gpu.cards.len());
        Some(2 + reason + count.saturating_mul(OVERVIEW_GPU_ROWS))
    }

    fn render(
        &self,
        context: &DisplayContext,
        model: &ViewModel,
        area: Rect,
        frame: &mut Frame<'_>,
    ) -> RenderOutput {
        let mut output = Output::default();
        let Some(gpu) = model.modules.gpu.as_deref() else {
            let inner = panel(frame, area, "GPU", context);
            message(frame, inner, &no_data("gpu"), context.theme.muted_style());
            output.label("gpu: no data");
            return output.finish();
        };
        if gpu.cards.is_empty() {
            let inner = panel(frame, area, "GPU", context);
            let reason = gpu.unavailable_reason.as_deref().map_or_else(
                || "no GPUs reported".to_owned(),
                |reason| format!("GPU unavailable: {}", clean(reason)),
            );
            message(frame, inner, &reason, context.theme.muted_style());
            output.label(format!("gpu: {reason}"));
            return output.finish();
        }
        if context.slot == Slot::Detail {
            self.render_detail(context, model, gpu, area, frame, &mut output);
        } else {
            self.render_overview(context, model, gpu, area, frame, &mut output);
        }
        output.finish()
    }
}

impl GpuModule {
    /// The overview GPU panel (module docs); a panel too short for the
    /// gauges falls back to one compact row per GPU.
    fn render_overview(
        &self,
        context: &DisplayContext,
        model: &ViewModel,
        gpu: &GpuView,
        area: Rect,
        frame: &mut Frame<'_>,
        output: &mut Output,
    ) {
        let count = gpu.cards.len();
        let title = format!("GPUs  {count}x");
        let inner = panel(frame, area, &title, context);
        let mut rows = RowCursor::new(inner);
        self.render_partial_reason(gpu, &mut rows, frame, context);
        if rows.remaining() / saturate_u16(count).max(1) < OVERVIEW_GPU_ROWS {
            if let Some(list) = rows.rest() {
                self.render_card_list(context, model, gpu, list, frame, output);
            }
            return;
        }
        let widths = ReadingWidths::of(gpu, model);
        for card in &gpu.cards {
            let Some(block) = rows.take(OVERVIEW_GPU_ROWS) else {
                break;
            };
            render_overview_gpu(frame, block, card, model, widths, context);
            output.entity_target(
                model,
                &self.manifest,
                controls::GPU_CARD,
                block,
                card.entity_id.clone(),
            );
            add_card_labels(output, card, context);
        }
    }

    /// `partial: <reason>` when some GPUs could not be read.
    fn render_partial_reason(
        &self,
        gpu: &GpuView,
        rows: &mut RowCursor,
        frame: &mut Frame<'_>,
        context: &DisplayContext,
    ) {
        if let Some(reason) = gpu.unavailable_reason.as_deref()
            && let Some(row) = rows.next_row()
        {
            let text = format!("partial: {}", clean(reason));
            draw_line(
                frame.buffer_mut(),
                row,
                &Line::from(Span::styled(text, context.theme.label_style())),
            );
        }
    }

    /// One to three rows per GPU when the panel is too short for frames.
    fn render_card_list(
        &self,
        context: &DisplayContext,
        model: &ViewModel,
        gpu: &GpuView,
        area: Rect,
        frame: &mut Frame<'_>,
        output: &mut Output,
    ) {
        let mut rows = RowCursor::new(area);
        let count = saturate_u16(gpu.cards.len());
        let per_card = (rows.remaining() / count.max(1)).clamp(1, MAX_CARD_ROWS);
        let capacity = usize::from(rows.remaining() / per_card);
        let overflow = gpu.cards.len() > capacity && capacity > 1;
        let shown = if overflow {
            capacity - 1
        } else {
            gpu.cards.len().min(capacity)
        };
        for card in gpu.cards.iter().take(shown) {
            let Some(card_area) = rows.take(per_card) else {
                break;
            };
            render_card(frame, card_area, card, context);
            output.entity_target(
                model,
                &self.manifest,
                controls::GPU_CARD,
                card_area,
                card.entity_id.clone(),
            );
            add_card_labels(output, card, context);
        }
        if overflow && let Some(row) = rows.next_row() {
            let text = format!(
                "+{} more GPUs (press 2 for the GPU view)",
                gpu.cards.len() - shown
            );
            frame.buffer_mut().set_stringn(
                row.x,
                row.y,
                text,
                usize::from(row.width),
                context.theme.muted_style(),
            );
        }
    }

    /// The GPU view of the selected GPU ([`view`]).
    fn render_detail(
        &self,
        context: &DisplayContext,
        model: &ViewModel,
        gpu: &GpuView,
        area: Rect,
        frame: &mut Frame<'_>,
        output: &mut Output,
    ) {
        if let Some(selected) = selected_card(gpu, model) {
            view::render_gpu_view(
                &self.manifest,
                context,
                model,
                (gpu, selected),
                area,
                frame,
                output,
            );
        }
    }
}

/// One GPU in the overview (D-43): the headline, the utilization bar with
/// its chart under it, and the memory bar (VRAM, or the RAM a unified GPU's
/// processes hold), which has no chart.
fn render_overview_gpu(
    frame: &mut Frame<'_>,
    area: Rect,
    card: &GpuCard,
    model: &ViewModel,
    widths: ReadingWidths,
    context: &DisplayContext,
) {
    let mut rows = RowCursor::new(area);
    if let Some(row) = rows.next_row() {
        draw_line(frame.buffer_mut(), row, &overview_headline(card, context));
    }
    let [utilization, memory] = overview_readings(card, model);
    let [utilization_width, memory_width] = widths.kinds;
    if let Some(bar) = rows.take(1 + OVERVIEW_GPU_CHART_ROWS) {
        render_bar(
            frame,
            bar,
            &utilization,
            (utilization_width, widths.column),
            context,
        );
    }
    if let Some(bar) = rows.next_row() {
        render_bar(frame, bar, &memory, (memory_width, widths.column), context);
    }
}

/// Value widths of the overview GPU bars: one column for every bar, so all
/// gauges line up, and per reading the widest of its kind, so the
/// utilizations of all GPUs end on one column, as do their memories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadingWidths {
    column: u16,
    kinds: [u16; OVERVIEW_READINGS],
}

impl ReadingWidths {
    fn of(gpu: &GpuView, model: &ViewModel) -> Self {
        let mut kinds = [0; OVERVIEW_READINGS];
        for card in &gpu.cards {
            for (kind, reading) in kinds.iter_mut().zip(overview_readings(card, model)) {
                *kind = (*kind).max(saturate_u16(display_width(&reading.value)));
            }
        }
        Self {
            column: kinds.iter().copied().max().unwrap_or(0),
            kinds,
        }
    }
}

/// `GPU 0  NVIDIA GB10  71°C  ⚡120 W / 250 W  1980 MHz  fan 45%`.
fn overview_headline(card: &GpuCard, context: &DisplayContext) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            format!("{}  ", clean(&card.label)),
            context.theme.title_style(),
        ),
        Span::styled(clean(&card.name), context.theme.text_style()),
        gap(context),
    ];
    spans.extend(cell_spans(&card.temperature, CellDetail::GLANCE, context));
    spans.push(gap(context));
    spans.push(Span::styled("⚡", Style::new().fg(context.theme.warning)));
    spans.extend(cell_spans(&card.power_draw, CellDetail::GLANCE, context));
    spans.push(Span::styled(" / ", context.theme.muted_style()));
    spans.extend(cell_spans(&card.power_limit, CellDetail::GLANCE, context));
    if let Some(clock) = card.clocks.first() {
        spans.push(gap(context));
        spans.extend(cell_spans(&clock.cell, CellDetail::GLANCE, context));
    }
    spans.push(gap(context));
    spans.extend(labeled_cell("fan", &card.fan, CellDetail::GLANCE, context));
    Line::from(spans)
}

/// The GPU shown in detail: projection selection, then session, then first.
fn selected_card<'a>(gpu: &'a GpuView, model: &ViewModel) -> Option<&'a GpuCard> {
    let wanted = gpu
        .selected
        .as_ref()
        .or(model.session.selected_entity_id.as_ref());
    wanted
        .and_then(|id| gpu.cards.iter().find(|card| &card.entity_id == id))
        .or_else(|| gpu.cards.first())
}

fn render_card(frame: &mut Frame<'_>, area: Rect, card: &GpuCard, context: &DisplayContext) {
    let mut rows = RowCursor::new(area);
    if let Some(row) = rows.next_row() {
        draw_line(frame.buffer_mut(), row, &card_headline(card, context));
    }
    if let Some(row) = rows.next_row() {
        let (left, right) = split_columns(row, row.width / 2);
        gauge_row(
            frame,
            left,
            "util",
            5,
            percent_ratio(&card.utilization),
            &value_text(&card.utilization),
            context,
        );
        if card.unified_memory {
            let mut spans = vec![Span::styled("mem ", context.theme.label_style())];
            spans.push(Span::styled("system RAM · ", context.theme.muted_style()));
            spans.extend(labeled_cell(
                "GPU processes",
                &card.process_allocated,
                CellDetail::COMPACT,
                context,
            ));
            draw_line(frame.buffer_mut(), right, &Line::from(spans));
        } else {
            let memory = format!(
                "{} / {}",
                value_text(&card.framebuffer_used),
                value_text(&card.framebuffer_total)
            );
            gauge_row(
                frame,
                right,
                "mem",
                4,
                fraction_ratio(&card.framebuffer_used, &card.framebuffer_total),
                &memory,
                context,
            );
        }
    }
    if let Some(row) = rows.next_row() {
        render_chart(
            frame,
            row,
            &card.utilization_trend.points,
            Some(100.0),
            context,
        );
    }
}

fn card_headline(card: &GpuCard, context: &DisplayContext) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            format!("{} ", clean(&card.label)),
            context.theme.title_style(),
        ),
        Span::styled(clean(&card.name), context.theme.text_style()),
        gap(context),
    ];
    spans.extend(cell_spans(&card.temperature, CellDetail::GLANCE, context));
    spans.push(gap(context));
    spans.extend(cell_spans(&card.power_draw, CellDetail::GLANCE, context));
    spans.push(Span::styled(" / ", context.theme.muted_style()));
    spans.extend(cell_spans(&card.power_limit, CellDetail::GLANCE, context));
    spans.push(gap(context));
    spans.extend(labeled_cell("fan", &card.fan, CellDetail::GLANCE, context));
    Line::from(spans)
}

fn add_card_labels(output: &mut Output, card: &GpuCard, context: &DisplayContext) {
    let label = clean(&card.label);
    output.label(cell_label(
        &format!("gpu {label} utilization"),
        &card.utilization,
        context,
    ));
    output.label(cell_label(
        &format!("gpu {label} temperature"),
        &card.temperature,
        context,
    ));
    output.label(cell_label(
        &format!("gpu {label} power"),
        &card.power_draw,
        context,
    ));
    output.label(cell_label(
        &format!("gpu {label} framebuffer used"),
        &card.framebuffer_used,
        context,
    ));
}
