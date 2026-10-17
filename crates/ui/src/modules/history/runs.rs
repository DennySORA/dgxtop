//! The Process History tab (D-76, D-77): every GPU process run kept (the
//! latest 512), as the history orders them, the running ones first (D-115,
//! `tab` 1), and one run's whole record (`tab` 2). The run's page keeps
//! its top fixed ([`run_header`](super::run_header), D-116): what it ran
//! and where, its parent, program and control group, when it started and
//! ended and why, then the bar sizes `w` steps through. A divider joined
//! to the panel's borders parts it from the charts, laid out like the
//! Overview History's: panels of strips with their scale and legend, each
//! column a bar of the chosen size, on the local clock's axis. Charts that
//! would squeeze their strips past their legends go on pages (D-112), the
//! context's `offset` the page, which the divider names at its right with
//! what the next page holds and the keys turning pages. The context's
//! scroll is the list's cursor; its `expanded` shows the whole command
//! line; its `marked` runs carry a mark, in the warning color, for the
//! deletion (D-108).

use std::ops::Range;

use dgxtop_core::metric::CanonicalUnit;
use dgxtop_core::process_history::{GpuFigure, RUN_GPU_SLOTS, RunMetric};
use dgxtop_core::tiers::BarSize;
use dgxtop_core::view::{
    HISTORY_GUTTER_COLUMNS, ProcessHistoryView, RunChartView, RunDetailView, RunRowView,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::axis::clock_ticks;
use super::layout::{AXIS_ROWS, HistoryLayout, paginate, plan};
use super::panels::{Paint, Panel, Series, SeriesState, Strip};
use super::run_header::header_lines;
use super::strip::Window;
use super::{render_axes, render_bands};
use crate::format::{
    NOT_AVAILABLE, clean, counted, display_width, format_local_short, format_span_seconds,
    format_value,
};
use crate::modules::common::{
    ColumnSpec, DividerForm, Output, RowCursor, TableCell, draw_caption, draw_divider, draw_line,
    draw_table_header, draw_table_row, fit_columns, gap, message, page_divider_forms,
};
use crate::num::saturate_u16;
use crate::registry::{DisplayContext, DisplayModuleManifest, controls};
use dgxtop_core::view::ViewModel;

const SWATCH: &str = "■";
const MARK: &str = "✓";
const RUNNING: &str = "running";

/// A list column: title, right-aligned, priority (lower gives way first).
struct Column {
    title: &'static str,
    right: bool,
    priority: u8,
}

/// The mark, then the run's chart color, then its record.
const COLUMNS: [Column; 13] = [
    Column {
        title: "",
        right: false,
        priority: 255,
    },
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
    (view, frame_of): (&ProcessHistoryView, (&DisplayModuleManifest, &ViewModel)),
    context: &DisplayContext,
    output: &mut Output,
) {
    let Some(run) = cursor_run(view, context.scroll) else {
        let filter = clean(frame_of.1.session.filter.trim());
        let text = if filter.is_empty() {
            "no GPU process was seen yet".to_owned()
        } else {
            format!("no run's name or PID holds \"{filter}\" · / then Esc clears the filter")
        };
        message(frame, area, &text, context.theme.muted_style());
        output.label(format!("process history: {text}"));
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
    render_detail(
        frame,
        area,
        (run, detail),
        (view, frame_of),
        context,
        output,
    );
}

/// The panels of an opened run and the page of them shown (D-112).
struct RunLayout<'a> {
    panels: Vec<Panel<'a>>,
    /// Each page's panels, as positions.
    pages: Vec<Range<usize>>,
    page: usize,
}

impl<'a> RunLayout<'a> {
    /// Pages `detail`'s panels for the charts' rows `body`, under the
    /// divider: its bands, then the time axis.
    fn new(detail: &'a RunDetailView, body: Rect, context: &DisplayContext) -> Self {
        let panels = run_panels(detail);
        let strips: Vec<u16> = panels
            .iter()
            .map(|panel| saturate_u16(panel.strips.len()))
            .collect();
        let pages = paginate(body.width, body.height.saturating_sub(AXIS_ROWS), &strips);
        let page = usize::from(context.offset).min(pages.len() - 1);
        Self {
            panels,
            pages,
            page,
        }
    }

    /// The panels of the page shown.
    fn shown(&self) -> &[Panel<'a>] {
        &self.panels[self.pages[self.page].clone()]
    }

    /// What the divider says (D-116): `page 1 of 2 · next: GPU · 120 GiB,
    /// Tasks, Disk, Network · host` with the keys turning pages, in shorter
    /// forms; nothing on one page.
    fn divider_forms(&self) -> Vec<DividerForm> {
        let next: Vec<String> = self
            .pages
            .get(self.page + 1)
            .map(|next| {
                self.panels[next.clone()]
                    .iter()
                    .map(|panel| clean(&panel.title))
                    .collect()
            })
            .unwrap_or_default();
        page_divider_forms((self.page, self.pages.len()), &next, "panel")
    }
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
        let style = (index == cursor).then(|| cursor_style(&view.runs[index], context));
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
        let marked = context.marked.contains(&run.key);
        output.label(run_label(run, view.utc_offset_s, marked));
    }
}

