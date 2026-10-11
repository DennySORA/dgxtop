//! The Process History tab (D-76, D-77): every GPU process run kept (the
//! latest 2000), the latest start first (`tab` 1), and one run's whole record
//! (`tab` 2): what it ran and where, its parent, program and control group,
//! when it started and ended and why, and its charts laid out like the
//! Overview History's: panels of strips with their scale and legend, each
//! column a bar of the size `w` steps through, on the local clock's axis.
//! The context's scroll is the cursor; its `expanded` shows the whole
//! command line.

use dgxtop_core::metric::CanonicalUnit;
use dgxtop_core::process_history::{RunInfo, RunMetric};
use dgxtop_core::tiers::BarSize;
use dgxtop_core::view::{ProcessHistoryView, RunChartView, RunDetailView, RunRowView};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::axis::clock_ticks;
use super::layout::plan;
use super::panels::{Paint, Panel, Series, SeriesState, Strip, single};
use super::strip::Window;
use super::{Hidden, render_axes, render_bands};
use crate::format::{
    NOT_AVAILABLE, clean, display_width, format_local_datetime, format_local_short,
    format_span_seconds, format_value, truncate_to_width,
};
use crate::modules::common::{
    ColumnSpec, Output, RowCursor, TableCell, draw_caption, draw_line, draw_table_header,
    draw_table_row, fit_columns, gap, message, separator,
};
use crate::num::saturate_u16;
use crate::registry::DisplayContext;

const SWATCH: &str = "■";
const RUNNING: &str = "running";
/// Command lines an expanded command takes at most.
const MAX_COMMAND_LINES: usize = 8;

/// A list column: title, right-aligned, priority (lower gives way first).
struct Column {
    title: &'static str,
    right: bool,
    priority: u8,
}

const COLUMNS: [Column; 12] = [
    Column {
        title: "",
        right: false,
        priority: 255,
    },
    Column {
        title: "STARTED",
        right: false,
        priority: 250,
    },
    Column {
        title: "PID",
        right: true,
        priority: 245,
    },
    Column {
        title: "NAME",
        right: false,
        priority: 240,
    },
    Column {
        title: "GPU",
        right: false,
        priority: 120,
    },
    Column {
        title: "RUNTIME",
        right: true,
        priority: 200,
    },
    Column {
        title: "PEAK GPU MEM",
        right: true,
        priority: 190,
    },
    Column {
        title: "PEAK RAM",
        right: true,
        priority: 150,
    },
    Column {
        title: "GPU%",
        right: true,
        priority: 110,
    },
    Column {
        title: "CPU%",
        right: true,
        priority: 100,
    },
    Column {
        title: "CHILD",
        right: true,
        priority: 90,
    },
    Column {
        title: "STATUS",
        right: false,
        priority: 230,
    },
];

/// The run under the cursor: the session's selected run where the list
/// holds it (a run that starts later moves it down the list), else the
/// `scroll`th, clamped to the list.
fn cursor_run(view: &ProcessHistoryView, scroll: u16) -> Option<&RunRowView> {
    view.runs.get(cursor_index(view, scroll)?)
}

/// The position of [`cursor_run`].
fn cursor_index(view: &ProcessHistoryView, scroll: u16) -> Option<usize> {
    let last = view.runs.len().checked_sub(1)?;
    let selected = view
        .opened
        .as_ref()
        .and_then(|opened| view.runs.iter().position(|run| run.key == opened.info.key));
    Some(selected.unwrap_or_else(|| usize::from(scroll).min(last)))
}

/// The list or the opened run, under the tab's title row the caller drew.
pub(super) fn render_runs(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ProcessHistoryView,
    context: &DisplayContext,
    output: &mut Output,
) {
    let Some(run) = cursor_run(view, context.scroll) else {
        message(
            frame,
            area,
            "no GPU process was seen yet",
            context.theme.muted_style(),
        );
        output.label("process history: no GPU process run");
        return;
    };
    output.label(format!(
        "process history: {} runs{}",
        view.runs.len() + view.more_runs,
        if view.kept_on_disk {
            ", kept on disk"
        } else {
            ", while dgxtop runs"
        }
    ));
    if context.tab <= 1 {
        render_list(frame, area, view, context, output);
        return;
    }
    let detail = view
        .opened
        .as_ref()
        .filter(|detail| detail.info.key == run.key);
    render_detail(frame, area, (run, detail), view, context, output);
}

