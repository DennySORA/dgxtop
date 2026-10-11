//! The GPUs' long-term table of the history view (D-48, D-50): for each GPU
//! one row per window (the last 1 h, 6 h, 12 h and 24 h) with the mean and
//! peak of its utilization, memory in use, temperature and power and the
//! energy it drew. History starts with dgxtop, so a window that holds no
//! more than the window before it is left out instead of repeating its
//! numbers, and a window only partly recorded says how much it holds.

use dgxtop_core::view::{HistoryChartKind, HistoryWindowSummary};
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::format::{NOT_AVAILABLE, clean, display_width, format_energy, format_value};
use crate::modules::common::{
    ColumnSpec, FittedColumn, RowCursor, TableCell, draw_table_header, draw_table_row, fit_columns,
};
use crate::num::saturate_u16;
use crate::registry::DisplayContext;

/// Text of a value the history has not answered yet.
const PENDING: &str = "…";
/// More recorded time a window must hold than the one before it to be
/// listed.
const MIN_ADDED_SECONDS: f64 = 60.0;
/// Share of a window below which it says how much it holds.
const FULL_COVERAGE: f64 = 0.99;

/// The statistics of one GPU.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct GpuStats<'a> {
    /// `GPU 0 · NVIDIA GB10`.
    pub(super) subject: &'a str,
    windows: Vec<&'a str>,
    summaries: Vec<&'a HistoryWindowSummary>,
}

impl GpuStats<'_> {
    /// Rows of this GPU in the table: one per window.
    pub(super) fn rows(&self) -> u16 {
        saturate_u16(self.windows.len())
    }

    fn summary(&self, window: &str, kind: HistoryChartKind) -> Option<&HistoryWindowSummary> {
        self.summaries
            .iter()
            .copied()
            .find(|summary| summary.window_label == window && summary.kind == kind)
    }

    /// Seconds of `window` its best covered series holds; `None` while
    /// pending.
    fn recorded_seconds(&self, window: &str) -> Option<f64> {
        let in_window = || {
            self.summaries
                .iter()
                .filter(|summary| summary.window_label == window)
        };
        if in_window().any(|summary| summary.pending) {
            return None;
        }
        in_window()
            .filter_map(|summary| summary.covered_ns.map(nanos_to_seconds))
            .reduce(f64::max)
    }

    /// Seconds `window` spans, as the Manager gives it (D-99).
    fn span_seconds(&self, window: &str) -> Option<f64> {
        self.summaries
            .iter()
            .find(|summary| summary.window_label == window)
            .map(|summary| nanos_to_seconds(summary.span_ns))
    }
}

/// The summaries grouped by GPU, in the order they are listed, each with
/// the windows that hold more than the window before them.
pub(super) fn gpu_stats(summaries: &[HistoryWindowSummary]) -> Vec<GpuStats<'_>> {
    let mut blocks: Vec<GpuStats<'_>> = Vec::new();
    for summary in summaries {
        let position = blocks
            .iter()
            .position(|block| block.subject == summary.subject)
            .unwrap_or_else(|| {
                blocks.push(GpuStats {
                    subject: &summary.subject,
                    windows: Vec::new(),
                    summaries: Vec::new(),
                });
                blocks.len() - 1
            });
        let block = &mut blocks[position];
        if !block.windows.contains(&summary.window_label.as_str()) {
            block.windows.push(&summary.window_label);
        }
        block.summaries.push(summary);
    }
    for block in &mut blocks {
        block.windows = adding_windows(block);
    }
    blocks
}

/// The windows of `block` that add recorded time: the first always, a
/// later one when it holds a minute more than the last listed or has not
/// answered yet.
fn adding_windows<'a>(block: &GpuStats<'a>) -> Vec<&'a str> {
    let mut listed = Vec::new();
    let mut recorded = None;
    for window in &block.windows {
        let held = block.recorded_seconds(window);
        let adds = match (recorded, held) {
            (None, _) | (_, None) => true,
            (Some(before), Some(held)) => held >= before + MIN_ADDED_SECONDS,
        };
        if adds {
            listed.push(*window);
            if held.is_some() {
                recorded = held;
            }
        }
    }
    listed
}

/// Seconds of `nanos`.
pub(super) fn nanos_to_seconds(nanos: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let nanos = nanos as f64;
    nanos / 1e9
}

/// A column of the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    Gpu,
    Window,
    Series(HistoryChartKind),
    Energy,
}

