//! Memory module in the reference layout (D-38), one subject a row, each
//! row led by its label in a column of its own and in a color of its own
//! (D-143). `RAM`: the RAM in use and, on unified memory, what the system
//! itself holds of it (D-36, D-69). `GPU`: what each tracked GPU process
//! holds (D-64). `used`: the bar, which draws each process's part in its
//! color and the other GPU processes in gray, with the used percentage and
//! the used chart under it, stacked the same way (D-43), a column lower
//! than a row drawn a row high so both colors show (D-134). `avail`: what
//! is left to use and its share of the RAM, in the level color of how
//! little that is, then cache and buffers. `swap`: the swap in use, its
//! share, and what moves in and out of it (D-135). Last, only while they
//! have something to say: the memory's stall pressure and the OOM kills.

use dgxtop_core::view::{MemoryView, MetricCell, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::ModuleBuildError;
use super::common::{
    BarRow, ManifestSpec, Output, RowCursor, after_label, build_manifest, cell_label, draw_line,
    flow_row, fraction_ratio, has_value, labeled_cell, measuring_context, message, no_data, panel,
    panel_with_keys, pressure_labels, pressure_line, render_bar_row, render_chart_with_parts,
    reported, separator, short_pressure_line, telling_pressure,
};
use super::process_memory::{
    SWATCH, bytes_f64, legend_lines, other_points, share_segments, trend_parts,
};
use crate::format::{CellDetail, clean, format_bytes, format_percent, value_text};
use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, OverviewCell, RenderOutput, Slot,
    controls,
};
use crate::theme::MemoryRow;
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
/// The label column of the panel's rows (D-143): its widest label, `avail`,
/// and a space. Every row's figures, the bar and its chart start after it.
const LABEL_WIDTH: u16 = 6;
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
        let footer_rows = footer_lines(memory, inner, &context).len();
        let gpu_rows = gpu_legend_rows(memory, inner, MAX_GPU_ROWS, &context);
        Some(2 + RAM_ROWS + saturate_u16(gpu_rows + footer_rows))
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
        // The bar and its chart keep their rows and the GPU legend one;
        // the footer wraps its rows where that leaves it the room.
        let legend_row = usize::from(memory.gpu_resident.is_some());
        let footer_room =
            usize::from(rows.remaining()).saturating_sub(usize::from(1 + CHART_ROWS) + legend_row);
        let footer = footer_within(memory, (inner.width, footer_room), context);
        // The bar, its chart and the footer rows keep their rows; the GPU
        // legend takes what is left, up to its most, and scrolls (D-70).
        let room = usize::from(rows.remaining())
            .saturating_sub(usize::from(1 + CHART_ROWS) + footer.len())
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
            .saturating_sub(saturate_u16(footer.len()))
            .max(1 + CHART_ROWS);
        if let Some(area) = rows.take(chart_rows) {
            render_used(frame, area, memory, context);
        }
        for line in footer {
            let Some(row) = rows.next_row() else { break };
            draw_line(frame.buffer_mut(), row, &line);
        }
        output.label(cell_label("memory used", &memory.used, context));
        output.label(cell_label("memory total", &memory.total, context));
        output.label(cell_label("memory available", &memory.available, context));
        if let Some(pressure) = &memory.pressure {
            pressure_labels(&mut output, "memory", pressure, context);
        }
        if oom_line(memory, OomKills::SinceBoot, context).is_some() {
            output.label(cell_label(
                "processes killed by the OOM killer since boot",
                &memory.oom_kills,
                context,
            ));
        }
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
        for (label, cell) in swap_rates(memory) {
            output.label(cell_label(
                &format!("swapped {label} a second"),
                cell,
                context,
            ));
        }
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