/// The cursor's row: highlighted, a marked run still in the warning color.
fn cursor_style(run: &RunRowView, context: &DisplayContext) -> Style {
    let selected = context.theme.selected_row_style();
    if context.marked.contains(&run.key) {
        selected.fg(context.theme.warning)
    } else {
        selected
    }
}

fn list_cells(
    run: &RunRowView,
    view: &ProcessHistoryView,
    context: &DisplayContext,
) -> Vec<TableCell> {
    let theme = &context.theme;
    let marked = context.marked.contains(&run.key);
    let (text, label) = if marked {
        let warning = Style::new().fg(theme.warning);
        (warning, warning)
    } else if run.live {
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
        TableCell::new(if marked { MARK } else { " " }, text),
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
/// ran 1h 06m, SIGKILL by dgxtop`, and `, marked` for a marked run.
fn run_label(run: &RunRowView, offset_s: i32, marked: bool) -> String {
    format!(
        "process history run pid {} {} started {} on {}: ran {}, {}{}",
        run.key.pid,
        clean(&run.name),
        format_local_short(run.started_utc_ns, offset_s),
        gpu_text(&run.gpus),
        runtime_text(run.runtime_ns),
        status_text(run),
        if marked { ", marked" } else { "" }
    )
}

/// The run's page (module docs): its fixed top, the header and the bar
/// sizes, then the divider and the page of charts shown.
fn render_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    (run, detail): (&RunRowView, Option<&RunDetailView>),
    (view, frame_of): (&ProcessHistoryView, (&DisplayModuleManifest, &ViewModel)),
    context: &DisplayContext,
    output: &mut Output,
) {
    let header = header_lines(run, detail, view.utc_offset_s, area.width, context);
    let mut rows = RowCursor::new(area);
    for line in &header {
        output.label(line_text(line));
        if let Some(row) = rows.next_row() {
            draw_line(frame.buffer_mut(), row, line);
        }
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
    let (Some(bar_row), Some(divider)) = (rows.next_row(), rows.next_row()) else {
        return;
    };
    let body = rows
        .rest()
        .unwrap_or(Rect::new(area.x, area.bottom(), area.width, 0));
    let pages = RunLayout::new(detail, body, context);
    let shown = pages.shown();
    let strips: Vec<u16> = shown
        .iter()
        .map(|panel| saturate_u16(panel.strips.len()))
        .collect();
    let layout = plan(
        body.width,
        body.height.saturating_sub(AXIS_ROWS),
        &strips,
        0,
    );
    render_bar_row(
        frame,
        bar_row,
        (detail, view.bar),
        layout.panels_below,
        context,
    );
    output.label(format!(
        "process history bars: {}",
        bar_note(detail, view.bar)
    ));
    draw_divider(frame.buffer_mut(), divider, &pages.divider_forms(), context);
    render_charts(
        frame,
        body,
        (detail, view.utc_offset_s),
        (&layout, shown),
        context,
        output,
    );
    publish_pages(output, frame_of, body, &pages);
}

/// Tells the controller the pages before and after the one shown, on
/// the charts' rows (D-112).
fn publish_pages(
    output: &mut Output,
    (manifest, model): (&DisplayModuleManifest, &ViewModel),
    body: Rect,
    layout: &RunLayout<'_>,
) {
    for _ in 0..layout.page {
        output.target(model, manifest, controls::PAGE_ABOVE, body);
    }
    for _ in layout.page + 1..layout.pages.len() {
        output.target(model, manifest, controls::PAGE_BELOW, body);
    }
}

/// A page of the run's panels in `layout`, then the time axis, laid out
/// like the Overview History's (module docs).
fn render_charts(
    frame: &mut Frame<'_>,
    area: Rect,
    (detail, offset_s): (&RunDetailView, i32),
    (layout, panels): (&HistoryLayout, &[Panel<'_>]),
    context: &DisplayContext,
    output: &mut Output,
) {
    let mut rows = RowCursor::new(area);
    let window = Window {
        start_ns: detail.window_start_utc_ns,
        end_ns: detail.window_end_utc_ns,
    };
    render_bands(
        frame,
        &mut rows,
        (layout, panels),
        (window, None),
        context,
        output,
    );
    if let Some(axis) = rows.next_row() {
        let utc = |ns: u64| i64::try_from(ns).unwrap_or(i64::MAX);
        let span = (utc(window.start_ns), utc(window.end_ns));
        render_axes(
            frame,
            axis,
            layout,
            |width| clock_ticks(span, offset_s, width),
            context,
        );
    }
}

/// ` auto [1m] 5m … 24h`, the session's size marked and the sizes lined up
/// with the header's keys, then at the right what a bar is and how long
/// the charts span, and the panels of the page that did not fit.
fn render_bar_row(
    frame: &mut Frame<'_>,
    row: Rect,
    (detail, chosen): (&RunDetailView, BarSize),
    below: usize,
    context: &DisplayContext,
) {
    let theme = &context.theme;
    let mut spans = vec![Span::raw(" ")];
    spans.extend(BarSize::ALL.into_iter().map(|bar| {
        let style = if bar == chosen {
            theme.selected_row_style()
        } else {
            theme.muted_style()
        };
        Span::styled(format!(" {} ", bar.token()), style)
    }));
    let left = Line::from(spans);
    let room = usize::from(row.width).saturating_sub(left.width() + 2 * gap(context).width());
    let bars = bar_note(detail, chosen);
    let notes = match below {
        0 => vec![bars],
        below => {
            let waiting = format!("+{} on a taller terminal", counted(below, "panel"));
            vec![format!("{waiting} · {bars}"), bars, waiting]
        }
    };
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

/// The panels of a run, in order: CPU, memory, its GPUs' utilization and
/// memory ([`RunCharts::gpu_panel`]), tasks, disk and the host's network. Six, so two a band they
/// pair up to the last (D-117), the disk beside the network as in the
/// Overview History. The open files stay a strip of the tasks: they are
/// read of one's own processes only (D-77), and a panel of their own would
/// stand empty for another user's run. A strip without a sample gives way
/// to the others of its panel; a panel keeps its first strip so it says it
/// has none.
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
        charts.gpu_panel(),
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

    fn strip(&self, (metric, label, paint): (RunMetric, String, Paint)) -> Option<Strip<'a>> {
        let chart = self.chart(metric)?;
        let scale = match paint {
            Paint::Levels => chart.scale_max.or_else(|| positive(chart.peak)),
            Paint::Count => positive(chart.peak),
            Paint::Read | Paint::Write | Paint::Receive | Paint::Transmit => None,
        };
        Some(Strip {
            label,
            series: self.series(chart),
            scale,
            paint,
        })
    }

    fn panel(&self, title: String, members: &[Member]) -> Option<Panel<'a>> {
        let labelled = members
            .iter()
            .map(|(metric, label, paint)| (*metric, (*label).to_owned(), *paint))
            .collect();
        self.labelled_panel(title, labelled)
    }

    /// The GPU panel (D-117, D-120): for each GPU the run used, in the
    /// order it first used them, its utilization there, else the GPU's own
    /// (`dev`) where the driver sampled no process (GB10), then its memory
    /// there. A run recorded before the GPUs were kept apart shows the
    /// sums over its GPUs.
    fn gpu_panel(&self) -> Option<Panel<'a>> {
        let has_points = |metric| {
            self.chart(metric)
                .is_some_and(|chart| !chart.points.is_empty())
        };
        let mut members = Vec::new();
        for (slot, label) in self.detail.info.gpus.iter().enumerate().take(RUN_GPU_SLOTS) {
            let (Some(utilization), Some(device), Some(memory)) = (
                RunMetric::of_gpu(slot, GpuFigure::Utilization),
                RunMetric::of_gpu(slot, GpuFigure::Device),
                RunMetric::of_gpu(slot, GpuFigure::Memory),
            ) else {
                continue;
            };
            members.push(if has_points(utilization) {
                (
                    utilization,
                    gpu_strip_label("util", label, slot),
                    Paint::Levels,
                )
            } else {
                (device, gpu_strip_label("dev", label, slot), Paint::Levels)
            });
            members.push((memory, gpu_strip_label("mem", label, slot), Paint::Levels));
        }
        // The GPUs past those kept apart are in the sums only.
        if self.detail.info.gpus.len() > RUN_GPU_SLOTS {
            members.push((
                RunMetric::GpuUtilization,
                "util Σ".to_owned(),
                Paint::Levels,
            ));
            members.push((RunMetric::GpuMemory, "mem Σ".to_owned(), Paint::Levels));
        }
        if !members.iter().any(|(metric, ..)| has_points(*metric)) {
            return self.panel(
                self.titled("GPU", RunMetric::GpuMemory),
                &[
                    (RunMetric::GpuUtilization, "util", Paint::Levels),
                    (RunMetric::GpuMemory, "memory", Paint::Levels),
                ],
            );
        }
        self.labelled_panel("GPU".to_owned(), members)
    }

    fn labelled_panel(
        &self,
        title: String,
        members: Vec<(RunMetric, String, Paint)>,
    ) -> Option<Panel<'a>> {
        let strips: Vec<Strip<'a>> = members
            .into_iter()
            .filter_map(|member| self.strip(member))
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
            fold: None,
        })
    }
}

