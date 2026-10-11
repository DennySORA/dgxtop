//! CPU module: the first temperature and power sensor and the current and
//! maximum frequency of each kind of core, with load and tasks on the same
//! line when they fit; the
//! utilization bar with its chart under it (D-43); the usr/sys/iowait/steal
//! breakdown. In the overview it starts collapsed (D-36); a click on the
//! panel or `c` adds every sensor with its source and the htop-style
//! per-core bars.

use dgxtop_core::view::{CoreView, CpuView, FrequencyGroup, LabeledCell, MetricCell, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::ModuleBuildError;
use super::common::{
    BarRow, ManifestSpec, Output, RowCursor, build_manifest, cell_label, cell_spans, draw_line,
    gap, join_if_fits, labeled_cell, measuring_context, message, no_data, panel, panel_with_keys,
    percent_ratio, render_bar_row, render_chart, separator, under_bar,
};
use crate::format::{CellDetail, cell_tone, clean, display_width, truncate_to_width, value_text};
use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, OverviewCell, RenderOutput, Slot, Topic,
    controls,
};
use crate::widgets::Gauge;

/// Narrowest per-core cell: id (4) + bar (5) + value (6) + gutter (1).
const MIN_CORE_CELL: u16 = 16;
/// Rows of a panel that only says "no data" (borders + one line).
const NO_DATA_ROWS: u16 = 3;
/// Core rows aimed for before adding grid columns.
const PREFERRED_CORE_ROWS: usize = 16;
/// Width from which a core cell also shows frequency and temperature.
const RICH_CORE_CELL: u16 = 40;
/// Module id; the controller's `c` key expands this module.
pub const CPU_MODULE_ID: &str = "cpu";
/// Rows of the utilization chart under the bar (D-43).
const CHART_ROWS: u16 = 3;
/// Rows besides the text lines: the utilization bar and its chart.
const BAR_AND_CHART_ROWS: u16 = 1 + CHART_ROWS;
/// Value column of the utilization bar (`100.0%`).
const PERCENT_WIDTH: u16 = 6;
const EXPAND_HINT: &str = " ▸";
const COLLAPSE_HINT: &str = " ▾";
/// The keys acting on the panel, at the right of its title (D-70).
const EXPAND_KEYS: &str = "c/click cores";
const COLLAPSE_KEYS: &str = "c/click collapse";
/// Breakdown marker drawn in each part's color.
const SWATCH: &str = "■ ";
/// A kind of core whose cores report no current frequency (idle).
const NO_CURRENT_FREQUENCY: &str = "–";

pub struct CpuModule {
    manifest: DisplayModuleManifest,
}

impl CpuModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: CPU_MODULE_ID,
                title: "CPU utilization and temperature",
                topics: &[Topic::Cpu, Topic::Sources, Topic::Health],
                min_columns: 36,
                min_rows: 4,
                priority: 90,
                slots: &[Slot::Overview],
            })?,
        })
    }
}

impl DisplayModule for CpuModule {
    fn manifest(&self) -> &DisplayModuleManifest {
        &self.manifest
    }

    fn overview_cell(&self) -> Option<OverviewCell> {
        // Narrower than memory (D-40).
        Some(OverviewCell::at(0, 0, 42))
    }

    fn preferred_rows(&self, model: &ViewModel, width: u16, slot: Slot) -> Option<u16> {
        let Some(cpu) = model.modules.cpu.as_deref() else {
            return Some(NO_DATA_ROWS);
        };
        let count = cpu.cores.len();
        let core_rows = if count == 0 {
            0
        } else {
            let max_columns = (width.saturating_sub(2) / MIN_CORE_CELL).max(1);
            let baseline = if count > 4 { 2 } else { 1 };
            let columns = saturate_u16(count.div_ceil(PREFERRED_CORE_ROWS))
                .max(baseline)
                .clamp(1, max_columns);
            saturate_u16(count.div_ceil(usize::from(columns)))
        };
        let sensor_rows = saturate_u16(sensor_cells(cpu).len().max(1));
        Some(collapsed_rows(cpu, model, width, slot) + sensor_rows + core_rows)
    }

    fn collapsed_rows(&self, model: &ViewModel, width: u16, slot: Slot) -> Option<u16> {
        let cpu = model.modules.cpu.as_deref()?;
        (slot == Slot::Overview).then(|| collapsed_rows(cpu, model, width, slot))
    }

