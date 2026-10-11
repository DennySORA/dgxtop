//! Source diagnostics: per-metric arbitration (selected/pinned source,
//! conflict spread vs tolerance, policy revision), every candidate with its
//! disposition, freshness, age, origin and rank, adapter host health
//! (with its last success and the backoff left), configured/effective
//! intervals and unclassified sensors.

use dgxtop_core::identity::EntityId;
use dgxtop_core::quality::{Freshness, SelectionState};
use dgxtop_core::view::{
    CandidateView, HostStatus, IntervalView, SourceMetricView, SourcesView, ViewModel,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::ModuleBuildError;
use super::common::{
    ColumnSpec, ManifestSpec, Output, RowCursor, TableCell, build_manifest, draw_line,
    draw_table_header, draw_table_row, fit_columns, message, no_data, panel, split_columns,
};
use super::host_status::{backoff_text, last_success_text};
use crate::format::{
    CONFLICT_SYMBOL, NOT_AVAILABLE, clean, fixed, format_age, format_interval_ms, format_scalar,
    kind_tag, quality_word,
};
use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, RenderOutput, Slot, Topic, controls,
};
use crate::theme::ThemePalette;

const WIDE: u16 = 120;

const METRIC_COLUMNS: [ColumnSpec; 6] = [
    ColumnSpec::left("", 1, 255),
    ColumnSpec::left("METRIC", 14, 100).flexible().prefer(26),
    ColumnSpec::left("STATE", 11, 95),
    ColumnSpec::left("SELECTED", 14, 80).prefer(22),
    ColumnSpec::left("PINNED", 10, 50).prefer(22),
    ColumnSpec::left("SPREAD/TOL", 13, 85),
];

const CANDIDATE_COLUMNS: [ColumnSpec; 11] = [
    ColumnSpec::left("SOURCE", 12, 100).flexible().prefer(22),
    ColumnSpec::left("ADAPTER", 8, 60),
    ColumnSpec::right("VALUE", 10, 95),
    ColumnSpec::left("NATIVE", 12, 20),
    ColumnSpec::left("QUALITY", 8, 85),
    ColumnSpec::left("KIND", 9, 55),
    ColumnSpec::left("FRESH", 13, 75),
    ColumnSpec::right("AGE", 9, 70),
    ColumnSpec::left("ORIGIN", 10, 45),
    ColumnSpec::right("RANK", 4, 40),
    ColumnSpec::left("DISPOSITION", 14, 98)
        .flexible()
        .prefer(28),
];

const HOST_COLUMNS: [ColumnSpec; 10] = [
    ColumnSpec::left("ADAPTER", 9, 100),
    ColumnSpec::left("STATE", 12, 99).flexible().prefer(35),
    ColumnSpec::right("GEN", 4, 40),
    ColumnSpec::right("PID", 7, 50),
    ColumnSpec::right("FAILS", 5, 70),
    ColumnSpec::right("RESTARTS", 8, 60),
    ColumnSpec::right("TIMEOUTS", 8, 55),
    ColumnSpec::right("LAST OK", 10, 45),
    ColumnSpec::right("BACKOFF", 7, 35),
    ColumnSpec::left("DETAIL", 10, 30).flexible(),
];
const NANOS_PER_MILLI: u64 = 1_000_000;

const INTERVAL_COLUMNS: [ColumnSpec; 5] = [
    ColumnSpec::left("ADAPTER", 9, 100),
    ColumnSpec::left("GROUP", 9, 90),
    ColumnSpec::right("CONFIGURED", 10, 95),
    ColumnSpec::right("EFFECTIVE", 10, 98),
    ColumnSpec::left("REASON", 10, 80).flexible().prefer(20),
];

const UNCLASSIFIED_COLUMNS: [ColumnSpec; 5] = [
    ColumnSpec::left("UNCLASSIFIED SOURCE", 19, 100)
        .flexible()
        .prefer(24),
    ColumnSpec::left("ADAPTER", 8, 60),
    ColumnSpec::right("VALUE", 10, 90),
    ColumnSpec::left("NATIVE", 12, 50),
    ColumnSpec::left("QUALITY", 8, 70),
];

