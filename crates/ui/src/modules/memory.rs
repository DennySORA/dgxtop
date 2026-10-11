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
    join_if_fits, measuring_context, message, no_data, panel, panel_with_keys, render_bar_row,
    render_chart_with_parts, separator, under_bar,
};
use super::process_memory::{
    SWATCH, legend_line_count, legend_lines, other_points, share_segments, trend_parts,
};
use crate::format::{clean, format_bytes, format_percent, value_text};
use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, OverviewCell, RenderOutput, Slot, Topic,
    controls,
};
use crate::widgets::{Gauge, Segment, StackedBar};

/// Rows of the used chart under the bar (D-43).
const CHART_ROWS: u16 = 3;
/// Rows of the RAM line, the used bar and its chart.
const RAM_ROWS: u16 = 2 + CHART_ROWS;
/// Module id of the memory panel, whose GPU legend scrolls (D-70).
pub const MEMORY_MODULE_ID: &str = "memory";
/// Most lines the GPU legend takes (D-69); the rest scrolls (D-70).
const MAX_GPU_ROWS: usize = 3;
/// Columns the legend keeps free for its scroll hint (`  ▲9 ▼9`).
const SCROLL_HINT_WIDTH: usize = 8;
/// The keys acting on the panel while its GPU legend scrolls (D-70).
const SCROLL_KEYS: &str = "[/] scroll GPU";
/// What the system itself holds of a RAM shared with a GPU.
const OS_LABEL: &str = "OS";
/// Value column of the used bar (`100.0%`).
const PERCENT_WIDTH: u16 = 6;

pub struct MemoryModule {
    manifest: DisplayModuleManifest,
}

impl MemoryModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: MEMORY_MODULE_ID,
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
        let inner = width.saturating_sub(2);
        let swap_rows = swap_lines(memory, inner, &context).len();
        let gpu_rows = gpu_legend_rows(memory, inner, MAX_GPU_ROWS, &context);
        Some(2 + RAM_ROWS + saturate_u16(gpu_rows + swap_rows))
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
        let width = area.width.saturating_sub(2);
        let keys = if gpu_legend_rows(memory, width, usize::MAX, context) > MAX_GPU_ROWS {
            SCROLL_KEYS
        } else {
            ""
        };
        let inner = panel_with_keys(frame, area, &title(memory), keys, context);
        let mut rows = RowCursor::new(inner);
        if let Some(row) = rows.next_row() {
            draw_line(frame.buffer_mut(), row, &ram_line(memory, context));
        }
        let swap = swap_lines(memory, inner.width, context);
        // The bar, its chart and the swap rows keep their rows; the GPU
        // legend takes what is left, up to its most, and scrolls (D-70).
        let room = usize::from(rows.remaining())
            .saturating_sub(usize::from(1 + CHART_ROWS) + swap.len())
            .clamp(1, MAX_GPU_ROWS);
        let legend = gpu_legend(memory, inner.width, (room, context.scroll), context);
        if let Some(area) = rows.take(saturate_u16(legend.lines.len())) {
            for (row, line) in (area.y..area.bottom()).zip(&legend.lines) {
                draw_line(
                    frame.buffer_mut(),
                    Rect::new(area.x, row, area.width, 1),
                    line,
                );
            }
            if legend.scrolls {
                output.target(model, &self.manifest, controls::MODULE_SCROLL, area);
            }
        }
        // The chart takes the rows the panel has beyond its own, so it fills
        // a row the CPU panel beside it made taller (D-70).
        let chart_rows = rows
            .remaining()
            .saturating_sub(saturate_u16(swap.len()))
            .max(1 + CHART_ROWS);
        if let Some(area) = rows.take(chart_rows) {
            render_used(frame, area, memory, context);
        }
        for line in swap {
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
        let rest = if self.gpu.is_some() {
            theme.os_memory_color()
        } else {
            theme.level_color(self.used)
        };
        segments.push(Segment {
            fraction: self.used - attributed,
            color: rest,
        });
        segments
    }
}

