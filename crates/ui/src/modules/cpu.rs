//! CPU module: the CPU's model on the title (an arm64 CPU's kinds of
//! cores, D-132); the first temperature (in the level color of its 0–100 °C
//! scale, D-104) with the cluster it comes from where it is the hottest of
//! several (D-137), the first CPU package's power after `⚡` (D-106) and the
//! current and maximum frequency of each kind of core, with the limit the
//! cpufreq policy sets where it caps them (D-136), with load and tasks
//! on the same line when they fit; the
//! utilization bar with its chart under it (D-43); the usr/sys/iowait/steal
//! breakdown, joined by the CPU's stall pressure when both fit (D-135). In the overview it starts collapsed (D-36); a click on the
//! panel or `c` adds every sensor with its source and the htop-style
//! per-core bars.

use dgxtop_core::metric::CanonicalUnit;
use dgxtop_core::view::{CoreView, CpuView, FrequencyGroup, LabeledCell, MetricCell, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::ModuleBuildError;
use super::common::{
    BAR_LABEL_WIDTH, BarRow, ManifestSpec, Output, RowCursor, build_manifest, cell_label,
    cell_spans, draw_line, fitting_keys, gap, has_value, join_if_fits, labeled_cell,
    labeled_temperature, measuring_context, message, no_data, panel, panel_with_keys,
    percent_ratio, power_icon, pressure_labels, pressure_line, render_bar_row, render_chart,
    separator, telling_pressure, temperature_spans, under_bar, value_styled,
};
use crate::format::{
    CellDetail, cell_tone, clean, display_width, fixed, truncate_to_width, value_text,
};
use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, OverviewCell, RenderOutput, Slot,
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
/// The keys acting on the panel, at the right of its title (D-70): the
/// fullest form that fits beside the CPU's name.
const EXPAND_KEYS: &[&str] = &["c/click cores", "c cores"];
const COLLAPSE_KEYS: &[&str] = &["c/click collapse", "c collapse"];
/// Breakdown marker drawn in each part's color.
const SWATCH: &str = "■ ";
/// A kind of core whose cores report no current frequency (idle).
const NO_CURRENT_FREQUENCY: &str = "–";
/// A kilohertz, the step cpufreq counts in, in the megahertz of a cell.
const MEGAHERTZ_PER_KILOHERTZ: f64 = 0.001;

#[derive(Debug)]
pub struct CpuModule {
    manifest: DisplayModuleManifest,
}

impl CpuModule {
    /// # Errors
    ///
    /// Returns a `ModuleBuildError` when the module's static manifest is
    /// invalid, which is a build defect.
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(&ManifestSpec {
                id: CPU_MODULE_ID,
                title: "CPU utilization and temperature",
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
            (true, false) => fitting_keys(area.width, &title, EXPAND_KEYS),
            (true, true) => fitting_keys(area.width, &title, COLLAPSE_KEYS),
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
                &Line::from(sensor_spans(label, cell, detail, context)),
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
/// then the breakdown, joined by the CPU's stall pressure, while it has
/// something to say (D-143), when both fit (D-135), and, when it did not
/// fit, the load line.
fn text_lines(cpu: &CpuView, width: u16, context: &DisplayContext) -> Vec<Line<'static>> {
    let load = load_line(cpu, context);
    let summary = fitting_summary(cpu, &load, width, context);
    let mut lines = join_if_fits(summary, load, width, context);
    let breakdown = breakdown_line(cpu, context);
    let below_chart = match telling_pressure(cpu.pressure.as_ref()) {
        Some(pressure) => join_if_fits(breakdown, pressure_line(pressure, context), width, context),
        None => vec![breakdown],
    };
    lines.splice(1..1, below_chart);
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
            label_width: BAR_LABEL_WIDTH,
            label_style: context.theme.label_style(),
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

/// `71°C │ ⚡12 W │ 2.1/2.8 GHz · 3.4/3.9 GHz` (D-66): the first temperature
/// in its level color (D-104), the first CPU package's power while it has a
/// value (D-106) and, per kind of core, the mean frequency its cores run at
/// now (D-67) over their maximum; the expanded panel lists every sensor.
fn summary_line(cpu: &CpuView, context: &DisplayContext) -> Line<'static> {
    summary_with(cpu, None, context)
}

/// [`summary_line`] with the cluster its temperature comes from after it,
/// `89°C cluster 0p` (D-137), where the package's temperature is its
/// hottest cluster's; `None` where a sensor reports the package itself.
fn named_summary_line(cpu: &CpuView, context: &DisplayContext) -> Option<Line<'static>> {
    let cluster = cpu.hottest_cluster.as_deref()?;
    Some(summary_with(cpu, Some(cluster), context))
}

/// The summary line; `origin` follows a package's temperature, muted.
fn summary_with(cpu: &CpuView, origin: Option<&str>, context: &DisplayContext) -> Line<'static> {
    let package = cpu.package_temperatures.first();
    let temperature = package.or_else(|| cpu.soc_temperatures.first());
    let mut parts: Vec<Vec<Span<'static>>> = Vec::new();
    if let Some(sensor) = temperature {
        let mut spans = temperature_spans(&sensor.cell, CellDetail::GLANCE, context);
        if let Some(origin) = origin.filter(|_| package.is_some() && has_value(&sensor.cell)) {
            spans.push(Span::styled(
                format!(" {}", clean(origin)),
                context.theme.muted_style(),
            ));
        }
        parts.push(spans);
    }
    // Shown only while it is read: a host whose CPU power cannot be read
    // has none (D-106).
    if let Some(power) = cpu
        .package_power
        .first()
        .filter(|power| has_value(&power.cell))
    {
        let mut spans = vec![power_icon(context)];
        spans.extend(cell_spans(&power.cell, CellDetail::GLANCE, context));
        parts.push(spans);
    }
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

/// The summary line, naming the hottest cluster after the temperature
/// (D-137) unless that would push `load` off its line or the line past
/// `width`: the cluster is also on the package's row of the expanded
/// panel, the load nowhere else.
fn fitting_summary(
    cpu: &CpuView,
    load: &Line<'_>,
    width: u16,
    context: &DisplayContext,
) -> Line<'static> {
    let plain = summary_line(cpu, context);
    let Some(named) = named_summary_line(cpu, context) else {
        return plain;
    };
    let width = usize::from(width);
    let with_load =
        |line: &Line<'_>| line.width() + display_width(&separator(context).content) + load.width();
    let keeps_load = with_load(&named) <= width || with_load(&plain) > width;
    if keeps_load && named.width() <= width {
        named
    } else {
        plain
    }
}

/// `2.1/2.8 GHz · 3.4/3.9 GHz`: each kind of core's current frequency over
/// its maximum; `–` for a kind whose cores report no current frequency
/// (idle), and only the current one when cpufreq gives no maximum. A kind
/// the cpufreq policy caps reads `2.9/3.0 (3.9) GHz` (D-136): what it runs
/// at over the limit set for it, in the warning color, then what the
/// hardware can do.
fn frequency_spans(groups: &[FrequencyGroup], context: &DisplayContext) -> Vec<Span<'static>> {
    let theme = &context.theme;
    let mut spans = Vec::new();
    for (index, group) in groups.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", theme.muted_style()));
        }
        spans.push(gigahertz(&group.current).map_or_else(
            || Span::styled(NO_CURRENT_FREQUENCY, theme.muted_style()),
            |current| Span::styled(current, theme.tone_style(cell_tone(&group.current))),
        ));
        if let Some((limit, maximum)) = frequency_cap(group) {
            spans.push(Span::styled("/", theme.label_style()));
            spans.push(Span::styled(limit, Style::new().fg(theme.warning)));
            spans.push(Span::styled(format!(" ({maximum})"), theme.label_style()));
        } else {
            let top = gigahertz(&group.maximum).or_else(|| gigahertz(&group.limit));
            if let Some(top) = top {
                spans.push(Span::styled(format!("/{top}"), theme.label_style()));
            }
        }
        spans.push(Span::styled(" GHz", theme.label_style()));
    }
    spans
}