    fn render(
        &self,
        context: &DisplayContext,
        model: &ViewModel,
        area: Rect,
        frame: &mut Frame<'_>,
    ) -> RenderOutput {
        let mut output = Output::default();
        let Some(cpu) = model.modules.cpu.as_deref() else {
            let inner = panel(frame, area, "CPU", context);
            message(frame, inner, &no_data("cpu"), context.theme.muted_style());
            output.label("cpu: no data");
            return output.finish();
        };
        let collapsible = context.slot == Slot::Overview;
        let expanded = !collapsible || context.expanded;
        let mut title = "CPU".to_owned();
        if collapsible {
            title.push_str(if expanded { COLLAPSE_HINT } else { EXPAND_HINT });
        }
        if let Some(model_name) = cpu.model.as_deref() {
            title.push_str(if collapsible { " " } else { " · " });
            title.push_str(&clean(model_name));
        }
        let keys = match (collapsible, expanded) {
            (false, _) => "",
            (true, false) => EXPAND_KEYS,
            (true, true) => COLLAPSE_KEYS,
        };
        let inner = panel_with_keys(frame, area, &title, keys, context);
        if collapsible {
            // Collapsed, the whole panel expands; expanded, the title row
            // collapses so clicks on the cores stay free.
            let target = if expanded {
                Rect::new(area.x, area.y, area.width, 1.min(area.height))
            } else {
                area
            };
            output.target(model, &self.manifest, controls::MODULE_TOGGLE, target);
        }
        let mut rows = RowCursor::new(inner);
        let lines = text_lines(cpu, inner.width, context);
        let after_chart = saturate_u16(lines.len().saturating_sub(1));
        let mut text = lines.into_iter();
        if let (Some(line), Some(row)) = (text.next(), rows.next_row()) {
            draw_line(frame.buffer_mut(), row, &line);
        }
        // Collapsed, the chart takes the rows the panel has beyond its own,
        // so it fills a row the memory panel beside it made taller (D-70).
        let chart_rows = if expanded {
            BAR_AND_CHART_ROWS
        } else {
            rows.remaining()
                .saturating_sub(after_chart)
                .max(BAR_AND_CHART_ROWS)
        };
        if let Some(area) = rows.take(chart_rows) {
            render_utilization(frame, area, cpu, context);
        }
        for line in text {
            let Some(row) = rows.next_row() else { break };
            draw_line(frame.buffer_mut(), row, &line);
        }
        if expanded {
            render_sensors_and_cores(frame, &mut rows, cpu, inner.width, context);
        }
        add_labels(&mut output, cpu, context);
        output.finish()
    }
}

/// Every sensor with its source (one per line while the cores still get
/// room), then the per-core grid; the trend keeps the rows left over.
fn render_sensors_and_cores(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    cpu: &CpuView,
    width: u16,
    context: &DisplayContext,
) {
    let sensors = sensor_cells(cpu);
    if usize::from(rows.remaining()) >= sensors.len() + 2 && !sensors.is_empty() {
        for (label, cell) in &sensors {
            let Some(row) = rows.next_row() else { break };
            let detail = if row.width >= 50 {
                CellDetail::SOURCED
            } else {
                CellDetail::COMPACT
            };
            draw_line(
                frame.buffer_mut(),
                row,
                &Line::from(labeled_cell(label, cell, detail, context)),
            );
        }
    } else if let Some(row) = rows.next_row() {
        draw_line(frame.buffer_mut(), row, &sensors_line(&sensors, context));
    }
    let core_rows = core_rows_needed(cpu.cores.len(), width, rows.remaining());
    if let Some(grid) = rows.take(core_rows) {
        render_cores(frame, grid, &cpu.cores, context);
    }
}

/// Panel height without the expanded detail: borders, the text lines, the
/// utilization bar and its chart.
fn collapsed_rows(cpu: &CpuView, model: &ViewModel, width: u16, slot: Slot) -> u16 {
    let context = measuring_context(model, slot);
    let text = text_lines(cpu, width.saturating_sub(2), &context).len();
    2 + saturate_u16(text) + BAR_AND_CHART_ROWS
}

/// The sensors line first, joined by load and tasks when both fit `width`,
/// then the breakdown and, when it did not fit, the load line.
fn text_lines(cpu: &CpuView, width: u16, context: &DisplayContext) -> Vec<Line<'static>> {
    let mut lines = join_if_fits(
        summary_line(cpu, context),
        load_line(cpu, context),
        width,
        context,
    );
    lines.insert(1, breakdown_line(cpu, context));
    lines
}