fn render_list(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ProcessHistoryView,
    context: &DisplayContext,
    output: &mut Output,
) {
    let cursor = cursor_index(view, context.scroll).unwrap_or(0);
    let rows: Vec<Vec<TableCell>> = view
        .runs
        .iter()
        .map(|run| list_cells(run, view, context))
        .collect();
    let specs: Vec<ColumnSpec> = COLUMNS
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let widest = saturate_u16(
                rows.iter()
                    .filter_map(|cells| cells.get(index))
                    .map(|cell| display_width(&cell.text))
                    .max()
                    .unwrap_or(0)
                    .max(display_width(column.title)),
            );
            let spec = if column.right {
                ColumnSpec::right(column.title, widest, column.priority)
            } else {
                ColumnSpec::left(column.title, widest, column.priority)
            };
            spec.prefer(widest.saturating_add(1))
        })
        .collect();
    let fitted = fit_columns(&specs, area.width);
    let mut cursor_rows = RowCursor::new(area);
    if let Some(header) = cursor_rows.next_row() {
        draw_table_header(
            frame.buffer_mut(),
            header,
            &specs,
            &fitted,
            context.theme.header_style(),
        );
    }
    let visible = usize::from(cursor_rows.remaining()).max(1);
    let first = (cursor + 1).saturating_sub(visible);
    for (index, cells) in rows.into_iter().enumerate().skip(first) {
        let Some(row) = cursor_rows.next_row() else {
            break;
        };
        let style = (index == cursor).then(|| context.theme.selected_row_style());
        draw_table_row(frame.buffer_mut(), row, &fitted, &cells, style);
    }
    if view.more_runs > 0
        && let Some(row) = cursor_rows.next_row()
    {
        draw_line(
            frame.buffer_mut(),
            row,
            &Line::styled(
                format!("+{} earlier runs", view.more_runs),
                context.theme.muted_style(),
            ),
        );
    }
    for run in &view.runs {
        output.label(run_label(run, view.utc_offset_s));
    }
}

fn list_cells(
    run: &RunRowView,
    view: &ProcessHistoryView,
    context: &DisplayContext,
) -> Vec<TableCell> {
    let theme = &context.theme;
    let (text, label) = if run.live {
        (theme.text_style(), theme.label_style())
    } else {
        (theme.muted_style(), theme.muted_style())
    };
    // A share of its whole in the gauge colors (D-77): the GPUs it used,
    // the host's CPUs.
    #[allow(clippy::cast_precision_loss)]
    let gpu_scale = 100.0 * run.gpus.len().max(1) as f64;
    let level = |value: Option<f64>, scale: Option<f64>| match (value, scale) {
        (Some(value), Some(scale)) if scale > 0.0 && value.is_finite() => {
            Style::new().fg(theme.level_color(value / scale))
        }
        _ => text,
    };
    let swatch = run.slot.map_or_else(
        || TableCell::new(" ", text),
        |slot| TableCell::new(SWATCH, Style::new().fg(theme.process_color(slot))),
    );
    vec![
        swatch,
        TableCell::new(
            format_local_short(run.started_utc_ns, view.utc_offset_s),
            label,
        ),
        TableCell::new(run.key.pid.to_string(), label),
        TableCell::new(clean(&run.name), text),
        TableCell::new(gpu_text(&run.gpus), label),
        TableCell::new(runtime_text(run.runtime_ns), text),
        TableCell::new(bytes_text(run.peak_gpu_memory), text),
        TableCell::new(bytes_text(run.peak_memory), text),
        TableCell::new(
            percent_text(run.mean_gpu_utilization),
            level(run.mean_gpu_utilization, Some(gpu_scale)),
        ),
        TableCell::new(
            percent_text(run.mean_cpu),
            level(run.mean_cpu, view.cpu_scale),
        ),
        TableCell::new(count_text(run.peak_children), text),
        TableCell::new(status_text(run), status_style(run, context)),
    ]
}

