//! Processes module: the projection's visible window of the process table.
//!
//! Columns (D-66): GPU, PID, USER, TYPE (compute `C`, graphics `G` or both,
//! as `nvidia-smi` prints them, D-54), CPU%, GPU util, GPU memory, CPU
//! memory (the resident set), NAME, set apart by `│` and as wide as the
//! drawn rows need. A tracked process's name follows a `■` in its color
//! (D-64), so the table names the colors of the memory bars and charts.
//! Rows are identified by `ProcessKey`; the selected row is marked with `▶`
//! (not only by color) and published as the only `K` hit target.

use dgxtop_core::canonical::GpuUsageKind;
use dgxtop_core::identity::EntityId;
use dgxtop_core::view::{ProcessGpuView, ProcessRowView, ProcessesView, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;

use super::ModuleBuildError;
use super::common::{
    ColumnSpec, FittedColumn, ManifestSpec, Output, RowCursor, TableCell, build_manifest,
    draw_column_separators, draw_table_header, draw_table_row, fit_separated_columns, message,
    no_data, panel, panel_with_keys,
};
use crate::format::{
    NOT_AVAILABLE, clean, display_width, format_optional_bytes, format_optional_percent,
    truncate_to_width,
};
use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, OverviewCell, RenderOutput, Slot,
    controls,
};

const SELECTION_MARKER: &str = "▶";
/// The keys acting on this table, at the right of its title (D-70).
const PROCESS_KEYS: &str =
    "j/k/↑/↓ select · s sort · r reverse · g GPU filter · / filter · K terminate · F9 kill";
/// A tracked process's color, before its name (D-64).
const COLOR_SWATCH: &str = "■";
/// Position of the swatch column in [`COLUMNS`].
const SWATCH_COLUMN: usize = 9;

/// The GPU first (D-66), then identity, type and CPU use, GPU use, the
/// memory on the CPU side and the name. Minimum widths fit one GPU's
/// values; [`fitted_columns`] widens a column to what the rows show.
const COLUMNS: [ColumnSpec; 11] = [
    ColumnSpec::left("", 1, 255),
    ColumnSpec::left("GPU", 3, 100).attached(),
    ColumnSpec::right("PID", 7, 98),
    ColumnSpec::left("USER", 4, 40),
    ColumnSpec::left("TYPE", 4, 45),
    ColumnSpec::right("CPU%", 5, 80),
    ColumnSpec::right("GPU%", 5, 85),
    ColumnSpec::right("GPU MEM", 8, 70),
    ColumnSpec::right("CPU MEM", 8, 60),
    ColumnSpec::left("", 1, 97),
    ColumnSpec::left("NAME", 10, 96).flexible().attached(),
];

pub struct ProcessesModule {
    manifest: DisplayModuleManifest,
}

impl ProcessesModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: "processes",
                title: "Processes (GPU first)",
                min_columns: 40,
                min_rows: 5,
                priority: 75,
                slots: &[Slot::Overview],
            })?,
        })
    }
}

impl DisplayModule for ProcessesModule {
    fn manifest(&self) -> &DisplayModuleManifest {
        &self.manifest
    }

    fn overview_cell(&self) -> Option<OverviewCell> {
        Some(OverviewCell::at(3, 0, 100).filling())
    }

    /// Enough rows for every GPU process (borders, column header, rows and
    /// the status line), so a short list leaves its rows to the GPU panel.
    fn preferred_rows(&self, model: &ViewModel, _width: u16, _slot: Slot) -> Option<u16> {
        let rows = model.modules.processes.as_deref().map_or(1, |view| {
            usize::try_from(view.total_rows).unwrap_or(usize::MAX)
        });
        // Borders, header, its rule, the status line and the rows a blank
        // row apart (D-72).
        let rows = saturate_u16(rows.max(1));
        Some(rows.saturating_mul(2).saturating_sub(1).saturating_add(5))
    }

