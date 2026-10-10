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
//! - the GPUs' long-term table over 1 h to 24 h ([`stats`]).

mod axis;
mod layout;
mod panels;
mod stats;
mod strip;

use dgxtop_core::view::{DiskHistory, HISTORY_GUTTER_COLUMNS, HistoryView, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use self::axis::render_axis;
use self::layout::{HistoryLayout, plan};
use self::panels::{Panel, panels};
use self::stats::{
    GpuStats, gpu_stats, nanos_to_seconds, recorded_text, render_gpu_stats, stats_labels,
};
use self::strip::{Window, plot_width, render_strip, strip_text};
use super::ModuleBuildError;
use super::common::{
    ManifestSpec, Output, RowCursor, build_manifest, draw_caption, draw_line, gap, message,
    no_data, panel,
};
use crate::format::{CellTone, clean, display_width, format_span_ns};
use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, RenderOutput, Slot, Topic,
};

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
                id: "history",
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
        let inner = panel(frame, area, "History", context);
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
        render_history(frame, inner, history, context, &mut output);
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
    let layout = plan(inner.width, inner.height, &strips, &block_rows);
    let mut rows = RowCursor::new(inner);
    if let Some(row) = rows.next_row() {
        let hidden = Hidden {
            panels: layout.hidden_panels,
            gpus: blocks.len() - layout.table_blocks,
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
    for strip in &panel.strips {
        let Some(strip_area) = rows.take(strip_rows) else {
            break;
        };
        render_strip(frame, strip_area, (strip, alone), window, context);
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
    let mut spans = vec![Span::styled(clean(title), theme.title_style())];
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
}

impl Hidden {
    /// What did not fit, longest first: `+2 panels · +1 GPU's long-term
    /// statistics on a taller terminal`, then shorter forms, the panels
    /// before the statistics.
    fn notes(self) -> Vec<String> {
        let panels = match self.panels {
            0 => None,
            1 => Some("+1 panel".to_owned()),
            panels => Some(format!("+{panels} panels")),
        };
        let gpus = match self.gpus {
            0 => None,
            1 => Some("+1 GPU's long-term statistics".to_owned()),
            gpus => Some(format!("+{gpus} GPUs' long-term statistics")),
        };
        let both: Vec<String> = panels.iter().chain(gpus.iter()).cloned().collect();
        let mut notes = Vec::new();
        if !both.is_empty() {
            notes.push(format!("{} on a taller terminal", both.join(" · ")));
        }
        if let Some(first) = panels.or(gpus) {
            notes.push(format!("{first} on a taller terminal"));
            notes.push(first);
        }
        notes.dedup();
        notes
    }
}

/// `5m [1h] 6h 12h 24h   w/W window`, then at the right what did not fit
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
    spans.push(gap(context));
    spans.push(Span::styled("w/W window", theme.muted_style()));
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
                Hidden { panels: 0, gpus: 0 },
                recorded,
                why_partly(&DiskHistory::On)
            )[0],
            "recorded 5h 12m of 24h · only while dgxtop runs"
        );
    }
}
