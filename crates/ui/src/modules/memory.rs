//! Memory module in the reference layout (D-38): the RAM line with the share
//! GPU processes hold on unified-memory GPUs (D-36) and, of it, what each
//! tracked process holds (D-64); the used bar, which draws each process's
//! part in its color and the other GPU processes in gray, with the used
//! percentage and the used chart under it, stacked the same way (D-43);
//! swap, joined by buffers/cache/available when they fit.

use dgxtop_core::view::{MemoryView, MetricCell, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::ModuleBuildError;
use super::common::{
    BarRow, ManifestSpec, Output, RowCursor, build_manifest, cell_label, draw_line, fraction_ratio,
    gap, join_if_fits, measuring_context, message, no_data, panel, render_bar_row,
    render_chart_with_parts, under_bar,
};
use super::process_memory::{SWATCH, legend, other_points, share_segments, trend_parts};
use crate::format::{clean, format_bytes, format_percent, value_text};
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
            draw_line(
                frame.buffer_mut(),
                row,
                &ram_line(memory, row.width, context),
            );
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
        for share in &memory.gpu_processes {
            output.label(format!(
                "RAM held by GPU process pid {} {}: {}",
                share.pid,
                clean(&share.name),
                format_bytes(share.bytes)
            ));
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
    /// The RAM total in bytes, the whole of the bar.
    total: f64,
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
            total,
            used: used / total,
            gpu,
        })
    }

    /// The bar's segments: each tracked GPU process in its color, the other
    /// GPU processes in gray, then the rest of the used RAM in the color of
    /// the used share's level. Without a GPU figure the processes' parts
    /// stay within the used RAM.
    fn segments(&self, memory: &MemoryView, context: &DisplayContext) -> Vec<Segment> {
        let theme = &context.theme;
        let gpu = self.gpu.unwrap_or(0.0);
        let room = self.gpu.unwrap_or(self.used) * self.total;
        let (mut segments, tracked) =
            share_segments(&memory.gpu_processes, self.total, room, context);
        let attributed = gpu.max(tracked / self.total);
        segments.push(Segment {
            fraction: attributed - tracked / self.total,
            color: theme.other_processes_color(),
        });
        segments.push(Segment {
            fraction: self.used - attributed,
            color: theme.level_color(self.used),
        });
        segments
    }
}

/// `RAM 90.0 GiB / 122 GiB  GPU 69.8 GiB  ■ train 40.0 GiB  ■ serve …`: the
/// RAM in use and, on unified memory, what GPU processes hold of it with
/// each tracked process (as many as fit `width`) and the other processes;
/// `■ GPU 69.8 GiB` in gray while no process is tracked. The percentage is
/// on the used row below.
fn ram_line(memory: &MemoryView, width: u16, context: &DisplayContext) -> Line<'static> {
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
    let Some(gpu) = &memory.gpu_resident else {
        return Line::from(spans);
    };
    spans.push(gap(context));
    if memory.gpu_processes.is_empty() {
        spans.push(Span::styled(
            SWATCH,
            Style::new().fg(theme.other_processes_color()),
        ));
    }
    spans.push(Span::styled("GPU ", theme.label_style()));
    spans.push(Span::styled(value_text(gpu), theme.text_style()));
    if !memory.gpu_processes.is_empty() {
        let used: usize = spans.iter().map(Span::width).sum();
        let left = usize::from(width).saturating_sub(used);
        spans.extend(legend(
            &memory.gpu_processes,
            other_bytes(memory),
            left,
            context,
        ));
    }
    Line::from(spans)
}