    fn render(
        &self,
        context: &DisplayContext,
        model: &ViewModel,
        area: Rect,
        frame: &mut Frame<'_>,
    ) -> RenderOutput {
        let mut output = Output::default();
        let Some(view) = model.modules.processes.as_deref() else {
            let inner = panel(frame, area, "Processes", context);
            message(
                frame,
                inner,
                &no_data("processes"),
                context.theme.muted_style(),
            );
            output.label("processes: no data");
            return output.finish();
        };
        let inner = panel_with_keys(frame, area, &title_text(view), PROCESS_KEYS, context);
        output.label(format!(
            "processes: {} rows, sort {} {}",
            view.total_rows,
            view.sort.label(),
            if view.ascending {
                "ascending"
            } else {
                "descending"
            }
        ));
        if inner.is_empty() {
            return output.finish();
        }
        if view.rows.is_empty() {
            message(frame, inner, &empty_text(view), context.theme.muted_style());
            output.label(empty_text(view));
            return output.finish();
        }
        let table = TableLayout::new(inner);
        // A blank row between two processes while all of them fit so, else
        // one row each (D-72).
        let spaced = view.rows.len().saturating_mul(2).saturating_sub(1)
            <= usize::from(table.body.height)
            && view.rows.len() > 1;
        let capacity = if spaced {
            table.body.height.saturating_add(1) / 2
        } else {
            table.body.height
        };
        let window = DisplayWindow::new(view, capacity);
        let filter = view.gpu_filter.as_ref().map(|(id, _)| id);
        let drawn: Vec<(&ProcessRowView, bool, [TableCell; 11])> = view
            .rows
            .iter()
            .skip(window.offset)
            .take(window.count)
            .map(|row| {
                let selected = view.selected.as_ref() == Some(&row.key);
                (row, selected, row_cells(row, selected, filter, context))
            })
            .collect();
        let specs = fitted_columns(drawn.iter().map(|(_, _, cells)| cells));
        let columns = fit_separated_columns(&specs, table.body.width.saturating_sub(1));
        let buffer = frame.buffer_mut();
        draw_table_header(
            buffer,
            table.header,
            &specs,
            &columns,
            context.theme.header_style(),
        );
        draw_column_separators(buffer, table.header, &columns, context.theme.muted_style());
        if let Some(rule) = table.rule {
            draw_header_rule(buffer, rule, &columns, context);
        }
        let mut cursor = RowCursor::new(table.body);
        for (index, (row, selected, cells)) in drawn.into_iter().enumerate() {
            if spaced && index > 0 {
                cursor.next_row();
            }
            let Some(rect) = cursor.next_row() else { break };
            let style = selected.then(|| context.theme.selected_row_style());
            draw_table_row(frame.buffer_mut(), rect, &columns, &cells, style);
            let separator = style.map_or(context.theme.muted_style(), |row| {
                context.theme.muted_style().patch(row)
            });
            draw_column_separators(frame.buffer_mut(), rect, &columns, separator);
            if selected {
                keep_swatch_color(frame, rect, &columns, &cells[SWATCH_COLUMN]);
            }
            let control = if selected {
                controls::PROCESS_ROW_SELECTED
            } else {
                controls::PROCESS_ROW
            };
            output.process_target(model, &self.manifest, control, rect, row.key.clone());
            output.label(row_label(row, selected, filter));
        }
        draw_scrollbar(frame, table.body, view, &window, context);
        if let Some(status) = table.status {
            let text = status_text(view, &window, filter);
            frame.buffer_mut().set_stringn(
                status.x,
                status.y,
                truncate_to_width(&text, usize::from(status.width)),
                usize::from(status.width),
                context.theme.label_style(),
            );
        }
        output.finish()
    }
}

/// Header, the rule under it, body and optional status row of the table
/// area.
struct TableLayout {
    header: Rect,
    rule: Option<Rect>,
    body: Rect,
    status: Option<Rect>,
}

impl TableLayout {
    fn new(inner: Rect) -> Self {
        let mut rows = RowCursor::new(inner);
        let header = rows.next_row().unwrap_or_default();
        // The rule needs a row besides the header, a process and the status.
        let rule = (inner.height >= 4).then(|| rows.next_row()).flatten();
        let status_rows = u16::from(inner.height >= 4);
        let body = rows
            .take(rows.remaining().saturating_sub(status_rows))
            .unwrap_or_default();
        let status = rows.next_row();
        Self {
            header,
            rule,
            body,
            status,
        }
    }
}

/// A gray `────┼────` under the header that sets it apart from the rows,
/// crossing each column separator (D-72).
fn draw_header_rule(
    buffer: &mut ratatui::buffer::Buffer,
    rule: Rect,
    columns: &[FittedColumn],
    context: &DisplayContext,
) {
    let style = context.theme.muted_style();
    buffer.set_stringn(
        rule.x,
        rule.y,
        "─".repeat(usize::from(rule.width)),
        usize::from(rule.width),
        style,
    );
    for column in columns.iter().filter(|column| column.separated) {
        let x = rule.x.saturating_add(column.x).saturating_sub(2);
        if x < rule.right() {
            buffer.set_stringn(x, rule.y, "┼", 1, style);
        }
    }
}