/// The GPU series, in the order of the overview's GPU readings.
const SERIES: [HistoryChartKind; 4] = [
    HistoryChartKind::GpuUtilization,
    HistoryChartKind::GpuMemory,
    HistoryChartKind::GpuTemperature,
    HistoryChartKind::GpuPower,
];

/// The GPU and window, the series some GPU reports, then the energy when
/// a GPU reports power.
fn columns(blocks: &[GpuStats<'_>]) -> Vec<Column> {
    let reported = |kind| {
        blocks
            .iter()
            .flat_map(|block| &block.summaries)
            .any(|summary| summary.kind == kind)
    };
    let mut columns = vec![Column::Gpu, Column::Window];
    columns.extend(
        SERIES
            .into_iter()
            .filter(|kind| reported(*kind))
            .map(Column::Series),
    );
    if reported(HistoryChartKind::GpuPower) {
        columns.push(Column::Energy);
    }
    columns
}

const fn title(column: Column) -> &'static str {
    match column {
        Column::Gpu => "GPU",
        Column::Window => "avg / peak",
        Column::Series(HistoryChartKind::GpuMemory) => "memory",
        Column::Series(HistoryChartKind::GpuTemperature) => "temperature",
        Column::Series(HistoryChartKind::GpuPower) => "power",
        Column::Series(_) => "utilization",
        Column::Energy => "energy",
    }
}

/// Which columns give way first when the panel is narrow; the energy is
/// optional (below 50): it leaves before the series crowd.
const fn priority(column: Column) -> u8 {
    match column {
        Column::Window => 255,
        Column::Gpu => 250,
        Column::Series(HistoryChartKind::GpuTemperature) => 95,
        Column::Series(HistoryChartKind::GpuPower) => 92,
        Column::Series(HistoryChartKind::GpuMemory) => 90,
        Column::Series(_) => 100,
        Column::Energy => 45,
    }
}

/// `GPU 0` of `GPU 0 · NVIDIA GB10`: what a narrow GPU column shows.
fn short_subject(subject: &str) -> &str {
    subject.split(" · ").next().unwrap_or(subject)
}

/// Draws the table of `blocks` into `area` at its natural width.
pub(super) fn render_gpu_stats(
    frame: &mut Frame<'_>,
    area: Rect,
    blocks: &[GpuStats<'_>],
    context: &DisplayContext,
) {
    let columns = columns(blocks);
    let rows: Vec<(usize, &str, Vec<TableCell>)> = blocks
        .iter()
        .enumerate()
        .flat_map(|(position, block)| block.windows.iter().map(move |window| (position, *window)))
        .map(|(position, window)| {
            let block = &blocks[position];
            let cells = columns
                .iter()
                .map(|column| value_cell(block, window, *column, context))
                .collect();
            (position, window, cells)
        })
        .collect();
    let specs: Vec<ColumnSpec> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let widest = match column {
                Column::Gpu => blocks
                    .iter()
                    .map(|block| display_width(&clean(block.subject)))
                    .max()
                    .unwrap_or(0),
                _ => rows
                    .iter()
                    .filter_map(|(_, _, cells)| cells.get(index))
                    .map(|cell| display_width(&cell.text))
                    .max()
                    .unwrap_or(0),
            }
            .max(display_width(title(*column)));
            let narrowest = match column {
                Column::Gpu => blocks
                    .iter()
                    .map(|block| display_width(short_subject(&clean(block.subject))))
                    .max()
                    .unwrap_or(0)
                    .max(display_width(title(*column))),
                _ => widest,
            };
            let spec = if matches!(column, Column::Gpu | Column::Window) {
                ColumnSpec::left(title(*column), 0, priority(*column))
            } else {
                ColumnSpec::right(title(*column), 0, priority(*column))
            };
            ColumnSpec {
                min_width: saturate_u16(narrowest),
                preferred: saturate_u16(widest + 2),
                // The GPU's full name takes width only after every series
                // has its gap.
                flex: *column == Column::Gpu,
                ..spec
            }
        })
        .collect();
    // The table keeps its natural width: fitting it to the panel would hand
    // the spare columns to the last one and push it to the far edge.
    let natural = specs
        .iter()
        .fold(0u16, |width, spec| width.saturating_add(spec.preferred))
        .saturating_add(saturate_u16(specs.len().saturating_sub(1)));
    let area = Rect {
        width: area.width.min(natural),
        ..area
    };
    let fitted = fit_columns(&specs, area.width);
    let mut cursor = RowCursor::new(area);
    if let Some(header) = cursor.next_row() {
        draw_table_header(
            frame.buffer_mut(),
            header,
            &specs,
            &fitted,
            context.theme.header_style(),
        );
    }
    let gpu_width = gpu_column_width(&fitted, &columns);
    let mut previous = None;
    for (position, _, mut cells) in rows {
        let Some(row) = cursor.next_row() else { break };
        // The GPU names its block on the block's first row only.
        let subject = clean(blocks[position].subject);
        let fits = display_width(&subject) <= usize::from(gpu_width);
        cells[0] = if previous == Some(position) {
            TableCell::new("", context.theme.text_style())
        } else if fits {
            TableCell::new(subject, context.theme.title_style())
        } else {
            TableCell::new(short_subject(&subject), context.theme.title_style())
        };
        previous = Some(position);
        draw_table_row(frame.buffer_mut(), row, &fitted, &cells, None);
    }
}

