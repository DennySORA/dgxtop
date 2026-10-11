//! The Process History tab (D-76): every GPU process run of the last 7
//! days, the latest start first (`tab` 1), and one run's whole record
//! (`tab` 2): what it ran and where, when it started and ended and why, and
//! a chart of each thing it used. The context's scroll is the cursor; its
//! `expanded` shows the whole command line.

use dgxtop_core::process_history::RunMetric;
use dgxtop_core::view::{ProcessHistoryView, RunChartView, RunDetailView, RunRowView};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::format::{
    NOT_AVAILABLE, clean, display_width, format_local_datetime, format_local_short,
    format_span_seconds, format_value, truncate_to_width,
};
use crate::modules::common::{
    ColumnSpec, Output, RowCursor, TableCell, draw_line, draw_table_header, draw_table_row,
    fit_columns, message, render_chart_with_parts, separator,
};
use crate::num::saturate_u16;
use crate::registry::DisplayContext;

const SWATCH: &str = "■";
const RUNNING: &str = "running";
/// Command lines an expanded command takes at most.
const MAX_COMMAND_LINES: usize = 8;
/// Rows a chart takes with its title.
const MIN_CHART_ROWS: u16 = 3;

/// A list column: title, right-aligned, priority (lower gives way first).
struct Column {
    title: &'static str,
    right: bool,
    priority: u8,
}

const COLUMNS: [Column; 10] = [
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
        title: "STATUS",
        right: false,
        priority: 230,
    },
];

/// The run under the cursor, the cursor clamped to the list.
pub(crate) fn cursor_run(view: &ProcessHistoryView, scroll: u16) -> Option<&RunRowView> {
    let last = view.runs.len().checked_sub(1)?;
    view.runs.get(usize::from(scroll).min(last))
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
            ", kept 7 days"
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
    render_detail(frame, area, run, detail, view.utc_offset_s, context, output);
}

