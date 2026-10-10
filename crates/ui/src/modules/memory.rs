//! Memory module in the reference layout (D-38): the RAM line with the share
//! GPU processes hold on unified-memory GPUs (D-36); the used bar, which
//! draws that share in its own color, with the used percentage and the used
//! chart under it (D-43); swap, joined by buffers/cache/available when they
//! fit.

use dgxtop_core::view::{MemoryView, MetricCell, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::ModuleBuildError;
use super::common::{
    BarRow, ManifestSpec, Output, RowCursor, build_manifest, cell_label, draw_line, fraction_ratio,
    gap, join_if_fits, measuring_context, message, no_data, panel, render_bar_row, render_chart,
    under_bar,
};
use crate::format::{format_percent, value_text};
use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, OverviewCell, RenderOutput, Slot, Topic,
};
use crate::widgets::{Gauge, Segment, StackedBar};

/// Rows of the used chart under the bar (D-43).
const CHART_ROWS: u16 = 3;
/// Rows of the RAM line, the used bar and its chart.
const RAM_ROWS: u16 = 2 + CHART_ROWS;
/// Value column of the used bar (`100.0%`).
const PERCENT_WIDTH: u16 = 6;
/// Marker of the GPU share, drawn in its color.
const SWATCH: &str = "■ ";

pub struct MemoryModule {
    manifest: DisplayModuleManifest,
}

impl MemoryModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: "memory",
                title: "Memory and swap",
                topics: &[Topic::Memory],
                min_columns: 30,
                min_rows: 3,
                priority: 80,
                slots: &[Slot::Overview],
            })?,
        })
    }
}

impl DisplayModule for MemoryModule {
    fn manifest(&self) -> &DisplayModuleManifest {
        &self.manifest
    }

    fn overview_cell(&self) -> Option<OverviewCell> {
        // Wider than the CPU: on unified memory it carries the GPU's RAM (D-40).
        Some(OverviewCell::at(0, 1, 58))
    }

    fn preferred_rows(&self, model: &ViewModel, width: u16, slot: Slot) -> Option<u16> {
        let Some(memory) = model.modules.memory.as_deref() else {
            return Some(3);
        };
        let context = measuring_context(model, slot);
        let swap_rows = swap_lines(memory, width.saturating_sub(2), &context).len();
        Some(2 + RAM_ROWS + saturate_u16(swap_rows))
    }

    fn render(
        &self,
        context: &DisplayContext,
        model: &ViewModel,
        area: Rect,
        frame: &mut Frame<'_>,
    ) -> RenderOutput {
        let mut output = Output::default();
        let Some(memory) = model.modules.memory.as_deref() else {
            let inner = panel(frame, area, "Memory", context);
            message(
                frame,
                inner,
                &no_data("memory"),
                context.theme.muted_style(),
            );
            output.label("memory: no data");
            return output.finish();
        };
        let inner = panel(frame, area, &title(memory), context);
        let mut rows = RowCursor::new(inner);
        if let Some(row) = rows.next_row() {
            draw_line(frame.buffer_mut(), row, &ram_line(memory, context));
        }
        if let Some(area) = rows.take(1 + CHART_ROWS) {
            render_used(frame, area, memory, context);
        }
        for line in swap_lines(memory, inner.width, context) {
            let Some(row) = rows.next_row() else { break };
            draw_line(frame.buffer_mut(), row, &line);
        }
        output.label(cell_label("memory used", &memory.used, context));
        output.label(cell_label("memory total", &memory.total, context));
        output.label(cell_label("memory available", &memory.available, context));
        if let Some(gpu) = &memory.gpu_resident {
            output.label(cell_label("RAM held by GPU processes", gpu, context));
        }
        output.label(cell_label("swap used", &memory.swap_used, context));
        output.finish()
    }
}

/// `Memory`, noting when a GPU shares the RAM.
fn title(memory: &MemoryView) -> String {
    if memory.gpu_resident.is_some() {
        "Memory · shared with GPU".to_owned()
    } else {
        "Memory".to_owned()
    }
}

/// Used fraction of the RAM, and of it the part GPU processes hold (display
/// arithmetic only; `None` until total and used are known).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Shares {
    used: f64,
    gpu: Option<f64>,
}

impl Shares {
    fn of(memory: &MemoryView) -> Option<Self> {
        let total = memory.total.value_f64().filter(|total| *total > 0.0)?;
        let used = memory.used.value_f64()?.clamp(0.0, total);
        // GPU allocations on unified memory are part of `used`.
        let gpu = memory
            .gpu_resident
            .as_ref()
            .and_then(MetricCell::value_f64)
            .map(|gpu| gpu.clamp(0.0, used) / total);
        Some(Self {
            used: used / total,
            gpu,
        })
    }
}

/// `RAM 12.3 GiB / 64.0 GiB  ■ GPU 8.0 GiB`; the percentage is on the used
/// row below.
fn ram_line(memory: &MemoryView, context: &DisplayContext) -> Line<'static> {
    let theme = &context.theme;
    let mut spans = vec![
        Span::styled("RAM ", theme.label_style()),
        Span::styled(
            format!(
                "{} / {}",
                value_text(&memory.used),
                value_text(&memory.total)
            ),
            theme.text_style(),
        ),
    ];
    if let Some(gpu) = &memory.gpu_resident {
        spans.push(gap(context));
        spans.push(Span::styled(SWATCH, Style::new().fg(theme.accent)));
        spans.push(Span::styled("GPU ", theme.label_style()));
        spans.push(Span::styled(value_text(gpu), theme.text_style()));
    }
    Line::from(spans)
}