pub struct SourcesModule {
    manifest: DisplayModuleManifest,
}

impl SourcesModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: "sources",
                title: "Source diagnostics",
                topics: &[Topic::Sources, Topic::Health],
                min_columns: 40,
                min_rows: 6,
                priority: 40,
                slots: &[Slot::Detail],
            })?,
        })
    }
}

/// Areas of the diagnostics page.
struct Areas {
    metrics: Rect,
    candidates: Rect,
    hosts: Rect,
    intervals: Rect,
    unclassified: Rect,
}

impl Areas {
    fn new(inner: Rect, sources: &SourcesView) -> Self {
        let candidate_rows =
            selected_metric(sources, None).map_or(0, |metric| metric.candidates.len() + 1);
        let mut rows = RowCursor::new(inner);
        let share = |items: usize, cap: u16| saturate_u16(items).saturating_add(1).min(cap.max(2));
        let budget = inner.height;
        let (metrics, candidates) = if inner.width >= WIDE {
            // Metrics and the selected metric's candidates side by side.
            let content = saturate_u16(sources.metrics.len().max(candidate_rows)).saturating_add(2);
            let top = rows.take(content.min(budget * 3 / 5)).unwrap_or_default();
            split_columns(top, top.width * 2 / 5)
        } else {
            let metrics = rows
                .take(share(sources.metrics.len(), budget / 3))
                .unwrap_or_default();
            let candidates = rows
                .take(share(candidate_rows, budget / 3))
                .unwrap_or_default();
            (metrics, candidates)
        };
        let hosts = rows
            .take(share(sources.hosts.len(), budget / 4))
            .unwrap_or_default();
        let intervals = rows
            .take(share(sources.intervals.len(), budget / 4))
            .unwrap_or_default();
        let unclassified = rows.rest().unwrap_or_default();
        Self {
            metrics,
            candidates,
            hosts,
            intervals,
            unclassified,
        }
    }
}

impl DisplayModule for SourcesModule {
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
        let Some(sources) = model.modules.sources.as_deref() else {
            let inner = panel(frame, area, "Sources", context);
            message(
                frame,
                inner,
                &no_data("sources"),
                context.theme.muted_style(),
            );
            output.label("sources: no data");
            return output.finish();
        };
        let mut title = format!("Sources · policy r{}", sources.policy_revision);
        if !sources.filter.is_empty() {
            title.push_str(&format!(" · filter \"{}\"", clean(&sources.filter)));
        }
        let inner = panel(frame, area, &title, context);
        let areas = Areas::new(inner, sources);
        let session_selection = model.session.selected_entity_id.as_ref();
        let selected = selected_metric(sources, session_selection);
        self.render_metrics(
            frame,
            areas.metrics,
            model,
            sources,
            selected,
            context,
            &mut output,
        );
        render_candidates(frame, areas.candidates, selected, context, &mut output);
        // The projection's clock moved on by its age since it was built.
        let now_boot_ns = model
            .generated_boot_ns
            .saturating_add(context.projection_age_ms.saturating_mul(NANOS_PER_MILLI));
        render_hosts(
            frame,
            areas.hosts,
            (&sources.hosts, now_boot_ns),
            context,
            &mut output,
        );
        render_intervals(
            frame,
            areas.intervals,
            &sources.intervals,
            context,
            &mut output,
        );
        render_unclassified(
            frame,
            areas.unclassified,
            &sources.unclassified,
            context,
            &mut output,
        );
        output.finish()
    }
}

/// Selected metric: projection selection, then session selector, then first.
fn selected_metric<'a>(
    sources: &'a SourcesView,
    session_selection: Option<&EntityId>,
) -> Option<&'a SourceMetricView> {
    let wanted = sources
        .selected
        .clone()
        .or_else(|| session_selection.map(|id| id.as_str().to_owned()));
    wanted
        .and_then(|selector| {
            sources
                .metrics
                .iter()
                .find(|metric| metric.key.series_selector() == selector)
        })
        .or_else(|| sources.metrics.first())
}