/// The RAM in use and, of it, what GPU processes hold, in bytes (display
/// arithmetic only; `None` until total and used are known).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Shares {
    /// The RAM total, the whole of the bar.
    total: f64,
    /// The RAM in use, at most the total.
    used: f64,
    /// What GPU processes hold, at most the RAM in use.
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
            .map(|gpu| gpu.clamp(0.0, used));
        Some(Self { total, used, gpu })
    }

    /// The used share of the RAM.
    fn used_fraction(&self) -> f64 {
        self.used / self.total
    }

    /// The bar's segments: each tracked GPU process in its color, the other
    /// GPU processes in gray, then the rest of the used RAM in the system's
    /// color, or the used share's level color where no GPU shares the RAM.
    /// Without a GPU figure the processes' parts stay within the used RAM.
    /// Counted in bytes, so a part the legend leaves out is exactly none
    /// and takes no cell (D-133).
    fn segments(&self, memory: &MemoryView, context: &DisplayContext) -> Vec<Segment> {
        let theme = &context.theme;
        let room = self.gpu.unwrap_or(self.used);
        let (mut segments, tracked) =
            share_segments(&memory.gpu_processes, self.total, room, context);
        // In whole bytes, as the legend's `other` is, and within what the
        // bar shows of GPU memory.
        let other = other_bytes(memory)
            .map_or(0.0, |other| bytes_f64(other).min((room - tracked).max(0.0)));
        segments.push(Segment {
            fraction: other / self.total,
            color: theme.other_processes_color(),
        });
        let rest = if self.gpu.is_some() {
            theme.os_memory_color()
        } else {
            theme.level_color(self.used_fraction())
        };
        segments.push(Segment {
            fraction: (self.used - tracked - other).max(0.0) / self.total,
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
    let mut spans = vec![row_label("RAM", MemoryRow::Ram, context)];
    spans.extend(of_whole(&memory.used, &memory.total, context));
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

/// A row's label in the panel's label column, in the row's color (D-143).
fn row_label(label: &str, row: MemoryRow, context: &DisplayContext) -> Span<'static> {
    Span::styled(
        format!("{label:<width$}", width = usize::from(LABEL_WIDTH)),
        context.theme.memory_row_style(row),
    )
}

/// `85.1 GiB / 122 GiB`: a part, then the whole it is of, set back.
fn of_whole(part: &MetricCell, whole: &MetricCell, context: &DisplayContext) -> Vec<Span<'static>> {
    let theme = &context.theme;
    vec![
        Span::styled(value_text(part), theme.text_style()),
        Span::styled(format!(" / {}", value_text(whole)), theme.label_style()),
    ]
}

/// ` 30.0%`: `part`'s share of `whole` after the figure it belongs to, in
/// the level color of `level` (0 nothing to note, 1 the worst); `None`
/// until both are read and the whole is more than nothing.
fn share_span(
    part: &MetricCell,
    whole: &MetricCell,
    level: impl Fn(f64) -> f64,
    context: &DisplayContext,
) -> Option<Span<'static>> {
    if !has_value(part) || !has_value(whole) {
        return None;
    }
    let whole = whole.value_f64().filter(|whole| *whole > 0.0)?;
    let share = (part.value_f64()? / whole).clamp(0.0, 1.0);
    Some(Span::styled(
        format!(" {}", format_percent(share * 100.0)),
        Style::new().fg(context.theme.level_color(level(share))),
    ))
}