/// Width the fitted GPU column got.
fn gpu_column_width(fitted: &[FittedColumn], columns: &[Column]) -> u16 {
    fitted
        .iter()
        .find(|column| columns.get(column.spec) == Some(&Column::Gpu))
        .map_or(0, |column| column.width)
}

/// Accessibility text of each GPU's windows: `history GPU 0 · NVIDIA GB10
/// last 1h: utilization 12.0% / 90.0%, memory …, energy 80.0 Wh`.
pub(super) fn stats_labels(blocks: &[GpuStats<'_>]) -> Vec<String> {
    let mut labels = Vec::new();
    for block in blocks {
        for window in &block.windows {
            let mut parts: Vec<String> = SERIES
                .into_iter()
                .filter_map(|kind| {
                    let summary = block.summary(window, kind)?;
                    Some(format!(
                        "{} {}",
                        title(Column::Series(kind)),
                        series_text(summary)
                    ))
                })
                .collect();
            if let Some(power) = block.summary(window, HistoryChartKind::GpuPower) {
                parts.push(format!("energy {}", energy_text(power)));
            }
            labels.push(format!(
                "history {} last {window}: {}",
                clean(block.subject),
                parts.join(", ")
            ));
        }
    }
    labels
}

/// One cell of a GPU's window: `last 1h`, `12.0% / 90.0%`, `80.0 Wh`,
/// `100%`, `…` while the history is answering, `n/a` without data. The
/// GPU cell is filled once the column width is known.
fn value_cell(
    block: &GpuStats<'_>,
    window: &str,
    column: Column,
    context: &DisplayContext,
) -> TableCell {
    let theme = &context.theme;
    let text = match column {
        Column::Gpu => return TableCell::new("", theme.text_style()),
        Column::Window => return TableCell::new(window_text(block, window), theme.label_style()),
        Column::Series(kind) => block.summary(window, kind).map(series_text),
        Column::Energy => block
            .summary(window, HistoryChartKind::GpuPower)
            .map(energy_text),
    }
    .unwrap_or_else(|| NOT_AVAILABLE.to_owned());
    let style = if text == PENDING || text == NOT_AVAILABLE {
        theme.muted_style()
    } else {
        theme.text_style()
    };
    TableCell::new(text, style)
}

/// `12.0% / 90.0%`: the mean and the peak over the window.
fn series_text(summary: &HistoryWindowSummary) -> String {
    if summary.pending {
        return PENDING.to_owned();
    }
    let format = |value: Option<f64>| {
        value.map_or_else(
            || NOT_AVAILABLE.to_owned(),
            |value| format_value(value, summary.unit),
        )
    };
    match (summary.mean, summary.peak) {
        (None, None) => NOT_AVAILABLE.to_owned(),
        (mean, peak) => format!("{} / {}", format(mean), format(peak)),
    }
}

/// The energy the GPU drew over the covered part of the window.
fn energy_text(summary: &HistoryWindowSummary) -> String {
    if summary.pending {
        return PENDING.to_owned();
    }
    summary
        .integral
        .map_or_else(|| NOT_AVAILABLE.to_owned(), format_energy)
}

/// `last 6h`, or for a window only partly recorded `last 6h (1h 12m)`.
fn window_text(block: &GpuStats<'_>, window: &str) -> String {
    let label = format!("last {window}");
    match (block.recorded_seconds(window), block.span_seconds(window)) {
        (Some(held), Some(span)) if held < span * FULL_COVERAGE => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let held = held.max(0.0).round() as u64;
            format!("{label} ({})", recorded_text(held))
        }
        _ => label,
    }
}

