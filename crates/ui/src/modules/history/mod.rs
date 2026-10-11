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
//!
//! Its other pages are the Process History tab ([`runs`], D-75, D-76).

mod axis;
mod layout;
mod panels;
mod runs;
mod stats;
mod strip;

use dgxtop_core::view::{DiskHistory, HISTORY_GUTTER_COLUMNS, HistoryView, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use self::axis::{Tick, render_axis, ticks};
use self::layout::{HistoryLayout, plan};
use self::panels::{Panel, Strip, panels};
use self::runs::render_runs;
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

/// Module id of the History view, whose pages are the Overview History and
/// Process History tabs (D-75).
pub const HISTORY_MODULE_ID: &str = "history";

/// Bar sizes `w` and `W` step through, in order (D-78).
const WINDOWS: [&str; 12] = crate::controller::HISTORY_WINDOWS;
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
            _ => ("Process History · run", PROCESS_DETAIL_KEYS),
        };
        let inner = panel_with_keys(frame, area, title, keys, context);
        if context.tab > 0 {
            match model.modules.process_history.as_deref() {
                Some(runs) => render_runs(frame, inner, runs, context, &mut output),
                None => {
                    message(
                        frame,
                        inner,
                        &no_data("process history"),
                        context.theme.muted_style(),
                    );
                    output.label("process history: no data");
                }
            }
            return output.finish();
        }
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
    render_bands(
        frame,
        &mut rows,
        (&layout, &panels),
        window,
        context,
        output,
    );
    if let Some(axis) = rows.next_row() {
        // A window reaching back before boot starts at boot.
        let span_seconds = window.end_ns.saturating_sub(window.start_ns) / 1_000_000_000;
        render_axes(
            frame,
            axis,
            &layout,
            |width| ticks(span_seconds, width),
            context,
        );
    }
    let shown = &blocks[..layout.table_blocks];
    if let Some(table) = rows.take(layout.table_rows) {
        render_gpu_stats(frame, table, shown, context);
    }
    for label in stats_labels(shown) {
        output.label(label);
    }
}