/// `process history run pid 42 acme_llm.train started 10-11 14:03 on GPU 0:
/// ran 1h 06m, SIGKILL by dgxtop`.
fn run_label(run: &RunRowView, offset_s: i32) -> String {
    format!(
        "process history run pid {} {} started {} on {}: ran {}, {}",
        run.key.pid,
        clean(&run.name),
        format_local_short(run.started_utc_ns, offset_s),
        gpu_text(&run.gpus),
        runtime_text(run.runtime_ns),
        status_text(run)
    )
}

fn render_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    (run, detail): (&RunRowView, Option<&RunDetailView>),
    view: &ProcessHistoryView,
    context: &DisplayContext,
    output: &mut Output,
) {
    let mut rows = RowCursor::new(area);
    for line in detail_lines(run, detail, view.utc_offset_s, area.width, context) {
        let Some(row) = rows.next_row() else { return };
        output.label(line_text(&line));
        draw_line(frame.buffer_mut(), row, &line);
    }
    let Some(detail) = detail.filter(|detail| !detail.loading) else {
        if let Some(body) = rows.rest() {
            message(
                frame,
                body,
                "reading this run's history…",
                context.theme.muted_style(),
            );
        }
        return;
    };
    let Some(body) = rows.rest() else { return };
    render_charts(frame, body, (detail, view), context, output);
}

/// The run's identity, times, folder, program, parent and command line.
fn detail_lines(
    run: &RunRowView,
    detail: Option<&RunDetailView>,
    offset_s: i32,
    width: u16,
    context: &DisplayContext,
) -> Vec<Line<'static>> {
    let theme = &context.theme;
    let swatch = run.slot.map_or_else(
        || Span::raw("  "),
        |slot| Span::styled("■ ", Style::new().fg(theme.process_color(slot))),
    );
    let mut lines = vec![Line::from(vec![
        swatch,
        Span::styled(format!("PID {}", run.key.pid), theme.label_style()),
        separator(context),
        Span::styled(clean(&run.name), theme.title_style()),
        separator(context),
        Span::styled(gpu_text(&run.gpus), theme.label_style()),
        separator(context),
        Span::styled(status_text(run), status_style(run, context)),
    ])];
    let info = detail.map(|detail| &detail.info);
    let mut times = vec![field(
        "started",
        format_local_datetime(run.started_utc_ns, offset_s),
        context,
    )];
    if let Some(end) = run.end {
        times.push(field(
            "ended",
            format_local_datetime(end.utc_ns, offset_s),
            context,
        ));
    }
    let ran = if run.live { "running for" } else { "ran" };
    times.push(field(ran, runtime_text(run.runtime_ns), context));
    if let Some(uid) = info.and_then(|info| info.uid) {
        times.push(field("uid", uid.to_string(), context));
    }
    lines.push(joined(times, context));
    let text = |pick: fn(&RunInfo) -> &str| text_or_unknown(info.map_or("", pick));
    lines.push(joined(
        vec![
            field("folder", text(|info| &info.cwd), context),
            field("program", text(|info| &info.details.executable), context),
        ],
        context,
    ));
    lines.push(joined(
        vec![
            field("parent", parent_text(info), context),
            field("nice", nice_text(info), context),
            field("cgroup", text(|info| &info.details.cgroup), context),
        ],
        context,
    ));
    let command = info.map_or("", |info| info.command.as_str());
    lines.extend(command_lines(command, width, context));
    lines
}

/// `7 bash`: the parent's PID and name as first read.
fn parent_text(info: Option<&RunInfo>) -> String {
    let Some(details) = info.map(|info| &info.details) else {
        return NOT_AVAILABLE.to_owned();
    };
    match (details.parent_pid, details.parent_name.as_str()) {
        (None, _) => NOT_AVAILABLE.to_owned(),
        (Some(pid), "") => pid.to_string(),
        (Some(pid), name) => format!("{pid} {}", clean(name)),
    }
}

fn nice_text(info: Option<&RunInfo>) -> String {
    info.and_then(|info| info.details.nice)
        .map_or_else(|| NOT_AVAILABLE.to_owned(), |nice| nice.to_string())
}