const fn selection_word(state: SelectionState) -> &'static str {
    match state {
        SelectionState::Selected => "selected",
        SelectionState::Degraded => "fallback",
        SelectionState::Conflict => "≠ conflict",
        SelectionState::Unavailable => "unavailable",
    }
}

const fn freshness_word(freshness: Freshness) -> &'static str {
    match freshness {
        Freshness::Fresh => "fresh",
        Freshness::Stale => "stale",
        Freshness::Unknown => "unknown",
    }
}

fn spread_text(metric: &SourceMetricView) -> String {
    match (metric.conflict_spread, metric.tolerance) {
        (Some(spread), Some(tolerance)) => {
            let relation = if spread > tolerance { ">" } else { "≤" };
            format!("{} {relation} {}", fixed(spread, 1), fixed(tolerance, 1))
        }
        (Some(spread), None) => format!("{} / tol n/a", fixed(spread, 1)),
        (None, Some(tolerance)) => format!("tol {}", fixed(tolerance, 1)),
        (None, None) => NOT_AVAILABLE.to_owned(),
    }
}

impl SourcesModule {
    #[allow(clippy::too_many_arguments)]
    fn render_metrics(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        model: &ViewModel,
        sources: &SourcesView,
        selected: Option<&SourceMetricView>,
        context: &DisplayContext,
        output: &mut Output,
    ) {
        if area.is_empty() {
            return;
        }
        if sources.metrics.is_empty() {
            message(frame, area, "no metrics match", context.theme.muted_style());
            return;
        }
        let palette = &context.theme;
        let columns = fit_columns(&METRIC_COLUMNS, area.width);
        let mut rows = RowCursor::new(area);
        if let Some(header) = rows.next_row() {
            draw_table_header(
                frame.buffer_mut(),
                header,
                &METRIC_COLUMNS,
                &columns,
                palette.header_style(),
            );
        }
        let selected_selector = selected.map(|metric| metric.key.series_selector());
        let position = selected_selector.as_ref().and_then(|selector| {
            sources
                .metrics
                .iter()
                .position(|metric| &metric.key.series_selector() == selector)
        });
        let capacity = usize::from(rows.remaining());
        let offset = match position {
            Some(position) if capacity > 0 && position >= capacity => position + 1 - capacity,
            _ => 0,
        };
        for metric in sources.metrics.iter().skip(offset).take(capacity) {
            let Some(row) = rows.next_row() else { break };
            let selector = metric.key.series_selector();
            let is_selected = selected_selector.as_deref() == Some(selector.as_str());
            let state_style = if metric.selection_state == SelectionState::Conflict {
                palette.tone_style(crate::format::CellTone::Conflict)
            } else if metric.selection_state.has_value() {
                palette.text_style()
            } else {
                palette.tone_style(crate::format::CellTone::Missing)
            };
            let cells = [
                TableCell::new(if is_selected { "▶" } else { "" }, palette.title_style()),
                TableCell::new(clean(&metric.label), palette.text_style()),
                TableCell::new(selection_word(metric.selection_state), state_style),
                TableCell::new(
                    metric
                        .selected_source
                        .as_deref()
                        .map_or_else(|| "none".to_owned(), clean),
                    palette.label_style(),
                ),
                TableCell::new(
                    metric
                        .pinned_source
                        .as_deref()
                        .map_or_else(|| "not pinned".to_owned(), clean),
                    palette.label_style(),
                ),
                TableCell::new(spread_text(metric), palette.label_style()),
            ];
            let style = is_selected.then(|| palette.selected_row_style());
            draw_table_row(frame.buffer_mut(), row, &columns, &cells, style);
            if let Ok(entity) = EntityId::new(selector) {
                output.entity_target(model, &self.manifest, controls::SOURCES_METRIC, row, entity);
            }
            output.label(format!(
                "source metric {}: {} · selected {} · pinned {} · spread/tolerance {} · policy r{}",
                clean(&metric.label),
                selection_word(metric.selection_state),
                metric
                    .selected_source
                    .as_deref()
                    .map_or_else(|| "none".to_owned(), clean),
                metric
                    .pinned_source
                    .as_deref()
                    .map_or_else(|| "not pinned".to_owned(), clean),
                spread_text(metric),
                metric.policy_revision
            ));
        }
    }
}