/// The cap a kind of core runs under (D-136): its limit and its maximum in
/// GHz, with as few decimals as tell them apart, while the cpufreq policy
/// holds the cores below what the hardware can do; `None` while nothing
/// caps them or either is not read.
fn frequency_cap(group: &FrequencyGroup) -> Option<(String, String)> {
    let megahertz = |cell: &MetricCell| cell.value_f64().filter(|_| cell.state.has_value());
    let (limit, maximum) = (megahertz(&group.limit)?, megahertz(&group.maximum)?);
    // cpufreq counts in whole kHz: anything lower than the maximum caps.
    if limit + MEGAHERTZ_PER_KILOHERTZ / 2.0 >= maximum {
        return None;
    }
    (1..=3)
        .map(|decimals| {
            (
                fixed(limit / 1000.0, decimals),
                fixed(maximum / 1000.0, decimals),
            )
        })
        .find(|(limit, maximum)| limit != maximum)
}

/// A frequency cell (MHz) as GHz with one decimal; `None` without a value.
fn gigahertz(cell: &MetricCell) -> Option<String> {
    let mhz = cell.value_f64().filter(|_| cell.state.has_value())?;
    Some(format!("{:.1}", mhz / 1000.0))
}

/// `load 1.34 1.15 1.05 │ tasks 2/763`; on a line of its own when the
/// panel, narrower than memory (D-40), cannot fit it beside the sensors.
/// Each average is in the level color of that many CPUs busy (D-143): a
/// load of half the CPUs is amber like a half-full utilization bar, one
/// of every CPU red, where tasks begin to wait for a CPU.
fn load_line(cpu: &CpuView, context: &DisplayContext) -> Line<'static> {
    let glance = |cell: &MetricCell| cell_spans(cell, CellDetail::GLANCE, context);
    let mut spans = vec![Span::styled("load ", context.theme.label_style())];
    for (index, cell) in cpu.load.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        spans.extend(value_styled(
            glance(cell),
            context.theme.load_style(cell, cpu.cores.len()),
        ));
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
        ("power", &cpu.package_power),
        ("power", &cpu.power),
    ] {
        for labeled in group {
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

/// A sensor's `label value`, a temperature in its level color (D-104).
fn sensor_spans(
    label: &str,
    cell: &MetricCell,
    detail: CellDetail,
    context: &DisplayContext,
) -> Vec<Span<'static>> {
    if cell.unit == CanonicalUnit::DegC {
        labeled_temperature(label, cell, detail, context)
    } else {
        labeled_cell(label, cell, detail, context)
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
        spans.extend(sensor_spans(label, cell, CellDetail::COMPACT, context));
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
    let id_width = saturate_u16(display_width(&id_text));
    let mut gauge = Rect::new(
        cell.x + id_width,
        cell.y,
        cell.width.saturating_sub(id_width),
        1,
    );
    if cell.width >= RICH_CORE_CELL {
        label = format!("{label} {}", value_text(&core.frequency));
        // The temperature ends the cell in its own level color (D-104).
        let temperature = value_text(&core.temperature);
        let room = saturate_u16(display_width(&temperature)).saturating_add(1);
        gauge.width = gauge.width.saturating_sub(room);
        frame.buffer_mut().set_stringn(
            gauge.right().saturating_add(1),
            cell.y,
            &temperature,
            usize::from(cell.right().saturating_sub(gauge.right())),
            context.theme.temperature_style(&core.temperature),
        );
    }
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
    for labeled in cpu.package_power.iter().chain(&cpu.power) {
        output.label(cell_label(
            &format!("cpu power {}", clean(&labeled.label)),
            &labeled.cell,
            context,
        ));
    }
    for group in &cpu.frequencies {
        if frequency_cap(group).is_some() {
            output.label(cell_label(
                &format!("cpu frequency limit of {} cores", group.cores),
                &group.limit,
                context,
            ));
        }
    }
    output.label(cell_label("cpu load 1m", &cpu.load[0], context));
    if let Some(pressure) = &cpu.pressure {
        pressure_labels(output, "cpu", pressure, context);
    }
    output.label(format!("cpu cores: {}", cpu.cores.len()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures;
    use dgxtop_core::metric::{CanonicalUnit, Scalar};
    use dgxtop_core::view::CellState;

    fn megahertz(value: Option<f64>) -> MetricCell {
        value.map_or_else(
            || MetricCell::missing(CanonicalUnit::MHz, CellState::Unavailable),
            |mhz| MetricCell {
                value: Scalar::from_f64(mhz).ok(),
                ..MetricCell::missing(CanonicalUnit::MHz, CellState::Ok)
            },
        )
    }

    /// A kind of core nothing caps: its limit is its maximum.
    fn group(current: Option<f64>, maximum: Option<f64>) -> FrequencyGroup {
        capped(current, maximum, maximum)
    }

    fn capped(current: Option<f64>, limit: Option<f64>, maximum: Option<f64>) -> FrequencyGroup {
        FrequencyGroup {
            maximum: megahertz(maximum),
            limit: megahertz(limit),
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

    /// A kind of core the cpufreq policy caps shows the limit it runs
    /// under, in the warning color, before what the hardware can do
    /// (D-136); a kind nothing caps keeps `current/maximum`. A limit and a
    /// maximum a tenth of a GHz cannot tell apart get the decimals that
    /// do, and a driver that reports no limit changes nothing.
    #[test]
    fn a_capped_kind_shows_its_limit_before_the_hardware_maximum() {
        let context = test_fixtures::display_context();
        let groups = [
            group(Some(2100.0), Some(2808.0)),
            capped(Some(2900.0), Some(3000.0), Some(3900.0)),
        ];
        assert_eq!(frequency_text(&groups), "2.1/2.8 GHz · 2.9/3.0 (3.9) GHz");
        let spans = frequency_spans(&groups, &context);
        let limit = spans.iter().find(|span| span.content == "3.0").unwrap();
        assert_eq!(limit.style.fg, Some(context.theme.warning));
        assert_eq!(
            frequency_text(&[capped(Some(3800.0), Some(3850.0), Some(3900.0))]),
            "3.8/3.85 (3.90) GHz"
        );
        assert_eq!(
            frequency_text(&[capped(Some(3400.0), None, Some(3900.0))]),
            "3.4/3.9 GHz"
        );
        // Without a maximum the limit is the top the cores can reach.
        assert_eq!(
            frequency_text(&[capped(Some(1800.0), Some(2000.0), None)]),
            "1.8/2.0 GHz"
        );
        assert_eq!(frequency_cap(&group(Some(3400.0), Some(3900.0))), None);
    }

    /// The CPU line writes its temperature in its level color (D-104) and
    /// its package power after `⚡` only while it has a value (D-106); the
    /// platform's power is never taken for the CPU's.
    #[test]
    fn the_summary_colors_the_temperature_and_shows_power_only_when_read() {
        let context = test_fixtures::display_context();
        let mut cpu = test_fixtures::cpu_view();
        let line = summary_line(&cpu, &context);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.starts_with("68°C │ ⚡45.2 W"), "{text}");
        assert_eq!(
            line.spans[0].style.fg,
            Some(context.theme.temperature_color(68.0))
        );
        for state in [
            CellState::Unavailable,
            CellState::ReadFailed,
            CellState::Stale,
        ] {
            cpu.package_power[0].cell = MetricCell::missing(CanonicalUnit::Watts, state);
            let text: String = summary_line(&cpu, &context)
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert!(
                !text.contains('⚡') && !text.contains(" W"),
                "{state:?}: {text}"
            );
        }
        cpu.package_power.clear();
        let text: String = summary_line(&cpu, &context)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            !text.contains("61.5"),
            "the platform is not the CPU: {text}"
        );
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

    /// The title names the CPU (an arm64 CPU by its kinds of cores) and
    /// keeps a shorter form of its keys where the full one no longer fits
    /// beside the name.
    #[test]
    fn the_title_names_the_cpu_and_keeps_its_keys() {
        let context = test_fixtures::display_context();
        let mut model = test_fixtures::view_model(dgxtop_core::commands::ViewKind::Overview, 1);
        let mut cpu = test_fixtures::cpu_view();
        cpu.model = Some("10× Cortex-A725 + 10× Cortex-X925".to_owned());
        model.modules.cpu = Some(std::sync::Arc::new(cpu));
        let module = CpuModule::new().unwrap();
        let title_row = |width: u16| -> String {
            let height = module
                .preferred_rows(&model, width, Slot::Overview)
                .unwrap();
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    module.render(&context, &model, frame.area(), frame);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..width).map(|x| buffer[(x, 0)].symbol()).collect()
        };
        let wide = title_row(80);
        assert!(
            wide.contains(" CPU ▸ 10× Cortex-A725 + 10× Cortex-X925 "),
            "{wide}"
        );
        assert!(wide.contains("(c/click cores)"), "{wide}");
        // The overview's CPU panel on a 136-column terminal (D-40).
        let narrow = title_row(57);
        assert!(
            narrow.contains(" CPU ▸ 10× Cortex-A725 + 10× Cortex-X925 "),
            "{narrow}"
        );
        assert!(narrow.contains("(c cores)"), "{narrow}");
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

    /// The CPU's stall pressure follows the breakdown on its line where
    /// both fit, else on the next one (D-135); it has no `full`. A kernel
    /// without PSI keeps the lines it had.
    #[test]
    fn the_cpu_pressure_follows_the_breakdown() {
        let context = test_fixtures::display_context();
        let texts = |cpu: &CpuView, width: u16| -> Vec<String> {
            text_lines(cpu, width, &context)
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect()
                })
                .collect()
        };
        let mut cpu = test_fixtures::cpu_view();
        let without = texts(&cpu, 200);
        cpu.pressure = Some(test_fixtures::pressure_view([1.5, 0.7, 0.2], None));
        let wide = texts(&cpu, 200);
        assert_eq!(wide.len(), without.len());
        assert!(
            wide[1].ends_with("│ pressure some 1.5% 0.7% 0.2%  (10s 1m 5m)"),
            "{wide:?}"
        );
        assert!(wide[1].starts_with("■ usr"), "{wide:?}");
        let narrow = texts(&cpu, 60);
        assert_eq!(narrow[2], "pressure some 1.5% 0.7% 0.2%  (10s 1m 5m)");
        assert!(!narrow[1].contains("pressure"), "{narrow:?}");
        assert!(narrow.iter().all(|line| !line.contains("full")));
    }

    /// While no task waited for a CPU, every average reads `0.0%` and the
    /// pressure is left out (D-143): the lines are those of a kernel
    /// without PSI.
    #[test]
    fn a_cpu_pressure_of_nothing_is_left_out() {
        let context = test_fixtures::display_context();
        let texts = |cpu: &CpuView| -> Vec<String> {
            text_lines(cpu, 200, &context)
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect()
                })
                .collect()
        };
        let mut cpu = test_fixtures::cpu_view();
        let without = texts(&cpu);
        cpu.pressure = Some(test_fixtures::pressure_view([0.0, 0.0, 0.04], None));
        assert_eq!(texts(&cpu), without);
        cpu.pressure = Some(test_fixtures::pressure_view([0.0, 0.0, 0.05], None));
        assert!(texts(&cpu)[1].contains("pressure some 0.0% 0.0% 0.1%"));
    }

    /// Each load average is written in the level color of that many CPUs
    /// busy (D-143): of eight CPUs a load of four is the amber of a
    /// half-full bar, one of eight and more red. Without a count of the
    /// CPUs, or where it is not read, it keeps its tone.
    #[test]
    fn the_load_takes_the_level_color_of_that_many_cpus_busy() {
        let context = test_fixtures::display_context();
        let theme = &context.theme;
        let load = |value: f64| test_fixtures::ok(value, CanonicalUnit::Count);
        let colors = |cpu: &CpuView| -> Vec<Option<Color>> {
            let line = load_line(cpu, &context);
            // `load `, then each average with a space between.
            [1, 3, 5]
                .into_iter()
                .map(|index| line.spans[index].style.fg)
                .collect()
        };
        let mut cpu = test_fixtures::cpu_view();
        assert_eq!(cpu.cores.len(), 8);
        cpu.load = [load(1.0), load(4.0), load(8.0)];
        assert_eq!(
            colors(&cpu),
            vec![
                Some(theme.level_color(0.125)),
                Some(theme.gauge_mid),
                Some(theme.gauge_high),
            ]
        );
        cpu.load[0] = load(40.0);
        assert_eq!(
            colors(&cpu)[0],
            Some(theme.gauge_high),
            "more tasks than CPUs"
        );
        cpu.load[1] = MetricCell::missing(CanonicalUnit::Count, CellState::ReadFailed);
        assert_eq!(
            colors(&cpu)[1],
            theme.tone_style(cell_tone(&cpu.load[1])).fg,
            "a load that is not read keeps its tone"
        );
        cpu.cores.clear();
        assert_eq!(colors(&cpu)[2], Some(theme.text), "no CPUs counted");
    }

    /// Where the package's temperature is its hottest cluster's, the CPU
    /// line names that cluster after it (D-137), muted, unless that would
    /// push the load off the line or the line past the panel; a package a
    /// sensor reports itself, or the `SoC`'s temperature in its place, names
    /// none.
    #[test]
    fn the_cpu_line_names_the_hottest_cluster_where_it_fits() {
        let context = test_fixtures::display_context();
        let first = |cpu: &CpuView, width: u16| -> String {
            text_lines(cpu, width, &context)[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        let mut cpu = test_fixtures::cpu_view();
        cpu.package_power.clear();
        let plain = first(&cpu, 200);
        assert!(plain.starts_with("68°C │ "), "{plain}");
        cpu.hottest_cluster = Some("cluster 0p".to_owned());
        let named = first(&cpu, 200);
        assert!(named.starts_with("68°C cluster 0p │ "), "{named}");
        assert!(named.contains("load "), "{named}");
        let line = named_summary_line(&cpu, &context).unwrap();
        assert_eq!(line.spans[1].content, " cluster 0p");
        assert_eq!(line.spans[1].style, context.theme.muted_style());
        // A width that holds the load only without the cluster keeps the
        // load on the line.
        let joined = u16::try_from(plain.chars().count()).unwrap();
        let tight = first(&cpu, joined);
        assert!(
            tight.starts_with("68°C │ ") && tight.contains("load "),
            "{tight}"
        );
        // Too narrow for the load either way: the cluster is named while
        // the line itself fits.
        let narrow = first(&cpu, 34);
        assert!(narrow.starts_with("68°C cluster 0p │ "), "{narrow}");
        assert!(!narrow.contains("load "), "{narrow}");
        // The SoC's temperature standing in for a missing package names none.
        cpu.package_temperatures.clear();
        assert!(!first(&cpu, 200).contains("cluster"));
    }
}