/// `util ▆▆▆▆▆▆▆▆▆ 45.3%` with the utilization history under it (D-43).
fn render_utilization(frame: &mut Frame<'_>, area: Rect, cpu: &CpuView, context: &DisplayContext) {
    let mut rows = RowCursor::new(area);
    let Some(row) = rows.next_row() else { return };
    let ratio = percent_ratio(&cpu.total);
    let value = value_text(&cpu.total);
    let gauge = render_bar_row(
        frame,
        row,
        &BarRow {
            label: "util",
            ratio,
            value: &value,
            value_width: PERCENT_WIDTH,
        },
        context,
    );
    frame.render_widget(Gauge::new(ratio, "", &context.theme), gauge);
    if let Some(chart) = rows.rest() {
        render_chart(
            frame,
            under_bar(chart),
            &cpu.utilization_trend.points,
            Some(100.0),
            context,
        );
    }
}

/// `71°C │ 12 W │ 2.1/2.8 GHz · 3.4/3.9 GHz` (D-66): the first temperature
/// and power sensor and, per kind of core, the mean frequency its cores run
/// at now (D-67) over their maximum; the expanded panel lists every sensor.
fn summary_line(cpu: &CpuView, context: &DisplayContext) -> Line<'static> {
    let glance = |cell: &MetricCell| cell_spans(cell, CellDetail::GLANCE, context);
    let temperature = cpu
        .package_temperatures
        .first()
        .or_else(|| cpu.soc_temperatures.first());
    let mut parts: Vec<Vec<Span<'static>>> = [temperature, cpu.power.first()]
        .into_iter()
        .flatten()
        .map(|sensor| glance(&sensor.cell))
        .collect();
    if !cpu.frequencies.is_empty() {
        parts.push(frequency_spans(&cpu.frequencies, context));
    }
    let mut spans = Vec::new();
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            spans.push(separator(context));
        }
        spans.extend(part);
    }
    Line::from(spans)
}

/// `2.1/2.8 GHz · 3.4/3.9 GHz`: each kind of core's current frequency over
/// its maximum; `–` for a kind whose cores report no current frequency
/// (idle), and only the current one when cpufreq gives no maximum.
fn frequency_spans(groups: &[FrequencyGroup], context: &DisplayContext) -> Vec<Span<'static>> {
    let theme = &context.theme;
    let mut spans = Vec::new();
    for (index, group) in groups.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", theme.muted_style()));
        }
        spans.push(match gigahertz(&group.current) {
            Some(current) => Span::styled(current, theme.tone_style(cell_tone(&group.current))),
            None => Span::styled(NO_CURRENT_FREQUENCY, theme.muted_style()),
        });
        if let Some(maximum) = gigahertz(&group.maximum) {
            spans.push(Span::styled(format!("/{maximum}"), theme.label_style()));
        }
        spans.push(Span::styled(" GHz", theme.label_style()));
    }
    spans
}

/// A frequency cell (MHz) as GHz with one decimal; `None` without a value.
fn gigahertz(cell: &MetricCell) -> Option<String> {
    let mhz = cell.value_f64().filter(|_| cell.state.has_value())?;
    Some(format!("{:.1}", mhz / 1000.0))
}

/// `load 1.34 1.15 1.05 │ tasks 2/763`; on a line of its own when the
/// panel, narrower than memory (D-40), cannot fit it beside the sensors.
fn load_line(cpu: &CpuView, context: &DisplayContext) -> Line<'static> {
    let glance = |cell: &MetricCell| cell_spans(cell, CellDetail::GLANCE, context);
    let mut spans = vec![Span::styled("load ", context.theme.label_style())];
    for (index, cell) in cpu.load.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        spans.extend(glance(cell));
    }
    spans.push(separator(context));
    spans.push(Span::styled("tasks ", context.theme.label_style()));
    spans.extend(glance(&cpu.tasks_running));
    spans.push(Span::styled("/", context.theme.muted_style()));
    spans.extend(glance(&cpu.tasks_total));
    Line::from(spans)
}

/// Package/SoC temperatures and power, labeled, in display order.
fn sensor_cells(cpu: &CpuView) -> Vec<(String, &MetricCell)> {
    let mut cells = Vec::new();
    for (prefix, group) in [
        ("temp", &cpu.package_temperatures),
        ("soc", &cpu.soc_temperatures),
        ("power", &cpu.power),
    ] {
        for labeled in group.iter() {
            cells.push((sensor_label(prefix, labeled), &labeled.cell));
        }
    }
    cells
}