/// `1h 12m`, `45m`, `2h`, `30s`: recorded time, in whole minutes past the
/// first.
pub(super) fn recorded_text(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    match (minutes / 60, minutes % 60) {
        (0, minutes) => format!("{minutes}m"),
        (hours, 0) => format!("{hours}h"),
        (hours, minutes) => format!("{hours}h {minutes}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{display_context, gpu_window_stats};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Rows a table of `blocks` takes: its header and their windows.
    fn table_rows(blocks: &[GpuStats<'_>]) -> u16 {
        if blocks.is_empty() {
            return 0;
        }
        blocks
            .iter()
            .fold(1u16, |rows, block| rows.saturating_add(block.rows()))
    }

    /// Sets each window's recorded seconds, `held(window seconds)`.
    fn record(summaries: &mut [HistoryWindowSummary], held: impl Fn(f64) -> f64) {
        for summary in summaries {
            let seconds = held(nanos_to_seconds(summary.span_ns));
            summary.covered_ns = Some(format!("{:.0}", seconds * 1e9).parse().unwrap());
        }
    }

    fn draw(summaries: &[HistoryWindowSummary], width: u16) -> Vec<String> {
        let blocks = gpu_stats(summaries);
        let rows = table_rows(&blocks);
        let mut terminal = Terminal::new(TestBackend::new(width, rows)).unwrap();
        terminal
            .draw(|frame| {
                render_gpu_stats(frame, frame.area(), &blocks, &display_context());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..rows)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn a_short_history_lists_one_window_and_says_how_much_a_longer_holds() {
        // dgxtop ran 1 h 12 m: the last hour is full, the 6 h window holds
        // 1 h 12 m, the 12 h and 24 h windows nothing more.
        let mut summaries = gpu_window_stats();
        record(&mut summaries, |window| window.min(4_320.0));
        let blocks = gpu_stats(&summaries);
        assert_eq!(blocks[0].windows, vec!["1h", "6h"]);
        let rows = draw(&summaries, 140);
        assert!(rows[2].contains("last 6h (1h 12m)"), "{rows:#?}");
        assert!(
            !rows.iter().any(|row| row.contains("last 12h")),
            "{rows:#?}"
        );
        // Only the first hour recorded, partly: one window, which says so.
        record(&mut summaries, |_| 1_800.0);
        let blocks = gpu_stats(&summaries);
        assert_eq!(blocks[0].windows, vec!["1h"]);
        let rows = draw(&summaries, 140);
        assert!(rows[1].contains("last 1h (30m)"), "{rows:#?}");
        // A machine up five hours on which dgxtop recorded 15 minutes: each
        // window holds the same 15 minutes, so only the first is listed.
        record(&mut summaries, |_| 900.0);
        let blocks = gpu_stats(&summaries);
        assert_eq!(blocks[0].windows, vec!["1h"], "{:#?}", blocks[0].windows);
        let rows = draw(&summaries, 140);
        assert!(rows[1].contains("last 1h (15m)"), "{rows:#?}");
        assert_eq!(recorded_text(45), "45s");
        assert_eq!(recorded_text(7_200), "2h");
    }

    #[test]
    fn pending_and_empty_windows_say_so() {
        let mut summaries = gpu_window_stats();
        for summary in &mut summaries {
            match summary.window_label.as_str() {
                "6h" => summary.pending = true,
                "12h" => {
                    summary.mean = None;
                    summary.peak = None;
                    summary.integral = None;
                }
                _ => {}
            }
        }
        let rows = draw(&summaries, 140);
        assert!(
            rows[2].contains("…") && !rows[2].contains("n/a"),
            "{rows:#?}"
        );
        assert!(rows[3].contains("n/a"), "{rows:#?}");
    }

    #[test]
    fn a_series_no_gpu_reports_has_no_column() {
        let summaries: Vec<HistoryWindowSummary> = gpu_window_stats()
            .into_iter()
            .filter(|summary| summary.kind != HistoryChartKind::GpuPower)
            .collect();
        let rows = draw(&summaries, 140);
        assert!(!rows[0].contains("power"), "{rows:#?}");
        assert!(!rows[0].contains("energy"), "{rows:#?}");
    }
}
