//! History module (D-46, D-48, D-50): the overview's resources over a
//! longer window, laid out like a monitoring dashboard ([`layout`]):
//!
//! - the window bar: the windows `w`/`W` step through, how much of the
//!   window is recorded (history starts with dgxtop) and what a short
//!   terminal leaves out;
//! - the panels ([`panels`]) in bands, two side by side on a wide terminal:
//!   the CPU and the memory, the GPUs' utilization and memory, their
//!   temperature and power, the disk and the network. Each panel is a title
//!   over its strips ([`strip`]): a gutter with the scale, block columns of
//!   the window, a legend with the value now, the mean and the peak;
//! - the time axis under each column of panels ([`axis`]);
//! - the GPUs' long-term table over 1 h to 24 h ([`stats`]);
//! - the tracked GPU processes over the window, in the colors of their
//!   parts of the memory charts ([`processes`], D-64).

mod axis;
mod layout;
mod panels;
mod processes;
mod stats;
mod strip;

use dgxtop_core::view::{
    DiskHistory, HISTORY_GUTTER_COLUMNS, HistoryChartKind, HistoryChartView, HistoryPointView,
    HistoryProcessView, HistoryView, ViewModel,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use self::axis::render_axis;
use self::layout::{HistoryLayout, plan};
use self::panels::{Paint, Panel, Strip, panels};
use self::processes::{process_labels, render_processes};
use self::stats::{
    GpuStats, gpu_stats, nanos_to_seconds, recorded_text, render_gpu_stats, stats_labels,
};
use self::strip::{Window, plot_width, render_strip, strip_text, tallest_column};
use super::ModuleBuildError;
use super::common::{
    ManifestSpec, Output, RowCursor, build_manifest, draw_caption, draw_line, gap, message,
    no_data, panel_with_keys,
};
use crate::format::{CellTone, clean, display_width, format_span_ns};
use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, RenderOutput, Slot, Topic,
};

/// Module id of the History view, whose pages `p` switches (D-72).
pub const HISTORY_MODULE_ID: &str = "history";

/// Windows `w` and `W` step through, in order.
const WINDOWS: [&str; 5] = ["5m", "1h", "6h", "12h", "24h"];
/// Share of the window below which the bar says how much is recorded.
const FULL_COVERAGE: f64 = 0.99;

pub struct HistoryModule {
    manifest: DisplayModuleManifest,
}

impl HistoryModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: HISTORY_MODULE_ID,
                title: "History of the overview charts",
                topics: &[Topic::History, Topic::Sources],
                min_columns: 40,
                min_rows: 6,
                priority: 45,
                slots: &[Slot::Detail],
            })?,
        })
    }
}

impl DisplayModule for HistoryModule {
    fn manifest(&self) -> &DisplayModuleManifest {
        &self.manifest
    }

    fn render(
        &self,
        context: &DisplayContext,
        model: &ViewModel,
        area: Rect,
        frame: &mut Frame<'_>,
    ) -> RenderOutput {
        let mut output = Output::default();
        let (title, keys) = match context.tab {
            0 => ("Overview History", WINDOW_KEYS),
            1 => ("Process History", PROCESS_LIST_KEYS),
            _ => ("Process History · process", PROCESS_DETAIL_KEYS),
        };
        let inner = panel_with_keys(frame, area, title, keys, context);
        let Some(history) = model.modules.history.as_deref() else {
            message(
                frame,
                inner,
                &no_data("history"),
                context.theme.muted_style(),
            );
            output.label("history: no data");
            return output.finish();
        };
        if context.tab == 0 {
            render_history(frame, inner, history, context, &mut output);
        } else {
            render_process_pages(frame, inner, history, context, &mut output);
        }
        output.finish()
    }
}