/// Which projected rows are drawn. The projection carries the visible window
/// plus overscan with the selection centred; the drawn sub-window centres
/// the selected row too and clamps at both ends, so it is always visible and
/// the first/last rows of the list stay reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisplayWindow {
    offset: usize,
    count: usize,
}

impl DisplayWindow {
    fn new(view: &ProcessesView, body_height: u16) -> Self {
        let capacity = usize::from(body_height);
        let selected_position = view
            .selected
            .as_ref()
            .and_then(|key| view.rows.iter().position(|row| &row.key == key));
        let max_offset = view.rows.len().saturating_sub(capacity);
        let offset = selected_position
            .map_or(0, |position| position.saturating_sub(capacity / 2))
            .min(max_offset);
        let count = view.rows.len().saturating_sub(offset).min(capacity);
        Self { offset, count }
    }

    /// Absolute index (in the full table) of the first drawn row.
    fn first_absolute(&self, view: &ProcessesView) -> u64 {
        u64::from(view.window_start).saturating_add(u64::try_from(self.offset).unwrap_or(u64::MAX))
    }
}

fn title_text(view: &ProcessesView) -> String {
    let mut title = "GPU Processes".to_owned();
    let arrow = if view.ascending { "▲" } else { "▼" };
    title.push_str(&format!(" · sort {} {arrow}", view.sort.label()));
    if let Some((_, label)) = &view.gpu_filter {
        title.push_str(&format!(" · GPU {}", clean(label)));
    }
    if !view.filter.is_empty() {
        title.push_str(&format!(" · filter \"{}\"", clean(&view.filter)));
    }
    title
}

fn empty_text(view: &ProcessesView) -> String {
    if !view.filter.is_empty() {
        format!("no processes match filter \"{}\"", clean(&view.filter))
    } else if let Some((_, label)) = &view.gpu_filter {
        format!("no processes on GPU {}", clean(label))
    } else {
        "no process is using a GPU".to_owned()
    }
}

fn gpu_entries<'a>(row: &'a ProcessRowView, filter: Option<&EntityId>) -> Vec<&'a ProcessGpuView> {
    row.gpus
        .iter()
        .filter(|gpu| filter.is_none_or(|id| &gpu.gpu_entity_id == id))
        .collect()
}

fn joined<'a>(
    entries: &[&'a ProcessGpuView],
    text: impl Fn(&'a ProcessGpuView) -> String,
) -> String {
    if entries.is_empty() {
        return "none".to_owned();
    }
    entries
        .iter()
        .map(|gpu| text(gpu))
        .collect::<Vec<_>>()
        .join(",")
}

/// `C`, `G` or `C+G`: the contexts a process holds on a GPU.
fn usage_code(usage: GpuUsageKind) -> &'static str {
    match usage {
        GpuUsageKind::Compute => "C",
        GpuUsageKind::Graphics => "G",
        GpuUsageKind::ComputeAndGraphics => "C+G",
        GpuUsageKind::Unknown => NOT_AVAILABLE,
    }
}

/// `GPU 0` → `0` inside the GPU column (its header already says GPU).
fn short_gpu_label(label: &str) -> String {
    let label = clean(label);
    label
        .strip_prefix("GPU ")
        .map_or_else(|| label.clone(), str::to_owned)
}

/// [`COLUMNS`] with each column's preferred width grown to its title and
/// the widest of its `rows`' cells (a process on several GPUs lists a value
/// for each).
fn fitted_columns<'a>(rows: impl Iterator<Item = &'a [TableCell; 11]>) -> [ColumnSpec; 11] {
    let mut specs = COLUMNS;
    for cells in rows {
        for (spec, cell) in specs.iter_mut().zip(cells) {
            spec.preferred = spec.preferred.max(saturate_u16(display_width(&cell.text)));
        }
    }
    for spec in &mut specs {
        spec.preferred = spec.preferred.max(saturate_u16(display_width(spec.title)));
    }
    specs
}