/// What the GPU processes without a color hold: the GPU share less the
/// tracked processes (display arithmetic).
fn other_bytes(memory: &MemoryView) -> Option<u64> {
    let gpu = memory.gpu_resident.as_ref()?.value_f64()?;
    #[allow(clippy::cast_precision_loss)]
    let tracked: f64 = memory
        .gpu_processes
        .iter()
        .map(|share| share.bytes as f64)
        .sum();
    let other = (gpu - tracked).max(0.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bytes = other.round() as u64;
    Some(bytes)
}

/// `used ▆▆▆▆▆▆▆▆▆ 19.2%` (D-43): the used RAM as a bar whose GPU part shows
/// each tracked process in its color ([`Shares::segments`]), the used
/// percentage, and the used chart under it against the total, stacked the
/// same way; a plain `n/a` gauge until total and used are known.
fn render_used(frame: &mut Frame<'_>, area: Rect, memory: &MemoryView, context: &DisplayContext) {
    let theme = &context.theme;
    let mut rows = RowCursor::new(area);
    let Some(row) = rows.next_row() else { return };
    if let Some(chart) = rows.rest() {
        let other = other_points(
            &memory.gpu_resident_trend.points,
            &memory.gpu_process_trends,
        );
        let mut parts = trend_parts(&memory.gpu_process_trends, context);
        if memory.gpu_resident.is_some() {
            parts.push((
                other.as_slice(),
                Style::new().fg(theme.other_processes_color()),
            ));
        }
        render_chart_with_parts(
            frame,
            under_bar(chart),
            &memory.used_trend.points,
            parts,
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
    let segments = shares.segments(memory, context);
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
    use dgxtop_core::view::{CellState, ProcessShare, ProcessTrend, Trend};

    /// Label column of a bar row (`used`), before its gauge.
    const BAR_LABEL: u16 = crate::modules::common::BAR_LABEL_WIDTH;

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

    fn draw_used(view: &MemoryView, size: (u16, u16)) -> ratatui::buffer::Buffer {
        let context = crate::test_fixtures::display_context();
        let area = Rect::new(0, 0, size.0, size.1);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(size.0, size.1)).unwrap();
        terminal
            .draw(|frame| render_used(frame, area, view, &context))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn share(slot: u8, gib: f64) -> ProcessShare {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let bytes = (gib * GIB) as u64;
        ProcessShare {
            slot,
            pid: 40 + i32::from(slot),
            name: format!("job{slot}"),
            bytes,
        }
    }

    /// The used chart under the bar draws the GPU's share at the bottom of
    /// each column: gray while no process is tracked, like the bar above it.
    #[test]
    fn the_used_chart_shows_the_gpu_share_of_untracked_processes_in_gray() {
        let theme = crate::test_fixtures::display_context().theme;
        let mut view = memory(bytes(48.0 * GIB), Some(bytes(32.0 * GIB)));
        view.used_trend.points = vec![Some(48.0 * GIB); 4];
        view.gpu_resident_trend.points = vec![Some(32.0 * GIB); 4];
        let buffer = draw_used(&view, (30, 4));
        let (right, bottom) = (buffer.area.right() - 1, buffer.area.bottom() - 1);
        assert_eq!(buffer[(right, bottom)].fg, theme.other_processes_color());
        // Without a share the column keeps the gauge's level color.
        view.gpu_resident_trend.points.clear();
        let buffer = draw_used(&view, (30, 4));
        assert_eq!(buffer[(right, bottom)].fg, theme.level_color(0.75));
    }

    /// Each tracked process has its part of the bar and of every column in
    /// its color, from the foot: the first process, the second above it,
    /// the other GPU processes in gray, the rest of the used RAM in its
    /// level color, then the track.
    #[test]
    fn the_bar_and_the_chart_show_each_tracked_process_in_its_color() {
        let theme = crate::test_fixtures::display_context().theme;
        let mut view = memory(bytes(48.0 * GIB), Some(bytes(32.0 * GIB)));
        view.gpu_processes = vec![share(0, 16.0), share(1, 8.0)];
        view.used_trend.points = vec![Some(48.0 * GIB); 2];
        view.gpu_resident_trend.points = vec![Some(32.0 * GIB); 2];
        view.gpu_process_trends = vec![
            ProcessTrend {
                slot: 0,
                trend: Trend {
                    points: vec![Some(16.0 * GIB); 2],
                    step_ms: 1000,
                },
            },
            ProcessTrend {
                slot: 1,
                trend: Trend {
                    points: vec![Some(8.0 * GIB)],
                    step_ms: 1000,
                },
            },
        ];
        // 64 columns of bar: 16 job0, 8 job1, 8 others, 16 used, 16 track.
        let buffer = draw_used(&view, (BAR_LABEL + 64 + 1 + 6, 5));
        let bar: Vec<_> = (BAR_LABEL..BAR_LABEL + 64)
            .map(|x| buffer[(x, 0)].fg)
            .collect();
        let mut expected = vec![theme.process_color(0); 16];
        expected.extend([theme.process_color(1); 8]);
        expected.extend([theme.other_processes_color(); 8]);
        expected.extend([theme.level_color(0.75); 16]);
        expected.extend([theme.gauge_track; 16]);
        assert_eq!(bar, expected);
        // The newest column of the chart (four rows, 30 eighths): job0 to
        // 7.5 → 8 eighths, job1 to 11.25 → 11, the others to 15, the rest
        // to 22.5 → 23, three rows high.
        let x = buffer.area.right() - 1;
        let column: Vec<_> = (2..5).rev().map(|y| buffer[(x, y)].fg).collect();
        assert_eq!(
            column,
            vec![
                theme.process_color(0),
                theme.process_color(1),
                theme.level_color(0.75),
            ]
        );
        // job1 ends in the second row, the others fill most of the rest.
        assert_eq!(buffer[(x, 3)].bg, theme.other_processes_color());
        assert_eq!(buffer[(x, 1)].symbol(), " ");
        // The column before it has job1's point missing: job0, then gray.
        assert_eq!(buffer[(x - 1, 3)].fg, theme.other_processes_color());
    }

    #[test]
    fn the_ram_line_names_each_tracked_process_while_it_fits() {
        let context = crate::test_fixtures::display_context();
        let mut view = memory(bytes(48.0 * GIB), Some(bytes(32.0 * GIB)));
        let text = |line: &Line<'_>| -> String {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        view.total = bytes(64.0 * GIB);
        assert_eq!(
            text(&ram_line(&view, 120, &context)),
            "RAM 48.0 GiB / 64.0 GiB  ■ GPU 32.0 GiB"
        );
        view.gpu_processes = vec![share(0, 16.0), share(1, 8.0)];
        assert_eq!(
            text(&ram_line(&view, 120, &context)),
            "RAM 48.0 GiB / 64.0 GiB  GPU 32.0 GiB  ■ job0 16.0 GiB  ■ job1 8.0 GiB  ■ other 8.0 GiB"
        );
        assert_eq!(
            text(&ram_line(&view, 60, &context)),
            "RAM 48.0 GiB / 64.0 GiB  GPU 32.0 GiB  ■ job0 16.0 GiB  +2"
        );
    }
}