/// The lead of the GPU legend: the `GPU` row's label.
fn gpu_label(context: &DisplayContext) -> Span<'static> {
    row_label("GPU", MemoryRow::Gpu, context)
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
    let lines_at = |width: usize| {
        legend_lines(
            (gpu_label(context), &memory.gpu_processes),
            other_bytes(memory),
            width,
            usize::MAX,
            context,
        )
    };
    let mut all = lines_at(usize::from(width));
    if all.len() > shown {
        // Leave room for the hint at the end of each line.
        all = lines_at(usize::from(width).saturating_sub(hint_width));
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
    legend_lines(
        (gpu_label(context), &memory.gpu_processes),
        other_bytes(memory),
        usize::from(width),
        max_lines,
        context,
    )
    .len()
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

/// What the GPU processes without a color hold: as their process lists
/// count it (D-142), which does not move while a tracked process's memory
/// does; until those lists are known, the GPU share less the tracked
/// processes (display arithmetic), read at another moment than they.
fn other_bytes(memory: &MemoryView) -> Option<u64> {
    memory.gpu_resident.as_ref()?;
    memory
        .gpu_other_bytes
        .or_else(|| share_beyond_tracked(memory))
}

/// The GPU share less the tracked processes (display arithmetic).
fn share_beyond_tracked(memory: &MemoryView) -> Option<u64> {
    let gpu = memory.gpu_resident.as_ref()?.value_f64()?;
    let tracked: f64 = memory
        .gpu_processes
        .iter()
        .map(|share| bytes_f64(share.bytes))
        .sum();
    let other = (gpu - tracked).max(0.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bytes = other.round() as u64;
    Some(bytes)
}

/// `used ▆▆▆▆▆▆▆▆▆ 19.2%` (D-43): the used RAM as a bar whose GPU part shows
/// each tracked process in its color ([`Shares::segments`]), the used
/// percentage, and the used chart under it against the total, stacked the
/// same way, a column of GPU processes lower than a row drawn a row high,
/// split between them and the system (D-134); a plain `n/a` gauge until
/// total and used are known.
fn render_used(frame: &mut Frame<'_>, area: Rect, memory: &MemoryView, context: &DisplayContext) {
    let theme = &context.theme;
    let mut rows = RowCursor::new(area);
    let Some(row) = rows.next_row() else { return };
    if let Some(chart) = rows.rest() {
        let shared = memory.gpu_resident.is_some();
        let other = other_points(
            &memory.gpu_resident_trend.points,
            &memory.gpu_process_trends,
        );
        let mut parts = trend_parts(&memory.gpu_process_trends, context);
        if shared {
            parts.push((
                other.as_slice(),
                Style::new().fg(theme.other_processes_color()),
            ));
        }
        render_chart_with_parts(
            frame,
            after_label(chart, LABEL_WIDTH),
            &memory.used_trend.points,
            (parts, shared.then(|| theme.os_memory_color())),
            memory.total.value_f64(),
            context,
        );
    }
    let shares = Shares::of(memory);
    let value = shares.map_or_else(
        || value_text(&memory.used),
        |shares| format_percent(shares.used_fraction() * 100.0),
    );
    let gauge = render_bar_row(
        frame,
        row,
        &BarRow {
            label: "used",
            label_width: LABEL_WIDTH,
            label_style: theme.memory_row_style(MemoryRow::Ram),
            ratio: shares.map(|shares| shares.used_fraction()),
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

/// The lines under the used chart (D-143): what is left to use, then
/// swap, each a row of its own whose items wrap under its first while the
/// panel is narrow; last, while they have something to say, the memory's
/// stall pressure (D-135) and the OOM kills, on one line when both fit
/// `width`.
fn footer_lines(memory: &MemoryView, width: u16, context: &DisplayContext) -> Vec<Line<'static>> {
    let mut lines = figure_rows(memory, width, context);
    lines.extend(trouble_lines(memory, width, context));
    lines
}

/// [`footer_lines`] in at most `rows` lines where the panel has them; a
/// panel too short for the wrapped rows keeps each row on one line, cut
/// at its edge, so the lines after them still show.
fn footer_within(
    memory: &MemoryView,
    (width, rows): (u16, usize),
    context: &DisplayContext,
) -> Vec<Line<'static>> {
    let wrapped = footer_lines(memory, width, context);
    if wrapped.len() <= rows {
        return wrapped;
    }
    let mut lines = figure_rows(memory, u16::MAX, context);
    lines.extend(trouble_lines(memory, width, context));
    lines
}

/// The `avail` and `swap` rows, their items wrapped to `width`.
fn figure_rows(memory: &MemoryView, width: u16, context: &DisplayContext) -> Vec<Line<'static>> {
    let mut lines = flow_row(
        row_label("avail", MemoryRow::Available, context),
        available_items(memory, context),
        width,
        context,
    );
    lines.extend(flow_row(
        row_label("swap", MemoryRow::Swap, context),
        swap_items(memory, context),
        width,
        context,
    ));
    lines
}

/// The memory's stall pressure, while it has something to say (D-143),
/// and the OOM kills, once there are any: on one line when both fit
/// `width`, the kills without `since boot` and then the pressure without
/// what its averages span when only that fits; else a line each. A
/// pressure wider than `width` leaves out what its averages span instead
/// of losing one of them.
fn trouble_lines(memory: &MemoryView, width: u16, context: &DisplayContext) -> Vec<Line<'static>> {
    let fits = |line: &Line<'_>| line.width() <= usize::from(width);
    let pressures: Vec<Line<'static>> = telling_pressure(memory.pressure.as_ref())
        .map(|pressure| {
            vec![
                pressure_line(pressure, context),
                short_pressure_line(pressure, context),
            ]
        })
        .unwrap_or_default();
    let kills: Vec<Line<'static>> = [OomKills::SinceBoot, OomKills::Count]
        .into_iter()
        .filter_map(|form| oom_line(memory, form, context))
        .collect();
    let on_one_line = pressures.iter().find_map(|pressure| {
        kills.iter().find_map(|kills| {
            let mut spans = pressure.spans.clone();
            spans.push(separator(context));
            spans.extend(kills.spans.iter().cloned());
            Some(Line::from(spans)).filter(fits)
        })
    });
    if let Some(line) = on_one_line {
        return vec![line];
    }
    let pressure = pressures
        .iter()
        .find(|line| fits(line))
        .or(pressures.last())
        .cloned();
    pressure
        .into_iter()
        .chain(kills.into_iter().next())
        .collect()
}

/// `36.6 GiB 30.0%`, `cache 35.3 GiB`, `buf 115 MiB`: the RAM a process
/// can still take, its share of the RAM in the level color of how little
/// is left (the color the used bar has at that moment), then what the
/// kernel keeps of files and gives back when asked.
fn available_items(memory: &MemoryView, context: &DisplayContext) -> Vec<Vec<Span<'static>>> {
    let theme = &context.theme;
    let mut available = vec![Span::styled(
        value_text(&memory.available),
        theme.text_style(),
    )];
    available.extend(share_span(
        &memory.available,
        &memory.total,
        |left| 1.0 - left,
        context,
    ));
    let plain = |label: &str, cell: &MetricCell| {
        vec![
            Span::styled(format!("{label} "), theme.label_style()),
            Span::styled(value_text(cell), theme.text_style()),
        ]
    };
    vec![
        available,
        plain("cache", &memory.cached),
        plain("buf", &memory.buffers),
    ]
}