/// The window bar, the panels, the axis and the table (module docs).
fn render_history(
    frame: &mut Frame<'_>,
    inner: Rect,
    history: &HistoryView,
    context: &DisplayContext,
    output: &mut Output,
) {
    let panels = panels(history);
    let blocks = gpu_stats(&history.summaries);
    let strips: Vec<u16> = panels
        .iter()
        .map(|panel| saturate_u16(panel.strips.len()))
        .collect();
    let block_rows: Vec<u16> = blocks.iter().map(GpuStats::rows).collect();
    // The GPU processes have a tab of their own (D-75).
    let layout = plan(inner.width, inner.height, &strips, (&block_rows, 0));
    let mut rows = RowCursor::new(inner);
    if let Some(row) = rows.next_row() {
        let hidden = Hidden {
            panels: layout.hidden_panels,
            gpus: blocks.len() - layout.table_blocks,
            processes: 0,
        };
        render_window_bar(frame, row, history, hidden, context);
    }
    output.label(format!(
        "history: last {}",
        format_span_ns(history.requested_span_ns)
    ));
    if let Some(error) = history.error.as_deref() {
        if let Some(body) = rows.rest() {
            message(
                frame,
                body,
                &format!("history unavailable: {}", clean(error)),
                context.theme.tone_style(CellTone::Conflict),
            );
        }
        return;
    }
    if panels.is_empty() {
        if let Some(body) = rows.rest() {
            message(
                frame,
                body,
                "no series to chart yet",
                context.theme.muted_style(),
            );
        }
        return;
    }
    let window = Window {
        start_ns: history.start_utc_ns,
        end_ns: history.end_utc_ns,
    };
    for (band, members) in layout.bands.iter().enumerate() {
        let Some(area) = rows.take(layout.band_height(band)) else {
            break;
        };
        for (column, &position) in members.iter().enumerate() {
            let x = area.x + layout.panel_offset(saturate_u16(column));
            let panel_area = Rect::new(x, area.y, layout.panel_width, area.height);
            render_panel(
                frame,
                panel_area,
                &panels[position],
                (layout.rows_of_strip(band), window),
                context,
            );
            for strip in &panels[position].strips {
                output.label(format!(
                    "history {}: {}",
                    panels[position].title,
                    strip_text(strip)
                ));
            }
        }
    }
    if let Some(axis) = rows.next_row() {
        // A window reaching back before boot starts at boot.
        let span_ns = window.end_ns - window.start_ns;
        render_axes(frame, axis, &layout, span_ns, context);
    }
    let shown = &blocks[..layout.table_blocks];
    if let Some(table) = rows.take(layout.table_rows) {
        render_gpu_stats(frame, table, shown, context);
    }
    for label in stats_labels(shown) {
        output.label(label);
    }
}

/// The process pages (D-72): the window bar, then the GPU processes of the
/// window as a table to choose from (`tab` 1) or the chosen one's figures
/// and memory chart (`tab` 2). The context's scroll is the cursor.
fn render_process_pages(
    frame: &mut Frame<'_>,
    inner: Rect,
    history: &HistoryView,
    context: &DisplayContext,
    output: &mut Output,
) {
    let mut rows = RowCursor::new(inner);
    if let Some(row) = rows.next_row() {
        let none = Hidden {
            panels: 0,
            gpus: 0,
            processes: 0,
        };
        render_window_bar(frame, row, history, none, context);
    }
    if history.processes.is_empty() {
        if let Some(body) = rows.rest() {
            message(
                frame,
                body,
                "no GPU process was recorded in this window",
                context.theme.muted_style(),
            );
        }
        output.label("history: no GPU process in the window");
        return;
    }
    let cursor = usize::from(context.scroll).min(history.processes.len() - 1);
    for label in process_labels(history) {
        output.label(label);
    }
    if context.tab == 1 {
        if let Some(table) = rows.rest() {
            render_processes(frame, table, history, Some(cursor), context);
        }
        return;
    }
    let process = &history.processes[cursor];
    for line in process_detail_lines(process, history, context) {
        let Some(row) = rows.next_row() else { return };
        draw_line(frame.buffer_mut(), row, &line);
    }
    let Some(chart) = process_chart(process, history) else {
        if let Some(body) = rows.rest() {
            message(
                frame,
                body,
                "no memory chart for this GPU",
                context.theme.muted_style(),
            );
        }
        return;
    };
    let panel = Panel {
        title: format!("GPU memory · {}", clean(&process.gpu_label)),
        strips: vec![Strip {
            label: String::new(),
            chart: &chart,
            scale: process.scale_max,
            paint: Paint::Process(process.slot),
        }],
    };
    let window = Window {
        start_ns: history.start_utc_ns,
        end_ns: history.end_utc_ns,
    };
    let chart_rows = rows.remaining().saturating_sub(1);
    if let Some(area) = rows.take(chart_rows) {
        render_panel(
            frame,
            area,
            &panel,
            (chart_rows.saturating_sub(1), window),
            context,
        );
    }
    if let Some(axis) = rows.next_row() {
        let span_seconds = (window.end_ns - window.start_ns) / 1_000_000_000;
        let x = axis.x + HISTORY_GUTTER_COLUMNS;
        render_axis(
            frame,
            axis,
            (x, plot_width(axis.width)),
            span_seconds,
            context,
        );
    }
}