fn sensor_label(prefix: &str, labeled: &LabeledCell) -> String {
    let label = clean(&labeled.label);
    if label.is_empty() || label.eq_ignore_ascii_case(prefix) {
        prefix.to_owned()
    } else if label.to_ascii_lowercase().starts_with(prefix) {
        label
    } else {
        format!("{prefix} {label}")
    }
}

/// All sensors on one line (compact panels).
fn sensors_line(sensors: &[(String, &MetricCell)], context: &DisplayContext) -> Line<'static> {
    if sensors.is_empty() {
        return Line::from(Span::styled(
            "temperature and power: none reported",
            context.theme.muted_style(),
        ));
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (label, cell) in sensors {
        if !spans.is_empty() {
            spans.push(gap(context));
        }
        spans.extend(labeled_cell(label, cell, CellDetail::COMPACT, context));
    }
    Line::from(spans)
}

/// `■ usr 3%  ■ sys 2%  ■ iowait 0%  ■ steal 0%`, each marker in its color.
fn breakdown_line(cpu: &CpuView, context: &DisplayContext) -> Line<'static> {
    let theme = &context.theme;
    let parts: [(&str, &MetricCell, Color); 4] = [
        ("usr", &cpu.user, theme.success),
        ("sys", &cpu.system, theme.danger),
        ("iowait", &cpu.iowait, theme.warning),
        ("steal", &cpu.steal, theme.text_muted),
    ];
    let mut spans = Vec::new();
    for (label, cell, color) in parts {
        if !spans.is_empty() {
            spans.push(gap(context));
        }
        spans.push(Span::styled(SWATCH, Style::new().fg(color)));
        spans.extend(labeled_cell(label, cell, CellDetail::GLANCE, context));
    }
    Line::from(spans)
}

fn grid_columns(count: usize, width: u16, height: u16) -> u16 {
    let max_columns = (width / MIN_CORE_CELL).max(1);
    let rows = usize::from(height.max(1));
    let wanted = saturate_u16(count.div_ceil(rows));
    let preferred = if count > 4 { wanted.max(2) } else { wanted };
    preferred.clamp(1, max_columns)
}

/// Rows the grid would like given the width (bounded by what remains).
fn core_rows_needed(count: usize, width: u16, remaining: u16) -> u16 {
    if count == 0 {
        return 0;
    }
    let columns = usize::from(grid_columns(count, width, remaining));
    saturate_u16(count.div_ceil(columns)).min(remaining)
}

fn render_cores(frame: &mut Frame<'_>, area: Rect, cores: &[CoreView], context: &DisplayContext) {
    if cores.is_empty() || area.is_empty() {
        return;
    }
    let columns = grid_columns(cores.len(), area.width, area.height);
    let capacity = usize::from(columns) * usize::from(area.height);
    let overflow = cores.len() > capacity;
    let shown = if overflow {
        capacity.saturating_sub(1)
    } else {
        cores.len()
    };
    let rows_used = saturate_u16(shown.div_ceil(usize::from(columns)).max(1)).min(area.height);
    let cell_width = area.width / columns;
    for (index, core) in cores.iter().take(shown).enumerate() {
        let column = saturate_u16(index / usize::from(rows_used));
        let row = saturate_u16(index % usize::from(rows_used));
        let cell = Rect::new(
            area.x + column * cell_width,
            area.y + row,
            cell_width.saturating_sub(1),
            1,
        );
        render_core(frame, cell, core, context);
    }
    if overflow {
        let index = shown;
        let column = saturate_u16(index / usize::from(rows_used)).min(columns - 1);
        let row = saturate_u16(index % usize::from(rows_used));
        let text = format!("+{} cores", cores.len() - shown);
        frame.buffer_mut().set_stringn(
            area.x + column * cell_width,
            area.y + row,
            text,
            usize::from(cell_width),
            context.theme.muted_style(),
        );
    }
}

fn render_core(frame: &mut Frame<'_>, cell: Rect, core: &CoreView, context: &DisplayContext) {
    if cell.width < 6 {
        return;
    }
    let id = truncate_to_width(&clean(&core.core_id), 3);
    let id_text = format!("{id:>3} ");
    frame
        .buffer_mut()
        .set_string(cell.x, cell.y, &id_text, context.theme.muted_style());
    let mut label = value_text(&core.utilization);
    if cell.width >= RICH_CORE_CELL {
        label = format!(
            "{label} {} {}",
            value_text(&core.frequency),
            value_text(&core.temperature)
        );
    }
    let id_width = saturate_u16(display_width(&id_text));
    let gauge = Rect::new(
        cell.x + id_width,
        cell.y,
        cell.width.saturating_sub(id_width),
        1,
    );
    frame.render_widget(
        Gauge::new(percent_ratio(&core.utilization), &label, &context.theme),
        gauge,
    );
}