/// The bands of `layout`, each its panels side by side, from the rows'
/// next row on.
fn render_bands(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    (layout, panels): (&HistoryLayout, &[Panel<'_>]),
    window: Window,
    context: &DisplayContext,
    output: &mut Output,
) {
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
        let error = panel.strips.iter().find_map(|strip| strip.series.error());
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

/// The time axis under each column of panels, `ticks` of a plot's width.
fn render_axes(
    frame: &mut Frame<'_>,
    row: Rect,
    layout: &HistoryLayout,
    ticks: impl Fn(u16) -> Vec<Tick>,
    context: &DisplayContext,
) {
    let columns = layout.bands.iter().map(Vec::len).max().unwrap_or(1);
    for column in 0..saturate_u16(columns) {
        let x = row.x + layout.panel_offset(column) + HISTORY_GUTTER_COLUMNS;
        let width = plot_width(layout.panel_width);
        render_axis(frame, row, (x, width), ticks(width), context);
    }
}

/// What the view could not show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
const WINDOW_KEYS: &str = "w/W/←/→ bar size";
/// The keys of the process history's pages (D-72, D-75).
const PROCESS_LIST_KEYS: &str = "j/k/↑/↓ select · Enter open";
const PROCESS_DETAIL_KEYS: &str = "w/W bar · Enter back · j/k/↑/↓ next · e command · y copy";

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

    fn draw_runs(
        view: Option<dgxtop_core::view::ProcessHistoryView>,
        tab: u8,
        scroll: u16,
        expanded: bool,
    ) -> Vec<String> {
        let terminal = render_runs_at(view, (tab, scroll, expanded), (120, 30));
        let buffer = terminal.backend().buffer();
        (0..30)
            .map(|y| (0..120).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    fn render_runs_at(
        view: Option<dgxtop_core::view::ProcessHistoryView>,
        (tab, scroll, expanded): (u8, u16, bool),
        (width, height): (u16, u16),
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        let mut context = crate::test_fixtures::display_context();
        context.tab = tab;
        context.scroll = scroll;
        context.expanded = expanded;
        let mut model =
            crate::test_fixtures::view_model(dgxtop_core::commands::ViewKind::History, 1);
        model.modules.process_history = view.map(std::sync::Arc::new);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                HistoryModule::new()
                    .unwrap()
                    .render(&context, &model, frame.area(), frame);
            })
            .unwrap();
        terminal
    }

    /// On a tall terminal each strip's legend has its last value, mean and
    /// peak, and the bars take the Overview History's level colors (D-77):
    /// the run's busiest minutes red against their scale.
    #[test]
    fn a_tall_run_page_has_full_legends_and_level_colors() {
        let view = crate::test_fixtures::process_history_view();
        let (width, height) = (213, 70);
        let terminal = render_runs_at(Some(view), (2, 0, false), (width, height));
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let text = rows.join("\n");
        for entry in ["last ", "avg ", "max "] {
            assert!(text.contains(entry), "missing {entry}: {text}");
        }
        assert!(
            !text.contains("now "),
            "an ended run has no value now: {text}"
        );
        let gpu = rows
            .iter()
            .position(|row| row.contains("GPU utilization"))
            .unwrap();
        let theme = crate::test_fixtures::display_context().theme;
        let strip_rows = u16::try_from(gpu + 1).unwrap()..u16::try_from(gpu + 4).unwrap();
        let colors: Vec<ratatui::style::Color> = strip_rows
            .flat_map(|y| (0..width / 2).map(move |x| (x, y)))
            .map(|cell| buffer[cell].fg)
            .collect();
        assert!(colors.contains(&theme.level_color(1.0)), "{text}");
        assert!(colors.contains(&theme.level_color(0.1)), "{text}");
    }

    /// The Process History list (D-76): the newest start first, in local
    /// time, with each run's figures and how it ended.
    #[test]
    fn the_process_history_lists_runs_newest_first() {
        let view = crate::test_fixtures::process_history_view();
        let list = draw_runs(Some(view), 1, 0, false);
        let text = list.join("\n");
        assert!(list[0].contains("Process History"), "{text}");
        assert!(list[0].contains("Enter open"), "{text}");
        assert!(
            list[1].contains("STARTED") && list[1].contains("STATUS"),
            "{text}"
        );
        let killed = list
            .iter()
            .position(|line| line.contains("ComfyUI/main"))
            .unwrap();
        let training = list
            .iter()
            .position(|line| line.contains("acme_llm.train"))
            .unwrap();
        assert!(killed < training, "newest start first: {text}");
        assert!(
            list[killed].contains("10-04 09:00"),
            "UTC+8: {}",
            list[killed]
        );
        assert!(
            list[killed].contains("SIGKILL by dgxtop"),
            "{}",
            list[killed]
        );
        assert!(list[training].contains("running"), "{}", list[training]);
        assert!(
            list[training].contains("GPU 0, GPU 1"),
            "{}",
            list[training]
        );
        let empty = draw_runs(
            Some(dgxtop_core::view::ProcessHistoryView {
                runs: Vec::new(),
                more_runs: 0,
                opened: None,
                kept_on_disk: true,
                utc_offset_s: 0,
                bar: dgxtop_core::process_history::RunBar::Auto,
                cpu_scale: None,
            }),
            1,
            0,
            false,
        )
        .join("\n");
        assert!(empty.contains("no GPU process was seen yet"), "{empty}");
        assert!(
            draw_runs(None, 1, 0, false)
                .join("\n")
                .contains("no process history data")
        );
    }

    /// An opened run: identity, times, folder, program, parent, its
    /// command (one line, or all of it expanded) and its charts laid out
    /// like the Overview History's, as bars of the session's size.
    #[test]
    fn an_opened_run_shows_everything_known_of_it() {
        let view = crate::test_fixtures::process_history_view();
        let detail = draw_runs(Some(view.clone()), 2, 0, false);
        let text = detail.join("\n");
        assert!(detail[0].contains("Process History · run"), "{text}");
        assert!(detail[0].contains("y copy"), "{text}");
        assert!(detail[0].contains("w/W bar"), "{text}");
        assert!(
            text.contains("PID 42 │ ComfyUI/main │ GPU 0 │ SIGKILL by dgxtop"),
            "{text}"
        );
        assert!(text.contains("started 2026-10-04 09:00:00"), "{text}");
        assert!(text.contains("ended 2026-10-04 09:30:00"), "{text}");
        assert!(text.contains("ran 30m"), "{text}");
        assert!(text.contains("folder /home/u/code/ComfyUI"), "{text}");
        assert!(text.contains("program /venv/bin/python3.12"), "{text}");
        assert!(
            text.contains("parent 7 bash │ nice 0 │ cgroup /user.slice/"),
            "{text}"
        );
        let command_rows = detail
            .iter()
            .filter(|line| line.contains("--preview"))
            .count()
            + detail
                .iter()
                .filter(|line| line.trim_start().starts_with("command"))
                .count();
        assert!(command_rows >= 1, "{text}");
        for title in [
            "CPU · 20 CPUs",
            "Memory · 128 GiB",
            "GPU utilization",
            "GPU memory · 120 GiB",
            "Tasks",
            "Disk",
            "Network · host",
        ] {
            assert!(text.contains(title), "missing {title}: {text}");
        }
        assert!(text.contains(" auto ") && text.contains(" 18h "), "{text}");
        assert!(text.contains("auto: 1m a bar · 1h 20m shown"), "{text}");
        assert!(text.contains("max 2000%"), "{text}");
        assert!(text.contains("09:30"), "the local clock's axis: {text}");
        let expanded = draw_runs(Some(view.clone()), 2, 0, true).join("\n");
        assert!(expanded.contains("--preview-method auto"), "{expanded}");
        // The opened run wherever the list has moved it; the cursor's run,
        // its history being read, while none is opened.
        let moved = draw_runs(Some(view.clone()), 2, 1, false).join("\n");
        assert!(moved.contains("PID 42 │"), "{moved}");
        let mut unopened = view;
        unopened.opened = None;
        let other = draw_runs(Some(unopened), 2, 1, false).join("\n");
        assert!(other.contains("PID 41"), "{other}");
        assert!(other.contains("reading this run"), "{other}");
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