/// `■ PID 41 │ acme_llm.train │ GPU 0 │ running`, its figures, then its
/// command line as last listed (D-73).
fn process_detail_lines(
    process: &HistoryProcessView,
    history: &HistoryView,
    context: &DisplayContext,
) -> Vec<Line<'static>> {
    let theme = &context.theme;
    let value = |value: Option<f64>| {
        value.map_or_else(
            || crate::format::NOT_AVAILABLE.to_owned(),
            |value| crate::format::format_value(value, process.unit),
        )
    };
    let state = if process.running { "running" } else { "exited" };
    let first = Line::from(vec![
        Span::styled(
            "■ ",
            ratatui::style::Style::new().fg(theme.process_color(process.slot)),
        ),
        Span::styled(format!("PID {}", process.pid), theme.label_style()),
        super::common::separator(context),
        Span::styled(clean(&process.name), theme.title_style()),
        super::common::separator(context),
        Span::styled(clean(&process.gpu_label), theme.label_style()),
        super::common::separator(context),
        Span::styled(state, theme.muted_style()),
    ]);
    let figures = [
        ("avg memory", value(process.mean)),
        ("peak", value(process.peak)),
        ("listed", recorded_text(process.listed_ns / 1_000_000_000)),
        ("window", clean(&history.window_label)),
    ];
    let mut spans = Vec::new();
    for (index, (label, text)) in figures.into_iter().enumerate() {
        if index > 0 {
            spans.push(super::common::separator(context));
        }
        spans.push(Span::styled(format!("{label} "), theme.label_style()));
        spans.push(Span::styled(text, theme.text_style()));
    }
    let command = if process.command.is_empty() {
        crate::format::NOT_AVAILABLE.to_owned()
    } else {
        clean(&process.command)
    };
    let command_line = Line::from(vec![
        Span::styled("command ", theme.label_style()),
        Span::styled(command, theme.text_style()),
    ]);
    vec![first, Line::from(spans), command_line]
}

/// The process's memory chart: its GPU's memory chart (scale, series,
/// legend) with the process's means for points, one per even cell of the
/// window, the grid the means were taken on.
fn process_chart(process: &HistoryProcessView, history: &HistoryView) -> Option<HistoryChartView> {
    let template = history.charts.iter().find(|chart| {
        chart.kind == HistoryChartKind::GpuMemory
            && clean(&chart.subject).starts_with(&clean(&process.gpu_label))
    })?;
    let method = template.points.first()?.method;
    let cells = u128::try_from(process.means.len().max(1)).unwrap_or(1);
    let span = u128::from(history.end_utc_ns.saturating_sub(history.start_utc_ns));
    let edge = |cell: usize| {
        let offset = span * u128::try_from(cell).unwrap_or(u128::MAX) / cells;
        history
            .start_utc_ns
            .saturating_add(u64::try_from(offset).unwrap_or(u64::MAX))
    };
    let mut chart = template.clone();
    chart.points = process
        .means
        .iter()
        .enumerate()
        .map(|(cell, mean)| HistoryPointView {
            start_utc_ns: edge(cell),
            end_utc_ns: edge(cell + 1),
            mean: *mean,
            min: *mean,
            max: *mean,
            method,
        })
        .collect();
    chart.mean = process.mean;
    chart.peak = process.peak;
    chart.parts = Vec::new();
    chart.scale_max = process.scale_max;
    Some(chart)
}