/// `label value`, the label in the label color.
fn field(label: &str, value: String, context: &DisplayContext) -> Vec<Span<'static>> {
    let theme = &context.theme;
    vec![
        Span::styled(format!("{label} "), theme.label_style()),
        Span::styled(value, theme.text_style()),
    ]
}

/// Fields on one line between separators.
fn joined(fields: Vec<Vec<Span<'static>>>, context: &DisplayContext) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, field) in fields.into_iter().enumerate() {
        if index > 0 {
            spans.push(separator(context));
        }
        spans.extend(field);
    }
    Line::from(spans)
}

/// `command python -m …`: one line cut to fit, or with `expanded` the
/// whole of it wrapped onto at most [`MAX_COMMAND_LINES`] lines.
fn command_lines(command: &str, width: u16, context: &DisplayContext) -> Vec<Line<'static>> {
    let theme = &context.theme;
    const LEAD: &str = "command ";
    let text = text_or_unknown(command);
    let room = usize::from(width).saturating_sub(LEAD.len()).max(1);
    if !context.expanded || display_width(&text) <= room {
        return vec![Line::from(vec![
            Span::styled(LEAD, theme.label_style()),
            Span::styled(truncate_to_width(&text, room), theme.text_style()),
        ])];
    }
    let mut lines = Vec::new();
    let mut rest = text.as_str();
    while !rest.is_empty() && lines.len() < MAX_COMMAND_LINES {
        let piece = truncate_to_width(rest, room);
        let taken = if lines.len() + 1 == MAX_COMMAND_LINES {
            rest.len()
        } else {
            take_width(rest, room)
        };
        let lead = if lines.is_empty() { LEAD } else { "        " };
        let shown = if taken == rest.len() {
            piece
        } else {
            rest[..taken].to_owned()
        };
        lines.push(Line::from(vec![
            Span::styled(lead, theme.label_style()),
            Span::styled(shown, theme.text_style()),
        ]));
        rest = &rest[taken..];
    }
    lines
}

/// Bytes of `text` that fit `width` columns, whole characters.
fn take_width(text: &str, width: usize) -> usize {
    let mut used = 0;
    for (index, character) in text.char_indices() {
        let columns = display_width(&character.to_string());
        if used + columns > width {
            return index;
        }
        used += columns;
    }
    text.len()
}

/// The bar row, the panels and the axis, laid out like the Overview
/// History's (module docs).
fn render_charts(
    frame: &mut Frame<'_>,
    area: Rect,
    (detail, view): (&RunDetailView, &ProcessHistoryView),
    context: &DisplayContext,
    output: &mut Output,
) {
    let panels = run_panels(detail);
    let strips: Vec<u16> = panels
        .iter()
        .map(|panel| saturate_u16(panel.strips.len()))
        .collect();
    let layout = plan(area.width, area.height, &strips, (&[], 0));
    let mut rows = RowCursor::new(area);
    if let Some(row) = rows.next_row() {
        let hidden = Hidden {
            panels: layout.hidden_panels,
            ..Hidden::default()
        };
        render_bar_row(frame, row, (detail, view.bar), hidden, context);
    }
    output.label(format!(
        "process history bars: {}",
        bar_note(detail, view.bar)
    ));
    let window = Window {
        start_ns: detail.window_start_utc_ns,
        end_ns: detail.window_end_utc_ns,
    };
    render_bands(
        frame,
        &mut rows,
        (&layout, &panels),
        window,
        context,
        output,
    );
    if let Some(axis) = rows.next_row() {
        let utc = |ns: u64| i64::try_from(ns).unwrap_or(i64::MAX);
        let span = (utc(window.start_ns), utc(window.end_ns));
        render_axes(
            frame,
            axis,
            &layout,
            |width| clock_ticks(span, view.utc_offset_s, width),
            context,
        );
    }
}