fn candidate_value(candidate: &CandidateView) -> String {
    match (candidate.value, candidate.unit) {
        (Some(value), Some(unit)) => format_scalar(value, unit),
        (Some(value), None) => format_scalar(value, dgxtop_core::metric::CanonicalUnit::Count),
        (None, _) => NOT_AVAILABLE.to_owned(),
    }
}

fn disposition_text(candidate: &CandidateView) -> String {
    let mut text = clean(&candidate.disposition);
    if !candidate.suspect_reasons.is_empty() {
        let reasons: Vec<String> = candidate
            .suspect_reasons
            .iter()
            .map(|reason| clean(reason))
            .collect();
        text.push_str(&format!(" ({})", reasons.join("; ")));
    }
    text
}

fn candidate_cells(candidate: &CandidateView, palette: &ThemePalette) -> [TableCell; 11] {
    let quality = quality_word(candidate.quality).unwrap_or("verified");
    let kind = kind_tag(candidate.kind).unwrap_or(match candidate.kind {
        dgxtop_core::quality::SampleKind::Derived => "derived",
        _ => "measured",
    });
    let fresh = format!(
        "{}/{}",
        freshness_word(candidate.freshness),
        freshness_word(candidate.measurement_freshness)
    );
    let age = candidate
        .age_ms
        .map_or_else(|| NOT_AVAILABLE.to_owned(), format_age);
    let disposition_style = if candidate.disposition.starts_with("selected") {
        palette.text_style().fg(palette.success)
    } else if candidate.disposition.starts_with("conflict") {
        palette.tone_style(crate::format::CellTone::Conflict)
    } else {
        palette.label_style()
    };
    [
        TableCell::new(clean(&candidate.source_id), palette.text_style()),
        TableCell::new(clean(&candidate.adapter_id), palette.label_style()),
        TableCell::new(candidate_value(candidate), palette.text_style()),
        TableCell::new(clean(&candidate.native), palette.muted_style()),
        TableCell::new(quality, palette.label_style()),
        TableCell::new(kind, palette.label_style()),
        TableCell::new(fresh, palette.label_style()),
        TableCell::new(age, palette.label_style()),
        TableCell::new(clean(&candidate.origin), palette.label_style()),
        TableCell::new(candidate.rank.to_string(), palette.label_style()),
        TableCell::new(disposition_text(candidate), disposition_style),
    ]
}

fn render_candidates(
    frame: &mut Frame<'_>,
    area: Rect,
    selected: Option<&SourceMetricView>,
    context: &DisplayContext,
    output: &mut Output,
) {
    if area.is_empty() {
        return;
    }
    let Some(metric) = selected else {
        message(
            frame,
            area,
            "select a metric to see its candidates",
            context.theme.muted_style(),
        );
        return;
    };
    let palette = &context.theme;
    let columns = fit_columns(&CANDIDATE_COLUMNS, area.width);
    let mut rows = RowCursor::new(area);
    if let Some(header) = rows.next_row() {
        draw_table_header(
            frame.buffer_mut(),
            header,
            &CANDIDATE_COLUMNS,
            &columns,
            palette.header_style(),
        );
    }
    let note_rows = u16::from(
        metric.switch_note.is_some() || metric.selection_state == SelectionState::Conflict,
    );
    let capacity = usize::from(rows.remaining().saturating_sub(note_rows));
    for candidate in metric.candidates.iter().take(capacity) {
        let Some(row) = rows.next_row() else { break };
        draw_table_row(
            frame.buffer_mut(),
            row,
            &columns,
            &candidate_cells(candidate, palette),
            None,
        );
        output.label(format!(
            "candidate {} for {}: value {} · {} · rank {} · {}",
            clean(&candidate.source_id),
            clean(&metric.label),
            candidate_value(candidate),
            quality_word(candidate.quality).unwrap_or("verified"),
            candidate.rank,
            disposition_text(candidate)
        ));
    }
    if metric.candidates.is_empty()
        && let Some(row) = rows.next_row()
    {
        frame.buffer_mut().set_stringn(
            row.x,
            row.y,
            "no candidates",
            usize::from(row.width),
            palette.muted_style(),
        );
    }
    if let Some(row) = rows.next_row() {
        let text = match (&metric.switch_note, metric.selection_state) {
            (Some(note), _) => format!("note: {}", clean(note)),
            (None, SelectionState::Conflict) => format!(
                "{CONFLICT_SYMBOL} independent sources disagree beyond tolerance ({}); no value is published",
                spread_text(metric)
            ),
            _ => String::new(),
        };
        draw_line(
            frame.buffer_mut(),
            row,
            &Line::from(Span::styled(text, palette.label_style())),
        );
    }
}