fn row_cells(
    row: &ProcessRowView,
    selected: bool,
    filter: Option<&EntityId>,
    context: &DisplayContext,
) -> [TableCell; 11] {
    let palette = &context.theme;
    let gpus = gpu_entries(row, filter);
    let marker = if selected { SELECTION_MARKER } else { "" };
    let missing_style = palette.tone_style(crate::format::CellTone::Missing);
    let numeric = |text: String| {
        let style = if text == NOT_AVAILABLE {
            missing_style
        } else {
            palette.text_style()
        };
        TableCell::new(text, style)
    };
    [
        TableCell::new(marker, palette.title_style()),
        TableCell::new(
            joined(&gpus, |gpu| short_gpu_label(&gpu.gpu_label)),
            palette.secondary_style(),
        ),
        TableCell::new(row.key.pid.to_string(), palette.label_style()),
        TableCell::new(clean(&row.user), palette.label_style()),
        numeric(joined(&gpus, |gpu| usage_code(gpu.usage).to_owned())),
        numeric(format_optional_percent(row.cpu_percent)),
        numeric(joined(&gpus, |gpu| {
            format_optional_percent(gpu.utilization_percent)
        })),
        numeric(joined(&gpus, |gpu| format_optional_bytes(gpu.memory_bytes))),
        numeric(format_optional_bytes(row.rss_bytes)),
        swatch_cell(row, context),
        TableCell::new(clean(&row.name), palette.text_style()),
    ]
}

/// `■` in the process's color when it is tracked, else nothing.
fn swatch_cell(row: &ProcessRowView, context: &DisplayContext) -> TableCell {
    match row.color_slot {
        Some(slot) => TableCell::new(
            COLOR_SWATCH,
            ratatui::style::Style::new().fg(context.theme.process_color(slot)),
        ),
        None => TableCell::new("", context.theme.text_style()),
    }
}

/// The selected row's style recolors its cells; the swatch keeps the color
/// it names.
fn keep_swatch_color(
    frame: &mut Frame<'_>,
    row: Rect,
    columns: &[FittedColumn],
    swatch: &TableCell,
) {
    let (Some(column), Some(color)) = (
        columns.iter().find(|column| column.spec == SWATCH_COLUMN),
        swatch.style.fg,
    ) else {
        return;
    };
    let x = row.x.saturating_add(column.x);
    if x < row.right() && !swatch.text.is_empty() {
        frame.buffer_mut()[(x, row.y)].set_fg(color);
    }
}

fn row_label(row: &ProcessRowView, selected: bool, filter: Option<&EntityId>) -> String {
    let gpus = gpu_entries(row, filter);
    format!(
        "{}process pid {} {} gpu {} type {} gpu util {} gpu mem {} cpu {} cpu mem {}",
        if selected { "selected " } else { "" },
        row.key.pid,
        clean(&row.name),
        joined(&gpus, |gpu| short_gpu_label(&gpu.gpu_label)),
        joined(&gpus, |gpu| usage_code(gpu.usage).to_owned()),
        joined(&gpus, |gpu| format_optional_percent(
            gpu.utilization_percent
        )),
        joined(&gpus, |gpu| format_optional_bytes(gpu.memory_bytes)),
        format_optional_percent(row.cpu_percent),
        format_optional_bytes(row.rss_bytes),
    )
}

fn status_text(view: &ProcessesView, window: &DisplayWindow, filter: Option<&EntityId>) -> String {
    let first = window.first_absolute(view).saturating_add(1);
    let last = first
        .saturating_add(u64::try_from(window.count).unwrap_or(u64::MAX))
        .saturating_sub(1);
    let mut text = format!("rows {first}–{last} of {}", view.total_rows);
    if view.truncated {
        text.push_str(" · list truncated at the process limit");
    }
    match view
        .selected
        .as_ref()
        .and_then(|key| view.rows.iter().find(|row| &row.key == key))
    {
        Some(row) => {
            text.push_str(&format!(
                " · selected pid {} {} · start {}",
                row.key.pid,
                clean(&row.name),
                row.key.start_time_ticks
            ));
            if let Some(reason) = row.missing_reason.as_deref() {
                text.push_str(&format!(" · missing: {}", clean(reason)));
            }
            let gpus = gpu_entries(row, filter);
            if gpus.len() > 1 {
                text.push_str(&format!(" · {} GPUs", gpus.len()));
            }
        }
        None if view.selected.is_some() => text.push_str(" · selection outside the visible rows"),
        None => match &view.selected_missing {
            Some(gone) => text.push_str(&format!(
                " · selected pid {} exited; its values are unavailable",
                gone.pid
            )),
            None => text.push_str(" · no selection (j/k to select)"),
        },
    }
    text
}