/// `used ▆▆▆▆▆▆▆▆▆ 19.2%` (D-43): the used RAM as a bar whose GPU part has
/// the accent color, the used percentage, and the used chart under it
/// against the total; a plain `n/a` gauge until total and used are known.
fn render_used(frame: &mut Frame<'_>, area: Rect, memory: &MemoryView, context: &DisplayContext) {
    let theme = &context.theme;
    let mut rows = RowCursor::new(area);
    let Some(row) = rows.next_row() else { return };
    if let Some(chart) = rows.rest() {
        render_chart(
            frame,
            under_bar(chart),
            &memory.used_trend.points,
            memory.total.value_f64(),
            context,
        );
    }
    let shares = Shares::of(memory);
    let value = shares.map_or_else(
        || value_text(&memory.used),
        |shares| format_percent(shares.used * 100.0),
    );
    let gauge = render_bar_row(
        frame,
        row,
        &BarRow {
            label: "used",
            ratio: shares.map(|shares| shares.used),
            value: &value,
            value_width: PERCENT_WIDTH,
        },
        context,
    );
    let Some(shares) = shares else {
        frame.render_widget(
            Gauge::new(fraction_ratio(&memory.used, &memory.total), "", theme),
            gauge,
        );
        return;
    };
    let gpu = shares.gpu.unwrap_or(0.0);
    let segments = [
        Segment {
            fraction: gpu,
            color: theme.accent,
        },
        Segment {
            fraction: shares.used - gpu,
            color: theme.level_color(shares.used),
        },
    ];
    frame.render_widget(StackedBar::new(&segments, theme.gauge_track), gauge);
}

/// The swap line, joined by the buffers/cache/available line when both fit
/// `width`.
fn swap_lines(memory: &MemoryView, width: u16, context: &DisplayContext) -> Vec<Line<'static>> {
    join_if_fits(
        swap_line(memory, context),
        detail_line(memory, context),
        width,
        context,
    )
}

/// `Swap 0 B / 8.0 GiB`.
fn swap_line(memory: &MemoryView, context: &DisplayContext) -> Line<'static> {
    Line::from(vec![
        Span::styled("Swap ", context.theme.label_style()),
        Span::styled(
            format!(
                "{} / {}",
                value_text(&memory.swap_used),
                value_text(&memory.swap_total)
            ),
            context.theme.text_style(),
        ),
    ])
}

/// `buf 1.2 GiB  cache 20.1 GiB  avail 48.0 GiB`.
fn detail_line(memory: &MemoryView, context: &DisplayContext) -> Line<'static> {
    let theme = &context.theme;
    let mut spans = Vec::new();
    for (label, cell, style) in [
        ("buf", &memory.buffers, theme.text_style()),
        ("cache", &memory.cached, theme.text_style()),
        ("avail", &memory.available, Style::new().fg(theme.success)),
    ] {
        if !spans.is_empty() {
            spans.push(gap(context));
        }
        spans.push(Span::styled(format!("{label} "), theme.label_style()));
        spans.push(Span::styled(value_text(cell), style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::metric::CanonicalUnit;
    use dgxtop_core::view::CellState;

    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

    fn bytes(value: f64) -> MetricCell {
        crate::test_fixtures::ok(value, CanonicalUnit::Bytes)
    }

    fn memory(used: MetricCell, gpu: Option<MetricCell>) -> MemoryView {
        let mut view = crate::test_fixtures::memory_view();
        view.total = bytes(64.0 * GIB);
        view.used = used;
        view.gpu_resident = gpu;
        view
    }

    #[test]
    fn the_gpu_share_is_part_of_the_used_share() {
        let shares = Shares::of(&memory(bytes(16.0 * GIB), Some(bytes(8.0 * GIB)))).unwrap();
        assert_eq!(shares.used, 0.25);
        assert_eq!(shares.gpu, Some(0.125));
        // A GPU figure above `used` (counted at another instant) is capped.
        let capped = Shares::of(&memory(bytes(16.0 * GIB), Some(bytes(40.0 * GIB)))).unwrap();
        assert_eq!(capped.gpu, Some(0.25));
    }

    #[test]
    fn shares_wait_for_total_and_used() {
        let missing = MetricCell::missing(CanonicalUnit::Bytes, CellState::Warmup);
        assert_eq!(Shares::of(&memory(missing, None)), None);
        let mut no_total = memory(bytes(GIB), None);
        no_total.total = bytes(0.0);
        assert_eq!(Shares::of(&no_total), None);
        let unknown_gpu = MetricCell::missing(CanonicalUnit::Bytes, CellState::Unavailable);
        let shares = Shares::of(&memory(bytes(32.0 * GIB), Some(unknown_gpu))).unwrap();
        assert_eq!((shares.used, shares.gpu), (0.5, None));
    }
}