fn render_hosts(
    frame: &mut Frame<'_>,
    area: Rect,
    (hosts, now_boot_ns): (&[HostStatus], u64),
    context: &DisplayContext,
    output: &mut Output,
) {
    if area.is_empty() {
        return;
    }
    let palette = &context.theme;
    let columns = fit_columns(&HOST_COLUMNS, area.width);
    let mut rows = RowCursor::new(area);
    if let Some(header) = rows.next_row() {
        draw_table_header(
            frame.buffer_mut(),
            header,
            &HOST_COLUMNS,
            &columns,
            palette.header_style(),
        );
    }
    if hosts.is_empty()
        && let Some(row) = rows.next_row()
    {
        frame.buffer_mut().set_stringn(
            row.x,
            row.y,
            "no adapter hosts reported",
            usize::from(row.width),
            palette.muted_style(),
        );
    }
    for host in hosts {
        let Some(row) = rows.next_row() else { break };
        let state_style = match host.state {
            dgxtop_core::view::HostState::Healthy => palette.text_style().fg(palette.success),
            dgxtop_core::view::HostState::Starting | dgxtop_core::view::HostState::Disabled => {
                palette.label_style()
            }
            _ => palette.text_style().fg(palette.danger),
        };
        let cells = [
            TableCell::new(clean(&host.adapter_id), palette.text_style()),
            TableCell::new(host.state.label(), state_style),
            TableCell::new(host.generation.to_string(), palette.label_style()),
            TableCell::new(
                host.pid
                    .map_or_else(|| "none".to_owned(), |pid| pid.to_string()),
                palette.label_style(),
            ),
            TableCell::new(host.consecutive_failures.to_string(), palette.label_style()),
            TableCell::new(host.restarts_total.to_string(), palette.label_style()),
            TableCell::new(host.timeouts_total.to_string(), palette.label_style()),
            TableCell::new(last_success_text(host, now_boot_ns), palette.label_style()),
            TableCell::new(backoff_text(host, now_boot_ns), palette.label_style()),
            TableCell::new(
                host.detail.as_deref().map_or_else(String::new, clean),
                palette.muted_style(),
            ),
        ];
        draw_table_row(frame.buffer_mut(), row, &columns, &cells, None);
        output.label(format!(
            "adapter host {}: {}",
            clean(&host.adapter_id),
            host.state.label()
        ));
    }
}