/// `2.0 GiB / 100.0 GiB 2.0%`, `in 0 B/s`, `out 1.2 MiB/s`: the swap in
/// use says what was moved out once, with its share of the swap in the
/// level color of how full it is; the rates say whether pages move now.
fn swap_items(memory: &MemoryView, context: &DisplayContext) -> Vec<Vec<Span<'static>>> {
    let mut used = of_whole(&memory.swap_used, &memory.swap_total, context);
    used.extend(share_span(
        &memory.swap_used,
        &memory.swap_total,
        |full| full,
        context,
    ));
    let mut items = vec![used];
    for (label, cell) in swap_rates(memory) {
        items.push(labeled_cell(label, cell, CellDetail::GLANCE, context));
    }
    items
}

/// How the OOM kills are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OomKills {
    /// `OOM kills 4 since boot`.
    SinceBoot,
    /// `OOM kills 4`, where the line has no room for more.
    Count,
}

/// `OOM kills 4 since boot`, the count in the warning color: the
/// processes the kernel's OOM killer killed (D-135). Left out while it
/// killed none, or where the kernel does not count them.
fn oom_line(
    memory: &MemoryView,
    form: OomKills,
    context: &DisplayContext,
) -> Option<Line<'static>> {
    let theme = &context.theme;
    memory.oom_kills.value_f64().filter(|kills| *kills >= 1.0)?;
    let mut spans = vec![
        Span::styled("OOM kills ", theme.label_style()),
        Span::styled(
            value_text(&memory.oom_kills),
            Style::new().fg(theme.warning),
        ),
    ];
    if form == OomKills::SinceBoot {
        spans.push(Span::styled(" since boot", theme.label_style()));
    }
    Some(Line::from(spans))
}

