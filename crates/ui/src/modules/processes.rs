//! Processes module: the projection's visible window of the process table.
//!
//! Columns (GPU first): PID, USER, GPU, TYPE (compute `C`, graphics `G` or
//! both, as `nvidia-smi` prints them, D-54), GPU util, GPU memory, CPU%,
//! RSS, NAME. Rows are identified by `ProcessKey`; the selected row is
//! marked with `▶` (not only by color) and published as the only `K` hit
//! target.

use dgxtop_core::canonical::GpuUsageKind;
use dgxtop_core::identity::EntityId;
use dgxtop_core::view::{ProcessGpuView, ProcessRowView, ProcessesView, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;

use super::ModuleBuildError;
use super::common::{
    ColumnSpec, ManifestSpec, Output, RowCursor, TableCell, build_manifest, draw_table_header,
    draw_table_row, fit_columns, message, no_data, panel,
};
use crate::format::{
    NOT_AVAILABLE, clean, format_optional_bytes, format_optional_percent, truncate_to_width,
};
use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, OverviewCell, RenderOutput, Slot, Topic,
    controls,
};

const SELECTION_MARKER: &str = "▶";

/// Reference order (D-38): identity, GPU use, host use, then the name.
const COLUMNS: [ColumnSpec; 10] = [
    ColumnSpec::left("", 1, 255),
    ColumnSpec::right("PID", 7, 98),
    ColumnSpec::left("USER", 8, 40).prefer(10),
    ColumnSpec::left("GPU", 4, 100).prefer(7),
    ColumnSpec::left("TYPE", 4, 45).prefer(5),
    ColumnSpec::right("GPU%", 6, 85).prefer(12),
    ColumnSpec::right("GPU MEM", 9, 70).prefer(19),
    ColumnSpec::right("CPU%", 6, 80),
    ColumnSpec::right("RSS", 9, 60),
    ColumnSpec::left("NAME", 10, 96).flexible().prefer(24),
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
                topics: &[Topic::Processes, Topic::Gpu],
                min_columns: 40,
                min_rows: 5,
                priority: 75,
                slots: &[Slot::Overview, Slot::Detail, Slot::Sidebar],
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
        Some(saturate_u16(rows.max(1)).saturating_add(4))
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
        let overview = context.slot == Slot::Overview;
        let inner = panel(frame, area, &title_text(view, overview), context);
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
        let columns = fit_columns(&COLUMNS, table.body.width.saturating_sub(1));
        draw_table_header(
            frame.buffer_mut(),
            table.header,
            &COLUMNS,
            &columns,
            context.theme.header_style(),
        );
        let window = DisplayWindow::new(view, table.body.height);
        let filter = view.gpu_filter.as_ref().map(|(id, _)| id);
        let mut cursor = RowCursor::new(table.body);
        for row in view.rows.iter().skip(window.offset).take(window.count) {
            let Some(rect) = cursor.next_row() else { break };
            let selected = view.selected.as_ref() == Some(&row.key);
            let style = selected.then(|| context.theme.selected_row_style());
            let cells = row_cells(row, selected, filter, context);
            draw_table_row(frame.buffer_mut(), rect, &columns, &cells, style);
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

/// Header, body and optional status row of the table area.
struct TableLayout {
    header: Rect,
    body: Rect,
    status: Option<Rect>,
}

impl TableLayout {
    fn new(inner: Rect) -> Self {
        let mut rows = RowCursor::new(inner);
        let header = rows.next_row().unwrap_or_default();
        let status_rows = u16::from(inner.height >= 4);
        let body = rows
            .take(rows.remaining().saturating_sub(status_rows))
            .unwrap_or_default();
        let status = rows.next_row();
        Self {
            header,
            body,
            status,
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

fn title_text(view: &ProcessesView, overview: bool) -> String {
    let mut title = if overview {
        "GPU Processes".to_owned()
    } else {
        "Processes".to_owned()
    };
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

fn row_cells(
    row: &ProcessRowView,
    selected: bool,
    filter: Option<&EntityId>,
    context: &DisplayContext,
) -> [TableCell; 10] {
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
        TableCell::new(row.key.pid.to_string(), palette.label_style()),
        TableCell::new(clean(&row.user), palette.label_style()),
        TableCell::new(
            joined(&gpus, |gpu| short_gpu_label(&gpu.gpu_label)),
            palette.secondary_style(),
        ),
        numeric(joined(&gpus, |gpu| usage_code(gpu.usage).to_owned())),
        numeric(joined(&gpus, |gpu| {
            format_optional_percent(gpu.utilization_percent)
        })),
        numeric(joined(&gpus, |gpu| format_optional_bytes(gpu.memory_bytes))),
        numeric(format_optional_percent(row.cpu_percent)),
        numeric(format_optional_bytes(row.rss_bytes)),
        TableCell::new(clean(&row.name), palette.text_style()),
    ]
}

fn row_label(row: &ProcessRowView, selected: bool, filter: Option<&EntityId>) -> String {
    let gpus = gpu_entries(row, filter);
    format!(
        "{}process pid {} {} gpu {} type {} gpu util {} gpu mem {} cpu {} rss {}",
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