fn render_intervals(
    frame: &mut Frame<'_>,
    area: Rect,
    intervals: &[IntervalView],
    context: &DisplayContext,
    output: &mut Output,
) {
    if area.is_empty() {
        return;
    }
    let palette = &context.theme;
    let columns = fit_columns(&INTERVAL_COLUMNS, area.width);
    let mut rows = RowCursor::new(area);
    if let Some(header) = rows.next_row() {
        draw_table_header(
            frame.buffer_mut(),
            header,
            &INTERVAL_COLUMNS,
            &columns,
            palette.header_style(),
        );
    }
    if intervals.is_empty()
        && let Some(row) = rows.next_row()
    {
        frame.buffer_mut().set_stringn(
            row.x,
            row.y,
            "no read groups reported",
            usize::from(row.width),
            palette.muted_style(),
        );
    }
    for interval in intervals {
        let Some(row) = rows.next_row() else { break };
        let throttled = interval.effective_ms != interval.configured_ms;
        let effective_style = if throttled {
            palette.tone_style(crate::format::CellTone::Fallback)
        } else {
            palette.text_style()
        };
        let cells = [
            TableCell::new(clean(&interval.adapter_id), palette.text_style()),
            TableCell::new(clean(&interval.read_group), palette.label_style()),
            TableCell::new(
                format_interval_ms(interval.configured_ms),
                palette.text_style(),
            ),
            TableCell::new(format_interval_ms(interval.effective_ms), effective_style),
            TableCell::new(clean(&interval.reason), palette.label_style()),
        ];
        draw_table_row(frame.buffer_mut(), row, &columns, &cells, None);
        output.label(format!(
            "interval {} {}: configured {} effective {} ({})",
            clean(&interval.adapter_id),
            clean(&interval.read_group),
            format_interval_ms(interval.configured_ms),
            format_interval_ms(interval.effective_ms),
            clean(&interval.reason)
        ));
    }
}

fn render_unclassified(
    frame: &mut Frame<'_>,
    area: Rect,
    unclassified: &[CandidateView],
    context: &DisplayContext,
    output: &mut Output,
) {
    if area.is_empty() || unclassified.is_empty() {
        return;
    }
    let palette = &context.theme;
    let columns = fit_columns(&UNCLASSIFIED_COLUMNS, area.width);
    let mut rows = RowCursor::new(area);
    if let Some(header) = rows.next_row() {
        draw_table_header(
            frame.buffer_mut(),
            header,
            &UNCLASSIFIED_COLUMNS,
            &columns,
            palette.header_style(),
        );
    }
    for candidate in unclassified {
        let Some(row) = rows.next_row() else { break };
        let cells = [
            TableCell::new(clean(&candidate.source_id), palette.text_style()),
            TableCell::new(clean(&candidate.adapter_id), palette.label_style()),
            TableCell::new(candidate_value(candidate), palette.text_style()),
            TableCell::new(clean(&candidate.native), palette.muted_style()),
            TableCell::new(
                quality_word(candidate.quality).unwrap_or("verified"),
                palette.label_style(),
            ),
        ];
        draw_table_row(frame.buffer_mut(), row, &columns, &cells, None);
        output.label(format!(
            "unclassified sensor {} ({}): {}",
            clean(&candidate.source_id),
            clean(&candidate.adapter_id),
            candidate_value(candidate)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures;

    /// The host table shows the last success and the backoff, on the
    /// projection's clock moved on by its age; a narrow one gives them up.
    #[test]
    fn hosts_show_last_success_and_backoff_by_width() {
        let model = test_fixtures::view_model(dgxtop_core::commands::ViewKind::Sources, 1);
        let module = SourcesModule::new().unwrap();
        let wide = test_fixtures::render_text(&module, &model, (200, 40));
        let header = wide
            .lines()
            .find(|line| line.contains("RESTARTS"))
            .expect("the host header");
        assert!(
            header.contains("LAST OK") && header.contains("BACKOFF"),
            "{header}"
        );
        let row = |name: &str| {
            wide.lines()
                .find(|line| line.trim_start_matches('│').starts_with(name))
                .unwrap_or_else(|| panic!("{name} row in {wide}"))
                .to_owned()
        };
        assert!(row("procfs").contains("3s ago"), "{wide}");
        let nvml = row("nvml");
        assert!(nvml.contains("never") && nvml.contains("7s"), "{nvml}");
        let narrow = test_fixtures::render_text(&module, &model, (80, 40));
        assert!(!narrow.contains("BACKOFF"), "{narrow}");
    }

    #[test]
    fn spread_text_compares_with_tolerance() {
        let sources = test_fixtures::sources_view();
        let conflict = sources
            .metrics
            .iter()
            .find(|metric| metric.selection_state == SelectionState::Conflict)
            .unwrap();
        assert_eq!(spread_text(conflict), "26.0 > 5.0");
    }
}