/// A panel's title rule, then its strips, `strip_rows` rows each.
fn render_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    panel: &Panel<'_>,
    (strip_rows, window): (u16, Window),
    context: &DisplayContext,
) {
    let mut rows = RowCursor::new(area);
    if let Some(row) = rows.next_row() {
        let error = panel
            .strips
            .iter()
            .find_map(|strip| strip.chart.error.as_deref());
        draw_line(
            frame.buffer_mut(),
            row,
            &title_rule(&panel.title, error, row.width, context),
        );
    }
    let alone = panel.strips.len() == 1;
    let rate_scale = panel
        .is_rate()
        .then(|| tallest_column(&panel.strips, window, plot_width(area.width)))
        .flatten();
    for strip in &panel.strips {
        let Some(strip_area) = rows.take(strip_rows) else {
            break;
        };
        let strip = Strip {
            scale: rate_scale.or(strip.scale),
            ..strip.clone()
        };
        render_strip(frame, strip_area, (&strip, alone), window, context);
    }
}

/// `GPU power ──────────`: the title, why a query failed, and a rule to
/// the panel's edge.
fn title_rule(
    title: &str,
    error: Option<&str>,
    width: u16,
    context: &DisplayContext,
) -> Line<'static> {
    let theme = &context.theme;
    let mut spans = vec![Span::styled(clean(title), theme.section_title_style(title))];
    if let Some(error) = error {
        spans.push(gap(context));
        spans.push(Span::styled(
            format!("query failed: {}", clean(error)),
            theme.tone_style(CellTone::Conflict),
        ));
    }
    let used: usize = spans.iter().map(Span::width).sum();
    let rule = usize::from(width).saturating_sub(used + 1);
    spans.push(Span::raw(" "));
    spans.push(Span::styled("─".repeat(rule), theme.border_style(false)));
    Line::from(spans)
}

/// The time axis under each column of panels.
fn render_axes(
    frame: &mut Frame<'_>,
    row: Rect,
    layout: &HistoryLayout,
    span_ns: u64,
    context: &DisplayContext,
) {
    let span_seconds = span_ns / 1_000_000_000;
    let columns = layout.bands.iter().map(Vec::len).max().unwrap_or(1);
    for column in 0..saturate_u16(columns) {
        let x = row.x + layout.panel_offset(column) + HISTORY_GUTTER_COLUMNS;
        let width = plot_width(layout.panel_width);
        render_axis(frame, row, (x, width), span_seconds, context);
    }
}

/// What the view could not show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hidden {
    panels: usize,
    gpus: usize,
    processes: usize,
}

impl Hidden {
    /// What did not fit, longest first: `+2 panels · +1 GPU's long-term
    /// statistics · +1 GPU process on a taller terminal`, then shorter
    /// forms, the panels before the statistics before the processes.
    fn notes(self) -> Vec<String> {
        let counted = |count: usize, one: &str, many: &str| match count {
            0 => None,
            1 => Some(format!("+1 {one}")),
            count => Some(format!("+{count} {many}")),
        };
        let parts: Vec<String> = [
            counted(self.panels, "panel", "panels"),
            counted(
                self.gpus,
                "GPU's long-term statistics",
                "GPUs' long-term statistics",
            ),
            counted(self.processes, "GPU process", "GPU processes"),
        ]
        .into_iter()
        .flatten()
        .collect();
        let mut notes = Vec::new();
        if !parts.is_empty() {
            notes.push(format!("{} on a taller terminal", parts.join(" · ")));
        }
        if let Some(first) = parts.first() {
            notes.push(format!("{first} on a taller terminal"));
            notes.push(first.clone());
        }
        notes.dedup();
        notes
    }
}

/// The keys acting on the view, at the right of its title (D-70).
const WINDOW_KEYS: &str = "w/W/←/→ window";
/// The keys of the process history's pages (D-72, D-75).
const PROCESS_LIST_KEYS: &str = "j/k/↑/↓ select · Enter open · w/W/←/→ window";
const PROCESS_DETAIL_KEYS: &str = "Enter back · j/k/↑/↓ next · w/W/←/→ window";