/// `auto [1m] 5m … 24h`, the session's size marked, then at the right
/// what a bar is and how long the charts span, and what did not fit.
fn render_bar_row(
    frame: &mut Frame<'_>,
    row: Rect,
    (detail, chosen): (&RunDetailView, BarSize),
    hidden: Hidden,
    context: &DisplayContext,
) {
    let theme = &context.theme;
    let spans: Vec<Span<'static>> = BarSize::ALL
        .into_iter()
        .map(|bar| {
            let style = if bar == chosen {
                theme.selected_row_style()
            } else {
                theme.muted_style()
            };
            Span::styled(format!(" {} ", bar.token()), style)
        })
        .collect();
    let left = Line::from(spans);
    let room = usize::from(row.width).saturating_sub(left.width() + 2 * gap(context).width());
    let bars = bar_note(detail, chosen);
    let mut notes: Vec<String> = hidden
        .notes()
        .into_iter()
        .map(|hidden| format!("{hidden} · {bars}"))
        .collect();
    notes.push(bars);
    notes.extend(hidden.notes());
    let note = notes.into_iter().find(|note| display_width(note) <= room);
    draw_caption(frame.buffer_mut(), row, &left, note.as_deref(), context);
}

/// `auto: 10m a bar · 13h 20m shown`, `24h a bar · 80d shown`.
fn bar_note(detail: &RunDetailView, chosen: BarSize) -> String {
    let seconds = detail
        .window_end_utc_ns
        .saturating_sub(detail.window_start_utc_ns)
        / 1_000_000_000;
    let shown = if seconds >= 86_400 && seconds.is_multiple_of(86_400) {
        format!("{}d", seconds / 86_400)
    } else {
        format_span_seconds(seconds)
    };
    let lead = if chosen == BarSize::Auto {
        "auto: "
    } else {
        ""
    };
    format!("{lead}{} a bar · {shown} shown", detail.bar.token())
}

/// A strip of a run panel: its metric, label and paint.
type Member = (RunMetric, &'static str, Paint);

/// The panels of a run, in order: CPU, memory, GPU utilization and memory,
/// tasks, disk and the host's network. A strip without a sample gives way
/// to the others of its panel; a panel keeps its first strip so it says
/// it has none.
fn run_panels(detail: &RunDetailView) -> Vec<Panel<'_>> {
    let charts = RunCharts { detail };
    [
        charts.panel(
            charts.titled("CPU", RunMetric::Cpu),
            &[
                (RunMetric::Cpu, "self", Paint::Levels),
                (RunMetric::ChildrenCpu, "child", Paint::Levels),
            ],
        ),
        charts.panel(
            charts.titled("Memory", RunMetric::Memory),
            &[
                (RunMetric::Memory, "rss", Paint::Levels),
                (RunMetric::ChildrenMemory, "child", Paint::Levels),
                (RunMetric::Swap, "swap", Paint::Levels),
            ],
        ),
        charts.single("GPU utilization".to_owned(), RunMetric::GpuUtilization),
        charts.single(
            charts.titled("GPU memory", RunMetric::GpuMemory),
            RunMetric::GpuMemory,
        ),
        charts.panel(
            "Tasks".to_owned(),
            &[
                (RunMetric::Threads, "thread", Paint::Count),
                (RunMetric::Children, "child", Paint::Count),
                (RunMetric::OpenFiles, "files", Paint::Count),
            ],
        ),
        charts.panel(
            "Disk".to_owned(),
            &[
                (RunMetric::DiskRead, "read", Paint::Read),
                (RunMetric::DiskWrite, "write", Paint::Write),
            ],
        ),
        // The kernel counts no traffic per process.
        charts.panel(
            "Network · host".to_owned(),
            &[
                (RunMetric::HostNetworkReceive, "rx", Paint::Receive),
                (RunMetric::HostNetworkTransmit, "tx", Paint::Transmit),
            ],
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// The opened run's charts as strips and panels.
struct RunCharts<'a> {
    detail: &'a RunDetailView,
}

impl<'a> RunCharts<'a> {
    fn chart(&self, metric: RunMetric) -> Option<&'a RunChartView> {
        self.detail
            .charts
            .iter()
            .find(|chart| chart.metric == metric)
    }

    fn series(&self, chart: &'a RunChartView) -> Series<'a> {
        let latest = if self.detail.live { "now" } else { "last" };
        Series {
            points: &chart.points,
            parts: &[],
            unit: chart.unit,
            latest: (latest, chart.latest),
            mean: chart.mean,
            peak: chart.peak,
            energy: None,
            state: SeriesState::Ready,
        }
    }

    /// `CPU · 20 CPUs`, `Memory · 128.0 GiB`: the title with its scale.
    fn titled(&self, title: &str, metric: RunMetric) -> String {
        let Some((scale, unit)) = self
            .chart(metric)
            .and_then(|chart| Some((chart.scale_max?, chart.unit)))
        else {
            return title.to_owned();
        };
        match unit {
            CanonicalUnit::Percent => format!(
                "{title} · {} CPUs",
                format_value(scale / 100.0, CanonicalUnit::Count)
            ),
            unit => format!("{title} · {}", format_value(scale, unit)),
        }
    }

    fn strip(&self, (metric, label, paint): Member) -> Option<Strip<'a>> {
        let chart = self.chart(metric)?;
        let scale = match paint {
            Paint::Levels => chart.scale_max.or_else(|| positive(chart.peak)),
            Paint::Count => positive(chart.peak),
            Paint::Read | Paint::Write | Paint::Receive | Paint::Transmit => None,
        };
        Some(Strip {
            label: label.to_owned(),
            series: self.series(chart),
            scale,
            paint,
        })
    }

    fn panel(&self, title: String, members: &[Member]) -> Option<Panel<'a>> {
        let strips: Vec<Strip<'a>> = members
            .iter()
            .filter_map(|member| self.strip(*member))
            .collect();
        let first = strips.first().cloned();
        let mut shown: Vec<Strip<'a>> = strips
            .into_iter()
            .filter(|strip| !strip.series.points.is_empty())
            .collect();
        if shown.is_empty() {
            shown.extend(first);
        }
        (!shown.is_empty()).then_some(Panel {
            title,
            strips: shown,
        })
    }

    /// A panel of one bounded series, scaled to its peak without a scale.
    fn single(&self, title: String, metric: RunMetric) -> Option<Panel<'a>> {
        let chart = self.chart(metric)?;
        let scale = chart.scale_max.or_else(|| positive(chart.peak));
        Some(single(title, self.series(chart), scale))
    }
}