/// What is read back from swap (`in`) and written out to it (`out`) each
/// second, where the kernel counts it (D-135).
fn swap_rates(memory: &MemoryView) -> impl Iterator<Item = (&'static str, &MetricCell)> {
    [("in", &memory.swap_in), ("out", &memory.swap_out)]
        .into_iter()
        .filter(|(_, cell)| reported(cell))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::metric::CanonicalUnit;
    use dgxtop_core::view::{CellState, ProcessShare, ProcessTrend, Trend};

    /// Label column of the panel's rows, before the gauge of `used`.
    const BAR_LABEL: u16 = LABEL_WIDTH;

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
        assert_eq!(shares.used_fraction(), 0.25);
        assert_eq!(shares.gpu, Some(8.0 * GIB));
        // A GPU figure above `used` (counted at another instant) is capped.
        let capped = Shares::of(&memory(bytes(16.0 * GIB), Some(bytes(40.0 * GIB)))).unwrap();
        assert_eq!(capped.gpu, Some(16.0 * GIB));
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
        assert_eq!((shares.used_fraction(), shares.gpu), (0.5, None));
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
                },
            },
            ProcessTrend {
                slot: 1,
                trend: Trend {
                    points: vec![Some(8.0 * GIB)],
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

    /// A job holding 12.2 GiB of the 19.5 GiB in use of 122 GiB, beside one
    /// far under a cell: the bar gives the job its share and the small one a
    /// cell of its own, and no gray shows while the two hold all the GPU's
    /// memory; the chart under it, whose column is lower than a row, draws
    /// that row split between the jobs, in the larger one's color, and the
    /// system (D-134).
    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn a_small_job_keeps_its_color_on_the_bar_and_the_chart() {
        let theme = crate::test_fixtures::display_context().theme;
        let (job, small) = (share(3, 12.2), share(5, 0.1));
        let held = (job.bytes + small.bytes) as f64;
        let mut view = memory(bytes(19.5 * GIB), Some(bytes(held)));
        view.total = bytes(122.0 * GIB);
        view.used_trend.points = vec![Some(19.5 * GIB); 2];
        view.gpu_resident_trend.points = vec![Some(held); 2];
        view.gpu_process_trends = [&job, &small]
            .iter()
            .map(|process| ProcessTrend {
                slot: process.slot,
                trend: Trend {
                    points: vec![Some(process.bytes as f64); 2],
                },
            })
            .collect();
        view.gpu_processes = vec![job, small];
        // 64 columns of bar: the job to 6.4 → 6, less the cell the small
        // job keeps, the system to 10.2 → 10, then the track.
        let buffer = draw_used(&view, (BAR_LABEL + 64 + 1 + 6, 4));
        let bar: Vec<_> = (BAR_LABEL..BAR_LABEL + 64)
            .map(|x| buffer[(x, 0)].fg)
            .collect();
        let mut expected = vec![theme.process_color(3); 5];
        expected.push(theme.process_color(5));
        expected.extend([theme.os_memory_color(); 4]);
        expected.extend([theme.gauge_track; 54]);
        assert_eq!(bar, expected);
        // Three chart rows (22 eighths) against the total: the column to
        // 3.5 → 4, so one row high; the jobs hold 12.3 of the 19.5 GiB in
        // use → 5 of its 8 eighths.
        let x = buffer.area.right() - 1;
        let cell = |y: u16| {
            let cell = &buffer[(x, y)];
            (cell.symbol().to_owned(), cell.fg, cell.bg)
        };
        assert_eq!(
            cell(3),
            (
                "▅".to_owned(),
                theme.process_color(3),
                theme.os_memory_color()
            )
        );
        assert_eq!(cell(2).0, " ");
    }

    /// RAM no GPU shares keeps the total as the chart's scale: its columns
    /// take the level colors of their share of it.
    #[test]
    fn a_chart_of_ram_no_gpu_shares_is_scaled_to_the_total() {
        let theme = crate::test_fixtures::display_context().theme;
        let mut view = memory(bytes(16.0 * GIB), None);
        view.used_trend.points = vec![Some(16.0 * GIB); 2];
        // A quarter of 22 eighths: 5.5 → 6, in the bottom row only.
        let buffer = draw_used(&view, (30, 4));
        let x = buffer.area.right() - 1;
        assert_eq!(buffer[(x, 3)].symbol(), "▆");
        assert_eq!(buffer[(x, 3)].fg, theme.level_color(0.25));
        assert_eq!(buffer[(x, 2)].symbol(), " ");
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
        assert_eq!(text(&ram), "RAM   48.0 GiB / 64.0 GiB │ ■ OS 16.0 GiB");
        let swatch = ram
            .spans
            .iter()
            .find(|span| span.content == SWATCH)
            .unwrap();
        assert_eq!(swatch.style.fg, Some(context.theme.os_memory_color()));
        assert_eq!(legend(&view, 120), vec!["GPU   ■ other 32.0 GiB"]);
        view.gpu_processes = vec![share(0, 16.0), share(1, 8.0)];
        assert_eq!(
            legend(&view, 120),
            vec!["GPU   ■ job0 16.0 GiB  ■ job1 8.0 GiB  ■ other 8.0 GiB"]
        );
        assert_eq!(
            legend(&view, 40),
            vec![
                "GPU   ■ job0 16.0 GiB  ■ job1 8.0 GiB",
                "      ■ other 8.0 GiB"
            ]
        );
        assert_eq!(gpu_legend_rows(&view, 40, MAX_GPU_ROWS, &context), 2);
        // A discrete GPU's memory is not in the RAM: no OS part, no legend.
        view.gpu_resident = None;
        assert_eq!(
            text(&ram_line(&view, &context)),
            "RAM   48.0 GiB / 64.0 GiB"
        );
        assert!(legend(&view, 120).is_empty());
    }

    /// The GPU share is read at another moment than its processes: while a
    /// job's memory grows, the share is ahead of what the legend lists for
    /// the jobs (D-142). The legend and the bar take the other processes
    /// from the processes' own lists, so no `other` comes and goes with
    /// that difference and the legend keeps its lines; what the share is
    /// ahead by is drawn with the system until the next list.
    #[test]
    fn a_share_read_ahead_of_its_processes_is_no_other_process() {
        let context = crate::test_fixtures::display_context();
        let theme = &context.theme;
        let legend = |view: &MemoryView| -> Vec<String> {
            gpu_legend(view, 40, (MAX_GPU_ROWS, 0), &context)
                .lines
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect()
                })
                .collect()
        };
        let bar = |view: &MemoryView| -> Vec<ratatui::style::Color> {
            let buffer = draw_used(view, (BAR_LABEL + 64 + 1 + 6, 5));
            (BAR_LABEL..BAR_LABEL + 64)
                .map(|x| buffer[(x, 0)].fg)
                .collect()
        };
        let colors = |parts: &[(ratatui::style::Color, usize)]| -> Vec<ratatui::style::Color> {
            parts
                .iter()
                .flat_map(|(color, columns)| std::iter::repeat_n(*color, *columns))
                .collect()
        };
        // The jobs hold 24 GiB by their lists; the share read 32 GiB.
        let mut view = memory(bytes(48.0 * GIB), Some(bytes(32.0 * GIB)));
        view.gpu_processes = vec![share(0, 16.0), share(1, 8.0)];
        view.gpu_other_bytes = Some(0);
        assert_eq!(legend(&view), vec!["GPU   ■ job0 16.0 GiB  ■ job1 8.0 GiB"]);
        assert_eq!(gpu_legend_rows(&view, 40, MAX_GPU_ROWS, &context), 1);
        assert_eq!(
            bar(&view),
            colors(&[
                (theme.process_color(0), 16),
                (theme.process_color(1), 8),
                (theme.os_memory_color(), 24),
                (theme.gauge_track, 16),
            ])
        );
        // What the lists count for processes without a color is shown.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let four_gib = (4.0 * GIB) as u64;
        view.gpu_other_bytes = Some(four_gib);
        assert_eq!(
            legend(&view),
            vec![
                "GPU   ■ job0 16.0 GiB  ■ job1 8.0 GiB",
                "      ■ other 4.0 GiB"
            ]
        );
        assert_eq!(
            bar(&view),
            colors(&[
                (theme.process_color(0), 16),
                (theme.process_color(1), 8),
                (theme.other_processes_color(), 4),
                (theme.os_memory_color(), 20),
                (theme.gauge_track, 16),
            ])
        );
        // A share behind its processes leaves the others no room.
        view.gpu_resident = Some(bytes(24.0 * GIB));
        assert_eq!(
            bar(&view),
            colors(&[
                (theme.process_color(0), 16),
                (theme.process_color(1), 8),
                (theme.os_memory_color(), 24),
                (theme.gauge_track, 16),
            ])
        );
        // Until the lists are known the rest of the share stands for them.
        view.gpu_resident = Some(bytes(32.0 * GIB));
        view.gpu_other_bytes = None;
        assert_eq!(legend(&view)[1], "      ■ other 8.0 GiB");
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
        assert!(top[0].starts_with("GPU   ■ job0 7.0 GiB"), "{top:?}");
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

    fn footer(view: &MemoryView, width: u16) -> Vec<String> {
        let context = crate::test_fixtures::display_context();
        footer_lines(view, width, &context)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    /// Under the chart (D-143): what is left to use with its share of the
    /// RAM, cache and buffers; then swap with its share and what is read
    /// back from it and written out to it each second; last the memory's
    /// stall pressure, each kind's 10 s, 60 s and 300 s averages, which
    /// never stands for what is available.
    #[test]
    fn the_footer_lists_what_is_left_then_swap_then_the_pressure() {
        let context = crate::test_fixtures::display_context();
        let theme = &context.theme;
        let rate = |value: f64| crate::test_fixtures::ok(value, CanonicalUnit::BytesPerSecond);
        let mut view = memory(bytes(48.0 * GIB), None);
        view.pressure = Some(crate::test_fixtures::pressure_view(
            [2.4, 0.9, 0.2],
            Some([0.8, 0.3, 0.1]),
        ));
        view.swap_in = rate(0.0);
        view.swap_out = rate(1.2 * 1024.0 * 1024.0);
        assert_eq!(
            footer(&view, 120),
            vec![
                "avail 51.7 GiB 80.8% │ cache 20.1 GiB │ buf 1.2 GiB",
                "swap  0 B / 8.0 GiB 0.0% │ in 0 B/s │ out 1.2 MiB/s",
                "pressure some 2.4% 0.9% 0.2% │ full 0.8% 0.3% 0.1%  (10s 1m 5m)",
            ]
        );
        // A narrow panel wraps a row's items under its first; none is cut.
        assert_eq!(
            footer(&view, 40),
            vec![
                "avail 51.7 GiB 80.8% │ cache 20.1 GiB",
                "      buf 1.2 GiB",
                "swap  0 B / 8.0 GiB 0.0% │ in 0 B/s",
                "      out 1.2 MiB/s",
                "pressure some 2.4% 0.9% 0.2% │ full 0.8% 0.3% 0.1%",
            ]
        );
        // Each row's label has its color; a share takes its level color:
        // what is left by how little it is, the swap by how full it is.
        let lines = footer_lines(&view, 120, &context);
        let styled = |line: usize, text: &str| {
            lines[line]
                .spans
                .iter()
                .find(|span| span.content == text)
                .map(|span| span.style)
                .unwrap_or_else(|| panic!("{text} missing"))
        };
        assert_eq!(
            styled(0, "avail "),
            theme.memory_row_style(MemoryRow::Available)
        );
        assert_eq!(styled(1, "swap  "), theme.memory_row_style(MemoryRow::Swap));
        assert_ne!(
            theme.memory_row_style(MemoryRow::Available),
            theme.memory_row_style(MemoryRow::Swap)
        );
        let left = 51.7 / 64.0;
        assert_eq!(styled(0, " 80.8%").fg, Some(theme.level_color(1.0 - left)));
        assert_eq!(styled(1, " 0.0%").fg, Some(theme.gauge_low));
        view.available = bytes(1.6 * GIB);
        let tight = footer_lines(&view, 120, &context);
        assert_eq!(tight[0].spans[2].content, " 2.5%");
        assert_eq!(
            tight[0].spans[2].style.fg,
            Some(theme.level_color(0.975)),
            "little left is red"
        );
        // Each average takes its level color: green up to 1 %.
        let line = pressure_line(view.pressure.as_ref().unwrap(), &context);
        let color = |text: &str| {
            line.spans
                .iter()
                .find(|span| span.content == text)
                .and_then(|span| span.style.fg)
        };
        assert_eq!(color("0.9%"), Some(theme.gauge_low));
        assert_ne!(color("2.4%"), Some(theme.gauge_low));
        // The module asks for a row more than without the pressure.
        let module = MemoryModule::new().unwrap();
        let mut model =
            crate::test_fixtures::view_model(dgxtop_core::commands::ViewKind::Overview, 1);
        let rows = |view: &MemoryView, model: &mut ViewModel| {
            model.modules.memory = Some(std::sync::Arc::new(view.clone()));
            module.preferred_rows(model, 122, Slot::Overview).unwrap()
        };
        let with_pressure = rows(&view, &mut model);
        view.pressure = None;
        assert_eq!(rows(&view, &mut model) + 1, with_pressure);
    }

    /// A panel with the rows for them wraps its rows' items; one too short
    /// keeps each row on one line, cut at its edge, so the pressure and the
    /// OOM kills under them still show. A pressure wider than the panel
    /// leaves out what its averages span, never an average.
    #[test]
    fn a_short_panel_keeps_a_line_for_each_row() {
        let context = crate::test_fixtures::display_context();
        let rate = |value: f64| crate::test_fixtures::ok(value, CanonicalUnit::BytesPerSecond);
        let mut view = memory(bytes(48.0 * GIB), None);
        view.swap_in = rate(0.0);
        view.swap_out = rate(0.0);
        view.oom_kills = crate::test_fixtures::ok(4.0, CanonicalUnit::Count);
        view.pressure = Some(crate::test_fixtures::pressure_view(
            [2.4, 0.9, 0.2],
            Some([0.8, 0.3, 0.1]),
        ));
        let within = |rows: usize| -> Vec<String> {
            footer_within(&view, (56, rows), &context)
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect()
                })
                .collect()
        };
        let trouble = [
            "pressure some 2.4% 0.9% 0.2% │ full 0.8% 0.3% 0.1%",
            "OOM kills 4 since boot",
        ];
        // 40 columns: both rows wrap, six lines in all.
        assert_eq!(footer(&view, 40).len(), 6);
        assert_eq!(footer(&view, 56).len(), 4);
        assert_eq!(within(4)[2..], trouble);
        let narrow = |rows: usize| -> Vec<String> {
            footer_within(&view, (40, rows), &context)
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect()
                })
                .collect()
        };
        assert_eq!(narrow(6), footer(&view, 40));
        assert_eq!(
            narrow(5),
            vec![
                "avail 51.7 GiB 80.8% │ cache 20.1 GiB │ buf 1.2 GiB",
                "swap  0 B / 8.0 GiB 0.0% │ in 0 B/s │ out 0 B/s",
                trouble[0],
                trouble[1],
            ]
        );
    }

    /// While no task lost time waiting for memory, every average reads
    /// `0.0%` and the pressure has no line (D-143); the first average above
    /// that brings the line back, and the panel asks for its row.
    #[test]
    fn a_pressure_of_nothing_has_no_line() {
        let mut view = memory(bytes(48.0 * GIB), None);
        let without = footer(&view, 120);
        view.pressure = Some(crate::test_fixtures::pressure_view(
            [0.0, 0.0, 0.04],
            Some([0.0, 0.0, 0.0]),
        ));
        assert_eq!(footer(&view, 120), without);
        view.pressure = Some(crate::test_fixtures::pressure_view(
            [0.0, 0.0, 0.06],
            Some([0.0, 0.0, 0.0]),
        ));
        assert_eq!(
            footer(&view, 120).last().unwrap(),
            "pressure some 0.0% 0.0% 0.1% │ full 0.0% 0.0% 0.0%  (10s 1m 5m)"
        );
    }

    /// A kernel without PSI and the swap counters shows the swap in use
    /// alone; a counter still warming up says so instead of a rate, and a
    /// host without swap has no share of it.
    #[test]
    fn a_kernel_without_pressure_or_swap_counters_keeps_the_swap_row() {
        let mut view = memory(bytes(48.0 * GIB), None);
        assert_eq!(
            footer(&view, 120),
            vec![
                "avail 51.7 GiB 80.8% │ cache 20.1 GiB │ buf 1.2 GiB",
                "swap  0 B / 8.0 GiB 0.0%",
            ]
        );
        view.swap_in = MetricCell::missing(CanonicalUnit::BytesPerSecond, CellState::Warmup);
        assert_eq!(
            footer(&view, 120)[1],
            "swap  0 B / 8.0 GiB 0.0% │ in warming up"
        );
        view.swap_total = bytes(0.0);
        assert_eq!(footer(&view, 120)[1], "swap  0 B / 0 B │ in warming up");
    }

    /// Once the OOM killer killed a process, the count since boot ends the
    /// panel in the warning color (D-135), after the pressure when both fit
    /// a line, without `since boot` when only that fits; none killed, or a
    /// kernel that does not count them, adds nothing.
    #[test]
    fn oom_kills_end_the_panel_once_there_are_any() {
        let context = crate::test_fixtures::display_context();
        let count = |kills: f64| crate::test_fixtures::ok(kills, CanonicalUnit::Count);
        let mut view = memory(bytes(48.0 * GIB), None);
        view.oom_kills = count(0.0);
        assert_eq!(footer(&view, 120).len(), 2);
        assert!(oom_line(&view, OomKills::SinceBoot, &context).is_none());
        view.oom_kills = count(4.0);
        assert_eq!(footer(&view, 120)[2], "OOM kills 4 since boot");
        let kills = oom_line(&view, OomKills::SinceBoot, &context).unwrap();
        assert_eq!(kills.spans[1].style.fg, Some(context.theme.warning));
        view.pressure = Some(crate::test_fixtures::pressure_view(
            [2.4, 0.9, 0.2],
            Some([0.8, 0.3, 0.1]),
        ));
        assert_eq!(
            footer(&view, 120)[2],
            "pressure some 2.4% 0.9% 0.2% │ full 0.8% 0.3% 0.1%  (10s 1m 5m) \
             │ OOM kills 4 since boot"
        );
        // Room for the count only.
        assert_eq!(
            footer(&view, 77)[2],
            "pressure some 2.4% 0.9% 0.2% │ full 0.8% 0.3% 0.1%  (10s 1m 5m) │ OOM kills 4"
        );
        // Then the pressure gives up what its averages span.
        assert_eq!(
            footer(&view, 70)[2],
            "pressure some 2.4% 0.9% 0.2% │ full 0.8% 0.3% 0.1% │ OOM kills 4"
        );
        // Room for neither on one line: a line each, the kills in full.
        assert_eq!(
            footer(&view, 63)[2..],
            [
                "pressure some 2.4% 0.9% 0.2% │ full 0.8% 0.3% 0.1%  (10s 1m 5m)",
                "OOM kills 4 since boot",
            ]
        );
    }
}