fn render_list(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ProcessHistoryView,
    context: &DisplayContext,
    output: &mut Output,
) {
    let cursor = usize::from(context.scroll).min(view.runs.len().saturating_sub(1));
    let rows: Vec<Vec<TableCell>> = view
        .runs
        .iter()
        .map(|run| list_cells(run, view.utc_offset_s, context))
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

fn list_cells(run: &RunRowView, offset_s: i32, context: &DisplayContext) -> Vec<TableCell> {
    let theme = &context.theme;
    let (text, label) = if run.live {
        (theme.text_style(), theme.label_style())
    } else {
        (theme.muted_style(), theme.muted_style())
    };
    let swatch = run.slot.map_or_else(
        || TableCell::new(" ", text),
        |slot| TableCell::new(SWATCH, Style::new().fg(theme.process_color(slot))),
    );
    vec![
        swatch,
        TableCell::new(format_local_short(run.started_utc_ns, offset_s), label),
        TableCell::new(run.key.pid.to_string(), label),
        TableCell::new(clean(&run.name), text),
        TableCell::new(gpu_text(&run.gpus), label),
        TableCell::new(runtime_text(run.runtime_ns), text),
        TableCell::new(bytes_text(run.peak_gpu_memory), text),
        TableCell::new(percent_text(run.mean_gpu_utilization), text),
        TableCell::new(percent_text(run.mean_cpu), text),
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
    run: &RunRowView,
    detail: Option<&RunDetailView>,
    offset_s: i32,
    context: &DisplayContext,
    output: &mut Output,
) {
    let mut rows = RowCursor::new(area);
    for line in detail_lines(run, detail, offset_s, area.width, context) {
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
    render_charts(frame, body, detail, context, output);
}

/// The run's identity, times, folder and command line.
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
    let mut times = vec![
        Span::styled("started ", theme.label_style()),
        Span::styled(
            format_local_datetime(run.started_utc_ns, offset_s),
            theme.text_style(),
        ),
    ];
    if let Some(end) = run.end {
        times.push(separator(context));
        times.push(Span::styled("ended ", theme.label_style()));
        times.push(Span::styled(
            format_local_datetime(end.utc_ns, offset_s),
            theme.text_style(),
        ));
    }
    times.push(separator(context));
    times.push(Span::styled(
        if run.live { "running for " } else { "ran " },
        theme.label_style(),
    ));
    times.push(Span::styled(
        runtime_text(run.runtime_ns),
        theme.text_style(),
    ));
    if let Some(uid) = detail.and_then(|detail| detail.info.uid) {
        times.push(separator(context));
        times.push(Span::styled("uid ", theme.label_style()));
        times.push(Span::styled(uid.to_string(), theme.text_style()));
    }
    lines.push(Line::from(times));
    let info = detail.map(|detail| &detail.info);
    let cwd = info.map_or("", |info| info.cwd.as_str());
    lines.push(Line::from(vec![
        Span::styled("folder ", theme.label_style()),
        Span::styled(text_or_unknown(cwd), theme.text_style()),
    ]));
    let command = info.map_or("", |info| info.command.as_str());
    lines.extend(command_lines(command, width, context));
    lines
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

/// Eight charts, two side by side when the width allows.
fn render_charts(
    frame: &mut Frame<'_>,
    area: Rect,
    detail: &RunDetailView,
    context: &DisplayContext,
    output: &mut Output,
) {
    let columns: u16 = if area.width >= 100 { 2 } else { 1 };
    let count = saturate_u16(detail.charts.len());
    let bands = count.div_ceil(columns).max(1);
    let band_rows = (area.height / bands).max(MIN_CHART_ROWS);
    let width = area.width / columns;
    for (index, chart) in detail.charts.iter().enumerate() {
        let index = saturate_u16(index);
        let (band, column) = (index / columns, index % columns);
        let y = area.y.saturating_add(band.saturating_mul(band_rows));
        if y.saturating_add(MIN_CHART_ROWS) > area.bottom() {
            break;
        }
        let height = band_rows.min(area.bottom() - y);
        let x = area.x + column * width;
        let chart_width = if column + 1 == columns {
            area.right() - x
        } else {
            width.saturating_sub(1)
        };
        let chart_area = Rect::new(x, y, chart_width, height);
        render_chart_cell(frame, chart_area, chart, detail.slot, context);
        output.label(chart_label(chart));
    }
}

/// The chart's title with its mean and peak, then its points.
fn render_chart_cell(
    frame: &mut Frame<'_>,
    area: Rect,
    chart: &RunChartView,
    slot: Option<u8>,
    context: &DisplayContext,
) {
    let mut rows = RowCursor::new(area);
    let Some(title) = rows.next_row() else { return };
    let theme = &context.theme;
    let name = metric_title(chart.metric);
    draw_line(
        frame.buffer_mut(),
        title,
        &Line::from(vec![
            Span::styled(name, theme.section_title_style(name)),
            Span::styled(" · mean ", theme.label_style()),
            Span::styled(value_text(chart.mean, chart), theme.text_style()),
            Span::styled(" · peak ", theme.label_style()),
            Span::styled(value_text(chart.peak, chart), theme.text_style()),
        ]),
    );
    let Some(plot) = rows.rest() else { return };
    let max = match chart.unit {
        dgxtop_core::metric::CanonicalUnit::Percent => Some(chart.peak.unwrap_or(0.0).max(100.0)),
        _ => chart.peak,
    };
    let color = slot.map(|slot| theme.process_color(slot));
    let points = fit_to_width(&chart.means, usize::from(plot.width));
    render_chart_with_parts(frame, plot, &points, (Vec::new(), color), max, context);
}

/// The run's points fitted to `width` columns so its whole life spans the
/// chart: a short run's points each widened to the same number of
/// columns, a long run's merged, each column the mean of the points it
/// covers (`None` where none has a value).
fn fit_to_width(points: &[Option<f64>], width: usize) -> Vec<Option<f64>> {
    if width == 0 || points.is_empty() {
        return points.to_vec();
    }
    if points.len() <= width {
        let columns = width / points.len();
        return points
            .iter()
            .flat_map(|point| std::iter::repeat_n(*point, columns))
            .collect();
    }
    let per_column = points.len().div_ceil(width);
    points
        .chunks(per_column)
        .map(|chunk| {
            let present: Vec<f64> = chunk.iter().flatten().copied().collect();
            #[allow(clippy::cast_precision_loss)]
            let count = present.len() as f64;
            (!present.is_empty()).then(|| present.iter().sum::<f64>() / count)
        })
        .collect()
}

/// `2h 15m`, `3d 4h`: how long a run ran.
fn runtime_text(runtime_ns: u64) -> String {
    format_span_seconds(runtime_ns / 1_000_000_000)
}

fn chart_label(chart: &RunChartView) -> String {
    format!(
        "process history chart {}: mean {} peak {}",
        metric_title(chart.metric),
        value_text(chart.mean, chart),
        value_text(chart.peak, chart)
    )
}

/// The chart titles; per-process traffic is not counted by the kernel, so
/// the network charts say they are the host's.
const fn metric_title(metric: RunMetric) -> &'static str {
    match metric {
        RunMetric::Cpu => "CPU",
        RunMetric::Memory => "Memory",
        RunMetric::GpuUtilization => "GPU utilization",
        RunMetric::GpuMemory => "GPU memory",
        RunMetric::DiskRead => "Disk read",
        RunMetric::DiskWrite => "Disk write",
        RunMetric::HostNetworkReceive => "Network receive (host)",
        RunMetric::HostNetworkTransmit => "Network send (host)",
    }
}

fn value_text(value: Option<f64>, chart: &RunChartView) -> String {
    value.map_or_else(
        || NOT_AVAILABLE.to_owned(),
        |value| format_value(value, chart.unit),
    )
}

fn bytes_text(value: Option<f64>) -> String {
    value.map_or_else(
        || NOT_AVAILABLE.to_owned(),
        |value| format_value(value, dgxtop_core::metric::CanonicalUnit::Bytes),
    )
}

fn percent_text(value: Option<f64>) -> String {
    value.map_or_else(
        || NOT_AVAILABLE.to_owned(),
        |value| format_value(value, dgxtop_core::metric::CanonicalUnit::Percent),
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
    fn runs_fit_the_chart_width() {
        let points: Vec<Option<f64>> = (0..10).map(|value| Some(f64::from(value))).collect();
        let widened = fit_to_width(&points, 25);
        assert_eq!(widened.len(), 20, "each point two columns wide");
        assert_eq!(&widened[..4], &[Some(0.0), Some(0.0), Some(1.0), Some(1.0)]);
        assert_eq!(fit_to_width(&points, 10), points);
        assert_eq!(
            fit_to_width(&points, 5),
            vec![Some(0.5), Some(2.5), Some(4.5), Some(6.5), Some(8.5)]
        );
        let gappy = [None, None, Some(4.0), None];
        assert_eq!(fit_to_width(&gappy, 2), vec![None, Some(4.0)]);
        assert_eq!(fit_to_width(&points, 0), points);
    }

    #[test]
    fn runtimes_read_in_hours_and_minutes() {
        assert_eq!(runtime_text(135 * 60 * 1_000_000_000), "2h 15m");
        assert_eq!(runtime_text(42 * 1_000_000_000), "42s");
    }
}