fn positive(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite() && *value > 0.0)
}

/// `2h 15m`, `3d 4h`: how long a run ran.
fn runtime_text(runtime_ns: u64) -> String {
    format_span_seconds(runtime_ns / 1_000_000_000)
}

fn bytes_text(value: Option<f64>) -> String {
    value.map_or_else(
        || NOT_AVAILABLE.to_owned(),
        |value| format_value(value, CanonicalUnit::Bytes),
    )
}

fn count_text(value: Option<f64>) -> String {
    value.map_or_else(
        || NOT_AVAILABLE.to_owned(),
        |value| format_value(value, CanonicalUnit::Count),
    )
}

fn percent_text(value: Option<f64>) -> String {
    value.map_or_else(
        || NOT_AVAILABLE.to_owned(),
        |value| format_value(value, CanonicalUnit::Percent),
    )
}

/// `GPU 0, GPU 1`.
fn gpu_text(gpus: &[String]) -> String {
    if gpus.is_empty() {
        return NOT_AVAILABLE.to_owned();
    }
    gpus.iter()
        .map(|gpu| clean(gpu))
        .collect::<Vec<_>>()
        .join(", ")
}

fn status_text(run: &RunRowView) -> String {
    match run.end {
        Some(end) => end.cause.text(),
        None if run.live => RUNNING.to_owned(),
        None => NOT_AVAILABLE.to_owned(),
    }
}

/// Running in the text color, a kill as a warning, any other end muted.
fn status_style(run: &RunRowView, context: &DisplayContext) -> Style {
    let theme = &context.theme;
    match run.end {
        Some(end) if end.cause.was_killed() => theme.tone_style(crate::format::CellTone::Conflict),
        Some(_) => theme.muted_style(),
        None => theme.text_style(),
    }
}

fn text_or_unknown(text: &str) -> String {
    if text.is_empty() {
        NOT_AVAILABLE.to_owned()
    } else {
        clean(text)
    }
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtimes_read_in_hours_and_minutes() {
        assert_eq!(runtime_text(135 * 60 * 1_000_000_000), "2h 15m");
        assert_eq!(runtime_text(42 * 1_000_000_000), "42s");
    }
}