/// `RAM 90.0 GiB / 122 GiB │ ■ OS 20.2 GiB` (D-69): the RAM in use and, on
/// unified memory, what the system itself holds of it (the used RAM less
/// the GPU share) in its fixed color; the GPU legend follows on its own
/// lines. The percentage is on the used row below.
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
    if let Some(host) = host_bytes(memory) {
        spans.push(separator(context));
        spans.push(Span::styled(
            SWATCH,
            Style::new().fg(theme.os_memory_color()),
        ));
        spans.push(Span::styled(format!("{OS_LABEL} "), theme.label_style()));
        spans.push(Span::styled(format_bytes(host), theme.text_style()));
    }
    Line::from(spans)
}

/// The visible lines of the GPU legend and whether more of it scrolls.
struct GpuLegend {
    lines: Vec<Line<'static>>,
    scrolls: bool,
}

/// The GPU legend of a RAM shared with a GPU (D-69): the tracked processes
/// and the other GPU processes, or all GPU memory in gray while no process
/// is tracked, wrapped to `width`. `shown` lines from `scroll` on are drawn
/// (D-70); `▲`/`▼` with a count end the last line while lines are hidden
/// above or below (the panel's title names the keys).
fn gpu_legend(
    memory: &MemoryView,
    width: u16,
    (shown, scroll): (usize, u16),
    context: &DisplayContext,
) -> GpuLegend {
    if memory.gpu_resident.is_none() || shown == 0 {
        return GpuLegend {
            lines: Vec::new(),
            scrolls: false,
        };
    }
    let hint_width = SCROLL_HINT_WIDTH.min(usize::from(width) / 3);
    let mut all = legend_lines(
        &memory.gpu_processes,
        legend_other(memory),
        usize::from(width),
        usize::MAX,
        context,
    );
    if all.len() > shown {
        // Leave room for the hint at the end of each line.
        all = legend_lines(
            &memory.gpu_processes,
            legend_other(memory),
            usize::from(width).saturating_sub(hint_width),
            usize::MAX,
            context,
        );
    }
    let total = all.len();
    if total <= shown {
        return GpuLegend {
            lines: all,
            scrolls: false,
        };
    }
    let start = usize::from(scroll).min(total - shown);
    let mut lines: Vec<Line<'static>> = all.into_iter().skip(start).take(shown).collect();
    let (above, below) = (start, total - start - shown);
    if let Some(last) = lines.last_mut() {
        let mut hint = String::from("  ");
        if above > 0 {
            hint.push_str(&format!("▲{above} "));
        }
        if below > 0 {
            hint.push_str(&format!("▼{below} "));
        }
        last.spans.push(Span::styled(
            hint.trim_end().to_owned(),
            context.theme.muted_style(),
        ));
    }
    GpuLegend {
        lines,
        scrolls: true,
    }
}

fn gpu_legend_rows(
    memory: &MemoryView,
    width: u16,
    max_lines: usize,
    context: &DisplayContext,
) -> usize {
    if memory.gpu_resident.is_none() {
        return 0;
    }
    legend_line_count(
        &memory.gpu_processes,
        legend_other(memory),
        usize::from(width),
        max_lines,
        context,
    )
}

/// The GPU memory no tracked process accounts for.
fn legend_other(memory: &MemoryView) -> Option<u64> {
    other_bytes(memory)
}

