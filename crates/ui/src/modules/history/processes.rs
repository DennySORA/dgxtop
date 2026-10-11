//! The tracked GPU processes of the history view (D-64): per process and
//! GPU, the mean and peak of the memory it held over the window and how
//! long it was listed, each behind a `■` in the color its part of the
//! memory charts has. A process that has exited is dimmed and says so.
//! Recorded while dgxtop runs.

use dgxtop_core::view::{HistoryProcessView, HistoryView};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::stats::recorded_text;
use crate::format::{NOT_AVAILABLE, clean, display_width, format_value};
use crate::modules::common::{
    ColumnSpec, RowCursor, TableCell, draw_table_header, draw_table_row, fit_columns,
};
use crate::num::saturate_u16;
use crate::registry::DisplayContext;

const SWATCH: &str = "■";
const EXITED: &str = "exited";

/// A column: title, alignment, priority (the state gives way first), and
/// the space it keeps after its widest value.
#[derive(Debug, Clone, Copy)]
struct Column {
    title: &'static str,
    right: bool,
    priority: u8,
    space: u16,
}

impl Column {
    const fn left(title: &'static str, priority: u8, space: u16) -> Self {
        Self {
            title,
            right: false,
            priority,
            space,
        }
    }

    const fn right(title: &'static str, priority: u8, space: u16) -> Self {
        Self {
            title,
            right: true,
            priority,
            space,
        }
    }
}

/// The swatch beside the PID, then the name, the GPU and the figures.
const COLUMNS: [Column; 8] = [
    Column::left("", 255, 0),
    Column::right("PID", 250, 0),
    Column::left("process", 240, 1),
    Column::left("GPU", 100, 1),
    Column::right("avg memory", 230, 1),
    Column::right("peak", 90, 1),
    Column::right("listed", 80, 1),
    Column::left("", 40, 0),
];

/// Rows the table takes with its header; none without a process.
pub(super) fn table_rows(history: &HistoryView) -> u16 {
    match history.processes.len() {
        0 => 0,
        rows => saturate_u16(rows).saturating_add(1),
    }
}

/// Draws the header and as many processes as `area` has rows for, at the
/// table's natural width.
pub(super) fn render_processes(
    frame: &mut Frame<'_>,
    area: Rect,
    history: &HistoryView,
    context: &DisplayContext,
) {
    let rows: Vec<Vec<TableCell>> = history
        .processes
        .iter()
        .map(|process| cells(process, context))
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
            spec.prefer(widest.saturating_add(column.space))
        })
        .collect();
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
    for cells in rows {
        let Some(row) = cursor.next_row() else { break };
        draw_table_row(frame.buffer_mut(), row, &fitted, &cells, None);
    }
}

/// `■ 277055 python  GPU 0  61.8 GiB  69.8 GiB  46m`, dimmed with
/// `exited` once the process is gone.
fn cells(process: &HistoryProcessView, context: &DisplayContext) -> Vec<TableCell> {
    let theme = &context.theme;
    let (text, label) = if process.running {
        (theme.text_style(), theme.label_style())
    } else {
        (theme.muted_style(), theme.muted_style())
    };
    let value = |value: Option<f64>| {
        value.map_or_else(
            || NOT_AVAILABLE.to_owned(),
            |value| format_value(value, process.unit),
        )
    };
    vec![
        TableCell::new(SWATCH, Style::new().fg(theme.process_color(process.slot))),
        TableCell::new(process.pid.to_string(), label),
        TableCell::new(clean(&process.name), text),
        TableCell::new(clean(&process.gpu_label), label),
        TableCell::new(value(process.mean), text),
        TableCell::new(value(process.peak), text),
        TableCell::new(recorded_text(process.listed_ns / 1_000_000_000), label),
        TableCell::new(
            if process.running { "" } else { EXITED },
            theme.muted_style(),
        ),
    ]
}

/// Accessibility text of each process: `history process pid 277055
/// python on GPU 0: avg 61.8 GiB peak 69.8 GiB listed 46m`.
pub(super) fn process_labels(history: &HistoryView) -> Vec<String> {
    history
        .processes
        .iter()
        .map(|process| {
            let value = |value: Option<f64>| {
                value.map_or_else(
                    || NOT_AVAILABLE.to_owned(),
                    |value| format_value(value, process.unit),
                )
            };
            format!(
                "history process pid {} {} on {}: avg {} peak {} listed {}{}",
                process.pid,
                clean(&process.name),
                clean(&process.gpu_label),
                value(process.mean),
                value(process.peak),
                recorded_text(process.listed_ns / 1_000_000_000),
                if process.running { "" } else { ", exited" }
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{GIB, display_context, history_view};
    use dgxtop_core::metric::CanonicalUnit;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn process(slot: u8, pid: i32, name: &str, mean_gib: f64, running: bool) -> HistoryProcessView {
        HistoryProcessView {
            slot,
            pid,
            name: name.to_owned(),
            command: String::new(),
            gpu_label: "GPU 0".to_owned(),
            unit: CanonicalUnit::Bytes,
            mean: Some(mean_gib * GIB),
            peak: Some(mean_gib * GIB * 1.25),
            listed_ns: 46 * 60 * 1_000_000_000,
            running,
        }
    }

    #[test]
    fn each_process_is_a_row_in_its_color_and_an_exited_one_says_so() {
        let context = display_context();
        let mut history = history_view();
        history.processes = vec![
            process(1, 277_055, "python", 61.8, true),
            process(0, 281_234, "vllm", 19.9, false),
        ];
        assert_eq!(table_rows(&history), 3);
        let mut terminal = Terminal::new(TestBackend::new(90, 3)).unwrap();
        terminal
            .draw(|frame| render_processes(frame, frame.area(), &history, &context))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let line = |y: u16| -> String { (0..90).map(|x| buffer[(x, y)].symbol()).collect() };
        assert!(
            line(0).contains("PID") && line(0).contains("avg memory"),
            "{}",
            line(0)
        );
        let first = line(1);
        let peak = format_value(61.8 * GIB * 1.25, CanonicalUnit::Bytes);
        assert!(first.starts_with("■ 277055 python"), "{first}");
        assert!(first.contains("GPU 0"), "{first}");
        assert!(
            first.contains("61.8 GiB") && first.contains(&peak),
            "{first}"
        );
        assert!(first.trim_end().ends_with("46m"), "{first}");
        assert!(line(2).trim_end().ends_with(EXITED), "{}", line(2));
        assert_eq!(buffer[(0, 1)].fg, context.theme.process_color(1));
        assert_eq!(buffer[(0, 2)].fg, context.theme.process_color(0));
        let labels = process_labels(&history);
        assert_eq!(
            labels[0],
            format!(
                "history process pid 277055 python on GPU 0: avg 61.8 GiB peak {peak} listed 46m"
            )
        );
        assert!(labels[1].ends_with(", exited"));
        history.processes.clear();
        assert_eq!(table_rows(&history), 0);
    }
}