fn draw_scrollbar(
    frame: &mut Frame<'_>,
    body: Rect,
    view: &ProcessesView,
    window: &DisplayWindow,
    context: &DisplayContext,
) {
    let total = u64::from(view.total_rows);
    let shown = u64::try_from(window.count).unwrap_or(u64::MAX);
    if body.is_empty() || total <= shown || total == 0 {
        return;
    }
    let height = u64::from(body.height);
    let thumb_start = window.first_absolute(view).saturating_mul(height) / total;
    let thumb_length = (shown.saturating_mul(height) / total).max(1);
    let x = body.right() - 1;
    for offset in 0..body.height {
        let position = u64::from(offset);
        let in_thumb = position >= thumb_start && position < thumb_start + thumb_length;
        let (symbol, style) = if in_thumb {
            ("┃", context.theme.title_style())
        } else {
            ("│", context.theme.muted_style())
        };
        frame
            .buffer_mut()
            .set_string(x, body.y + offset, symbol, style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures;

    #[test]
    fn status_reports_absolute_rows_and_missing_reasons() {
        let mut view = test_fixtures::processes_view(10, 40, 412);
        view.selected = Some(view.rows[2].key.clone());
        view.rows[2].missing_reason = Some("cpu: permission denied".into());
        view.truncated = true;
        let window = DisplayWindow::new(&view, 8);
        let text = status_text(&view, &window, None);
        assert!(text.starts_with("rows 41–48 of 412"), "{text}");
        assert!(text.contains("truncated"));
        assert!(text.contains("missing: cpu: permission denied"));
    }

    #[test]
    fn an_exited_selection_is_reported_as_unavailable() {
        let mut view = test_fixtures::processes_view(3, 0, 3);
        let gone = view.rows[1].key.clone();
        view.rows.remove(1);
        view.total_rows = 2;
        view.selected = None;
        view.selected_index = None;
        view.selected_missing = Some(gone.clone());
        let window = DisplayWindow::new(&view, 8);
        let text = status_text(&view, &window, None);
        assert!(
            text.contains(&format!("selected pid {} exited", gone.pid)),
            "{text}"
        );
        assert!(text.contains("unavailable"), "{text}");
    }

    #[test]
    fn gpu_filter_limits_values_to_that_gpu() {
        let view = test_fixtures::processes_view(3, 0, 3);
        let multi = &view.rows[0];
        assert!(multi.gpus.len() >= 2);
        let first_gpu = multi.gpus[0].gpu_entity_id.clone();
        assert_eq!(gpu_entries(multi, Some(&first_gpu)).len(), 1);
        assert_eq!(joined(&[], |_| String::new()), "none");
    }

    /// A tracked process's name follows a swatch in its color, also on the
    /// selected row; an untracked one has none.
    #[test]
    fn a_tracked_process_names_its_color_before_its_name() {
        let context = test_fixtures::display_context();
        let mut view = test_fixtures::processes_view(2, 0, 2);
        view.rows[0].color_slot = Some(1);
        let cells = row_cells(&view.rows[0], true, None, &context);
        assert_eq!(cells[SWATCH_COLUMN].text, COLOR_SWATCH);
        assert_eq!(
            cells[SWATCH_COLUMN].style.fg,
            Some(context.theme.process_color(1))
        );
        assert_eq!(
            row_cells(&view.rows[1], false, None, &context)[SWATCH_COLUMN].text,
            ""
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 6)).unwrap();
        let mut model = test_fixtures::view_model(dgxtop_core::commands::ViewKind::Overview, 1);
        model.modules.processes = Some(std::sync::Arc::new(view));
        terminal
            .draw(|frame| {
                ProcessesModule::new()
                    .unwrap()
                    .render(&context, &model, frame.area(), frame);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let swatch = (0..120)
            .find(|x| buffer[(*x, 3)].symbol() == COLOR_SWATCH)
            .expect("the selected row draws its swatch");
        assert_eq!(buffer[(swatch, 3)].fg, context.theme.process_color(1));
        assert_eq!(buffer[(swatch, 3)].bg, context.theme.highlight_background);
    }

    /// The GPU leads (D-66), CPU% follows the type, the host memory is
    /// `CPU MEM`; columns are as wide as what they show and set apart by
    /// `│`, the swatch kept beside its name.
    #[test]
    fn the_table_leads_with_the_gpu_and_sets_its_columns_apart() {
        let context = test_fixtures::display_context();
        let view = test_fixtures::processes_view(2, 0, 2);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 6)).unwrap();
        let mut model = test_fixtures::view_model(dgxtop_core::commands::ViewKind::Overview, 1);
        model.modules.processes = Some(std::sync::Arc::new(view));
        terminal
            .draw(|frame| {
                ProcessesModule::new()
                    .unwrap()
                    .render(&context, &model, frame.area(), frame);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y: u16| -> String { (0..120).map(|x| buffer[(x, y)].symbol()).collect() };
        let header = row(1);
        let titles = [
            "GPU ", "PID", "USER", "TYPE", "CPU%", "GPU%", "GPU MEM", "CPU MEM", "NAME",
        ];
        let at: Vec<usize> = titles
            .iter()
            .map(|title| {
                header
                    .find(title)
                    .unwrap_or_else(|| panic!("{title}: {header}"))
            })
            .collect();
        assert!(at.windows(2).all(|pair| pair[0] < pair[1]), "{header}");
        assert!(!header.contains("RSS"), "{header}");
        assert_eq!(header.matches(" │ ").count(), 8, "{header}");
        let first = row(3);
        assert!(
            row(2).contains("─┼─"),
            "the rule under the header: {}",
            row(2)
        );
        let bars = |line: &str| -> Vec<usize> {
            line.chars()
                .enumerate()
                .filter(|(_, symbol)| *symbol == '│')
                .map(|(x, _)| x)
                .collect()
        };
        assert_eq!(bars(&header), bars(&first), "aligned:\n{header}\n{first}");
        assert!(
            first.contains("▶ 0,1 │    4242 │ 1000 │ C,C+G │ 12.5% │ "),
            "{first}"
        );
        assert!(
            first.contains(" │ 45.0%,n/a │ 10.0 GiB,2.0 GiB │ "),
            "{first}"
        );
        assert!(
            first.contains(" │   python3 "),
            "no swatch, then the name: {first}"
        );
    }

    /// A gray rule sets the header apart, and while every process fits so a
    /// blank row sets each apart; with more, one row each (D-72).
    #[test]
    fn rows_stand_a_row_apart_while_they_all_fit() {
        let context = test_fixtures::display_context();
        let draw = |count: usize, height: u16| -> Vec<String> {
            let view = test_fixtures::processes_view(count, 0, u32::try_from(count).unwrap());
            let mut model = test_fixtures::view_model(dgxtop_core::commands::ViewKind::Overview, 1);
            model.modules.processes = Some(std::sync::Arc::new(view));
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, height)).unwrap();
            terminal
                .draw(|frame| {
                    ProcessesModule::new()
                        .unwrap()
                        .render(&context, &model, frame.area(), frame);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..height)
                .map(|y| (0..120).map(|x| buffer[(x, y)].symbol()).collect())
                .collect()
        };
        let rows_of = |lines: &[String]| -> Vec<usize> {
            lines
                .iter()
                .enumerate()
                .filter(|(_, line)| line.contains("│ 1000 │"))
                .map(|(y, _)| y)
                .collect()
        };
        // Three processes in a body of six rows: a blank row between each.
        let spaced = draw(3, 10);
        assert!(spaced[2].contains("─┼─"), "{}", spaced.join("\n"));
        assert_eq!(rows_of(&spaced), vec![3, 5, 7], "{}", spaced.join("\n"));
        // Six processes in the same body of five rows: one row each.
        let packed = draw(6, 10);
        assert_eq!(
            rows_of(&packed),
            vec![3, 4, 5, 6, 7],
            "{}",
            packed.join("\n")
        );
    }

    #[test]
    fn the_type_column_names_each_gpus_contexts() {
        let view = test_fixtures::processes_view(2, 0, 2);
        let cells = row_cells(
            &view.rows[0],
            false,
            None,
            &test_fixtures::display_context(),
        );
        assert_eq!(cells[4].text, "C,C+G");
        let filtered = gpu_entries(&view.rows[1], None);
        assert_eq!(
            joined(&filtered, |gpu| usage_code(gpu.usage).to_owned()),
            "G"
        );
        assert_eq!(usage_code(GpuUsageKind::Unknown), NOT_AVAILABLE);
        assert!(row_label(&view.rows[0], false, None).contains("type C,C+G"));
    }
}