/// What the CPU side holds of the used RAM: the used RAM less the GPU share
/// (display arithmetic).
fn host_bytes(memory: &MemoryView) -> Option<u64> {
    let used = memory.used.value_f64()?;
    let gpu = memory.gpu_resident.as_ref()?.value_f64()?;
    let host = (used - gpu).max(0.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bytes = host.round() as u64;
    Some(bytes)
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
            (
                parts,
                memory
                    .gpu_resident
                    .is_some()
                    .then(|| theme.os_memory_color()),
            ),
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
            spans.push(separator(context));
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
        // Without a share the column is what the system holds, in its fixed
        // color (D-69).
        view.gpu_resident_trend.points.clear();
        let buffer = draw_used(&view, (30, 4));
        assert_eq!(buffer[(right, bottom)].fg, theme.os_memory_color());
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
        expected.extend([theme.os_memory_color(); 16]);
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
                theme.os_memory_color(),
            ]
        );
        // job1 ends in the second row, the others fill most of the rest.
        assert_eq!(buffer[(x, 3)].bg, theme.other_processes_color());
        assert_eq!(buffer[(x, 1)].symbol(), " ");
        // The column before it has job1's point missing: job0, then gray.
        assert_eq!(buffer[(x - 1, 3)].fg, theme.other_processes_color());
    }

    /// The RAM line names what the system holds in its fixed color; the GPU
    /// processes follow on lines of their own, wrapped (D-69).
    #[test]
    fn the_ram_line_names_the_system_and_the_gpu_legend_wraps_below() {
        let context = crate::test_fixtures::display_context();
        let mut view = memory(bytes(48.0 * GIB), Some(bytes(32.0 * GIB)));
        let text = |line: &Line<'_>| -> String {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        let legend = |view: &MemoryView, width: u16| -> Vec<String> {
            gpu_legend(view, width, (MAX_GPU_ROWS, 0), &context)
                .lines
                .iter()
                .map(text)
                .collect()
        };
        view.total = bytes(64.0 * GIB);
        let ram = ram_line(&view, &context);
        assert_eq!(text(&ram), "RAM 48.0 GiB / 64.0 GiB │ ■ OS 16.0 GiB");
        let swatch = ram
            .spans
            .iter()
            .find(|span| span.content == SWATCH)
            .unwrap();
        assert_eq!(swatch.style.fg, Some(context.theme.os_memory_color()));
        assert_eq!(legend(&view, 120), vec!["[GPU] ■ other 32.0 GiB"]);
        view.gpu_processes = vec![share(0, 16.0), share(1, 8.0)];
        assert_eq!(
            legend(&view, 120),
            vec!["[GPU] ■ job0 16.0 GiB  ■ job1 8.0 GiB  ■ other 8.0 GiB"]
        );
        assert_eq!(
            legend(&view, 40),
            vec![
                "[GPU] ■ job0 16.0 GiB  ■ job1 8.0 GiB",
                "      ■ other 8.0 GiB"
            ]
        );
        assert_eq!(gpu_legend_rows(&view, 40, MAX_GPU_ROWS, &context), 2);
        // A discrete GPU's memory is not in the RAM: no OS part, no legend.
        view.gpu_resident = None;
        assert_eq!(text(&ram_line(&view, &context)), "RAM 48.0 GiB / 64.0 GiB");
        assert!(legend(&view, 120).is_empty());
    }

    /// Beyond its most lines the GPU legend scrolls: the lines from the
    /// offset on, with what is hidden above and below and the keys (D-70).
    #[test]
    fn the_gpu_legend_scrolls_past_its_most_lines() {
        let context = crate::test_fixtures::display_context();
        let mut view = memory(bytes(60.0 * GIB), Some(bytes(56.0 * GIB)));
        view.total = bytes(64.0 * GIB);
        view.gpu_processes = (0..8).map(|slot| share(slot, 7.0)).collect();
        let shown = |scroll: u16| -> (Vec<String>, bool) {
            let legend = gpu_legend(&view, 52, (2, scroll), &context);
            let lines = legend
                .lines
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect()
                })
                .collect();
            (lines, legend.scrolls)
        };
        let (top, scrolls) = shown(0);
        assert!(scrolls);
        assert_eq!(top.len(), 2);
        assert!(top[0].starts_with("[GPU] ■ job0 7.0 GiB"), "{top:?}");
        assert!(top[1].ends_with("▼2"), "{top:?}");
        let (middle, _) = shown(1);
        assert!(middle[1].ends_with("▲1 ▼1"), "{middle:?}");
        // An offset past the end shows the last lines.
        let (bottom, _) = shown(40);
        assert!(bottom[1].ends_with("▲2"), "{bottom:?}");
        assert!(bottom[1].contains("job7"), "{bottom:?}");
        // Everything fits: nothing scrolls.
        let (all, scrolls) = {
            let legend = gpu_legend(&view, 52, (10, 0), &context);
            (legend.lines.len(), legend.scrolls)
        };
        assert_eq!((all, scrolls), (3, false));
    }
}