/// `5m [1h] 6h 12h 24h`, then at the right what did not fit
/// and how much of the window is recorded, as much as fits.
fn render_window_bar(
    frame: &mut Frame<'_>,
    row: Rect,
    history: &HistoryView,
    hidden: Hidden,
    context: &DisplayContext,
) {
    let theme = &context.theme;
    let current = clean(&history.window_label);
    let mut spans = Vec::new();
    for window in WINDOWS {
        let style = if window == current {
            theme.selected_row_style()
        } else {
            theme.muted_style()
        };
        spans.push(Span::styled(format!(" {window} "), style));
    }
    let left = Line::from(spans);
    let room = usize::from(row.width).saturating_sub(left.width() + 2 * gap(context).width());
    let note = bar_notes(hidden, recorded_note(history), why_partly(&history.disk))
        .into_iter()
        .find(|note| display_width(note) <= room);
    draw_caption(frame.buffer_mut(), row, &left, note.as_deref(), context);
}

/// Why a window is only partly recorded (D-51): without the archive the
/// history starts with dgxtop; with it, it is recorded while dgxtop runs;
/// an archive that cannot be read is said so.
fn why_partly(disk: &DiskHistory) -> &'static str {
    match disk {
        DiskHistory::Off => "history starts with dgxtop",
        DiskHistory::On => "only while dgxtop runs",
        DiskHistory::Failed(_) => "disk history unavailable",
    }
}

/// The window bar's notes in the order tried: each form of what did not
/// fit (fullest first) with how much of the window is recorded and why,
/// then with the record alone, then alone.
fn bar_notes(hidden: Hidden, recorded: Option<String>, why: &str) -> Vec<String> {
    let why = recorded
        .as_ref()
        .map(|recorded| format!("{recorded} · {why}"));
    let records = [why, recorded, None];
    let mut forms: Vec<Option<String>> = hidden.notes().into_iter().map(Some).collect();
    if forms.is_empty() {
        forms.push(None);
    }
    let mut notes: Vec<String> = Vec::new();
    for form in &forms {
        for record in &records {
            let parts: Vec<String> = form.iter().chain(record.iter()).cloned().collect();
            let note = parts.join(" · ");
            if !parts.is_empty() && !notes.contains(&note) {
                notes.push(note);
            }
        }
    }
    notes
}