fn add_labels(output: &mut Output, cpu: &CpuView, context: &DisplayContext) {
    output.label(cell_label("cpu total", &cpu.total, context));
    for labeled in &cpu.package_temperatures {
        output.label(cell_label(
            &format!("cpu temperature {}", clean(&labeled.label)),
            &labeled.cell,
            context,
        ));
    }
    for labeled in &cpu.soc_temperatures {
        output.label(cell_label(
            &format!("soc temperature {}", clean(&labeled.label)),
            &labeled.cell,
            context,
        ));
    }
    for labeled in &cpu.power {
        output.label(cell_label(
            &format!("cpu power {}", clean(&labeled.label)),
            &labeled.cell,
            context,
        ));
    }
    output.label(cell_label("cpu load 1m", &cpu.load[0], context));
    output.label(format!("cpu cores: {}", cpu.cores.len()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures;
    use dgxtop_core::metric::{CanonicalUnit, Scalar};
    use dgxtop_core::view::CellState;

    fn megahertz(value: Option<f64>) -> MetricCell {
        match value {
            Some(mhz) => MetricCell {
                value: Scalar::from_f64(mhz).ok(),
                ..MetricCell::missing(CanonicalUnit::MHz, CellState::Ok)
            },
            None => MetricCell::missing(CanonicalUnit::MHz, CellState::Unavailable),
        }
    }

    fn group(current: Option<f64>, maximum: Option<f64>) -> FrequencyGroup {
        FrequencyGroup {
            maximum: megahertz(maximum),
            current: megahertz(current),
            cores: 10,
        }
    }

    fn frequency_text(groups: &[FrequencyGroup]) -> String {
        let context = test_fixtures::display_context();
        frequency_spans(groups, &context)
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// A hybrid CPU shows each kind of core's current over its maximum
    /// frequency; one kind shows one value; an idle kind shows `–`; no
    /// maximum shows only the current frequency.
    #[test]
    fn frequencies_show_current_over_maximum_per_kind_of_core() {
        assert_eq!(
            frequency_text(&[
                group(Some(2100.0), Some(2808.0)),
                group(Some(3400.0), Some(3900.0))
            ]),
            "2.1/2.8 GHz · 3.4/3.9 GHz"
        );
        assert_eq!(
            frequency_text(&[group(Some(3000.0), Some(4500.0))]),
            "3.0/4.5 GHz"
        );
        assert_eq!(frequency_text(&[group(None, Some(3900.0))]), "–/3.9 GHz");
        assert_eq!(frequency_text(&[group(Some(1800.0), None)]), "1.8 GHz");
    }

    /// Without cpufreq the summary has no frequency part and no dangling
    /// separator.
    #[test]
    fn the_summary_omits_a_missing_frequency() {
        let context = test_fixtures::display_context();
        let mut cpu = test_fixtures::cpu_view();
        let text = |cpu: &CpuView| -> String {
            summary_line(cpu, &context)
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        assert!(text(&cpu).ends_with("│ 3.0/3.9 GHz"), "{}", text(&cpu));
        cpu.frequencies.clear();
        let without = text(&cpu);
        assert!(
            !without.contains("GHz") && !without.ends_with("│ "),
            "{without}"
        );
    }

    /// Collapsed, the utilization chart takes the rows a taller panel has
    /// beyond its own, so no blank rows stay under the breakdown (D-70).
    #[test]
    fn the_collapsed_chart_fills_a_taller_panel() {
        let context = test_fixtures::display_context();
        let model = test_fixtures::view_model(dgxtop_core::commands::ViewKind::Overview, 1);
        let module = CpuModule::new().unwrap();
        let own = module.preferred_rows(&model, 80, Slot::Overview).unwrap();
        for height in [own, own + 4] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, height)).unwrap();
            terminal
                .draw(|frame| {
                    module.render(&context, &model, frame.area(), frame);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let breakdown = (0..height)
                .find(|y| {
                    (0..80)
                        .map(|x| buffer[(x, *y)].symbol())
                        .collect::<String>()
                        .contains("usr")
                })
                .expect("the breakdown line");
            assert_eq!(breakdown, height - 2, "right above the bottom border");
        }
    }
}