/// A strip's label of the run's GPU in `slot`, labelled `label`, in the
/// gutter's six columns: `util 3` of GPU 3, `util10` past GPU 9; a GPU
/// labelled without its number (no NVML index) by its letter in the run,
/// `util a`, never a number it does not have.
fn gpu_strip_label(stem: &str, label: &str, slot: usize) -> String {
    let mark = label
        .strip_prefix("GPU ")
        .filter(|number| number.len() <= 2 && number.bytes().all(|byte| byte.is_ascii_digit()))
        .map_or_else(
            || char::from(b'a'.saturating_add(u8::try_from(slot).unwrap_or(u8::MAX))).to_string(),
            str::to_owned,
        );
    let spaced = format!("{stem} {mark}");
    if display_width(&spaced) < usize::from(HISTORY_GUTTER_COLUMNS) {
        spaced
    } else {
        format!("{stem}{mark}")
    }
}

fn positive(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite() && *value > 0.0)
}

/// `2h 15m`, `3d 4h`: how long a run ran.
pub(super) fn runtime_text(runtime_ns: u64) -> String {
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
pub(super) fn gpu_text(gpus: &[String]) -> String {
    if gpus.is_empty() {
        return NOT_AVAILABLE.to_owned();
    }
    gpus.iter()
        .map(|gpu| clean(gpu))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn status_text(run: &RunRowView) -> String {
    match run.end {
        Some(end) => end.cause.text(),
        None if run.live => RUNNING.to_owned(),
        None => NOT_AVAILABLE.to_owned(),
    }
}

/// Running in the text color, a kill as a warning, any other end muted.
pub(super) fn status_style(run: &RunRowView, context: &DisplayContext) -> Style {
    let theme = &context.theme;
    match run.end {
        Some(end) if end.cause.was_killed() => theme.tone_style(crate::format::CellTone::Conflict),
        Some(_) => theme.muted_style(),
        None => theme.text_style(),
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

    /// A GPU's strips name it in the gutter's six columns (D-120): its
    /// number, without the space past GPU 9, else its letter in the run.
    #[test]
    fn gpu_strips_name_their_gpu_in_six_columns() {
        assert_eq!(gpu_strip_label("util", "GPU 3", 0), "util 3");
        assert_eq!(gpu_strip_label("util", "GPU 10", 1), "util10");
        assert_eq!(gpu_strip_label("mem", "GPU 10", 1), "mem 10");
        assert_eq!(gpu_strip_label("dev", "GPU cccc3333", 2), "dev c");
    }

    #[test]
    fn runtimes_read_in_hours_and_minutes() {
        assert_eq!(runtime_text(135 * 60 * 1_000_000_000), "2h 15m");
        assert_eq!(runtime_text(42 * 1_000_000_000), "42s");
    }
}