/// `recorded 1h 12m of 6h` while the window is only partly recorded.
fn recorded_note(history: &HistoryView) -> Option<String> {
    let recorded = history
        .charts
        .iter()
        .filter_map(|chart| chart.covered_ns)
        .max()?;
    if nanos_to_seconds(recorded) >= nanos_to_seconds(history.requested_span_ns) * FULL_COVERAGE {
        return None;
    }
    Some(format!(
        "recorded {} of {}",
        recorded_text(recorded / 1_000_000_000),
        format_span_ns(history.requested_span_ns)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::history_view;

    fn with_processes() -> HistoryView {
        let mut history = history_view();
        let gib = crate::test_fixtures::GIB;
        let cells = history.charts[0].points.len();
        history.processes = [
            (0_u8, 41, "acme_llm.train", 40.0, true),
            (3, 42, "ComfyUI/main", 9.0, false),
        ]
        .into_iter()
        .map(|(slot, pid, name, mean, running)| HistoryProcessView {
            slot,
            pid,
            name: name.to_owned(),
            command: format!("python -m {name} --epochs 3"),
            gpu_label: "GPU 0".to_owned(),
            unit: dgxtop_core::metric::CanonicalUnit::Bytes,
            mean: Some(mean * gib),
            peak: Some(mean * gib * 1.1),
            listed_ns: 30 * 60 * SECOND,
            running,
            means: vec![Some(mean * gib); cells],
            scale_max: Some(64.0 * gib),
        })
        .collect();
        history
    }

    fn draw(history: &HistoryView, tab: u8, scroll: u16) -> Vec<String> {
        let mut context = crate::test_fixtures::display_context();
        context.tab = tab;
        context.scroll = scroll;
        let mut model =
            crate::test_fixtures::view_model(dgxtop_core::commands::ViewKind::History, 1);
        model.modules.history = Some(std::sync::Arc::new(history.clone()));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| {
                HistoryModule::new()
                    .unwrap()
                    .render(&context, &model, frame.area(), frame);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..24)
            .map(|y| (0..120).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    /// The Process History tab (D-72, D-75): first the table to choose from,
    /// then the chosen process's figures and its memory chart in its color.
    #[test]
    fn the_process_pages_list_then_open_a_process() {
        let history = with_processes();
        let list = draw(&history, 1, 1);
        assert!(list[0].contains("Process History"), "{}", list.join("\n"));
        assert!(list[0].contains("Enter open"), "{}", list[0]);
        let selected = list
            .iter()
            .position(|line| line.contains("ComfyUI/main"))
            .unwrap();
        assert!(list.iter().any(|line| line.contains("acme_llm.train")));
        assert!(selected > 0);
        let detail = draw(&history, 2, 1);
        let text = detail.join("\n");
        assert!(detail[0].contains("Process History · process"), "{text}");
        assert!(
            text.contains("PID 42 │ ComfyUI/main │ GPU 0 │ exited"),
            "{text}"
        );
        assert!(text.contains("avg memory 9.0 GiB"), "{text}");
        assert!(
            text.contains("command python -m ComfyUI/main --epochs 3"),
            "{text}"
        );
        assert!(text.contains("GPU memory · GPU 0"), "{text}");
        let chart = process_chart(&history.processes[1], &history).unwrap();
        assert_eq!(chart.points.len(), history.processes[1].means.len());
        assert!(chart.points.iter().all(|point| point.mean.is_some()));
        assert_eq!(
            chart.points.first().unwrap().start_utc_ns,
            history.start_utc_ns
        );
        assert_eq!(chart.points.last().unwrap().end_utc_ns, history.end_utc_ns);
        // An offset past the list opens the last process.
        assert!(draw(&history, 2, 9).join("\n").contains("PID 42"));
        assert!(draw(&history, 2, 0).join("\n").contains("PID 41"));
        // No process in the window: it says so.
        let empty = draw(&history_view(), 1, 0).join("\n");
        assert!(
            empty.contains("no GPU process was recorded in this window"),
            "{empty}"
        );
    }

    #[test]
    fn a_partly_recorded_window_says_how_much_it_holds() {
        let mut history = history_view();
        for chart in &mut history.charts {
            chart.coverage_ratio = Some(0.2);
            chart.covered_ns = Some(720 * SECOND);
        }
        assert_eq!(
            recorded_note(&history).as_deref(),
            Some("recorded 12m of 1h")
        );
        // The 24 h window on a machine up five hours: its ratio is of the
        // five hours, the note still of the time recorded.
        history.requested_span_ns = 24 * 3_600 * SECOND;
        for chart in &mut history.charts {
            chart.coverage_ratio = Some(720.0 / (5.0 * 3_600.0));
        }
        assert_eq!(
            recorded_note(&history).as_deref(),
            Some("recorded 12m of 24h")
        );
        history.requested_span_ns = 3_600 * SECOND;
        for chart in &mut history.charts {
            chart.coverage_ratio = Some(1.0);
            chart.covered_ns = Some(3_600 * SECOND);
        }
        assert_eq!(recorded_note(&history), None);
    }

    const SECOND: u64 = 1_000_000_000;

    #[test]
    fn a_window_kept_on_disk_says_it_is_recorded_while_dgxtop_runs() {
        // D-51: with the archive the gaps are the times dgxtop was not
        // running; an archive that cannot be read is said so.
        assert_eq!(why_partly(&DiskHistory::On), "only while dgxtop runs");
        assert_eq!(
            why_partly(&DiskHistory::Failed("locked".to_owned())),
            "disk history unavailable"
        );
        let recorded = Some("recorded 5h 12m of 24h".to_owned());
        assert_eq!(
            bar_notes(
                Hidden {
                    panels: 0,
                    gpus: 0,
                    processes: 0
                },
                recorded,
                why_partly(&DiskHistory::On)
            )[0],
            "recorded 5h 12m of 24h · only while dgxtop runs"
        );
    }
}
