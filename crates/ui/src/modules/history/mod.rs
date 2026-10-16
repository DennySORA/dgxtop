//! History module (D-46, D-48, D-50): the overview's resources over a
//! longer span, laid out like a monitoring dashboard ([`layout`]):
//!
//! - the bar-size row: the bar sizes `←`/`→` (and `w`/`W`) step through
//!   (D-78), on a host of several GPUs the GPU the GPU panels show, `↑`/`↓`
//!   (D-107), how much of the charts' span is recorded (history starts with
//!   dgxtop) and the panels above or below the ones shown, which
//!   `PgUp`/`PgDn` scroll to;
//! - the panels ([`panels`]) in bands, two side by side on a wide terminal:
//!   the CPU and the memory, the GPU's utilization and memory, its
//!   temperature and power, the disk and the network. Each panel is a title
//!   over its strips ([`strip`]): a gutter with the scale, one block column
//!   a bar, a legend with the value now, the mean and the peak;
//! - the time axis under each column of panels ([`axis`]).
//!
//! The GPUs' 1 h to 24 h table of D-48 is gone (D-107): the charts and
//! their bar sizes show the same.
//!
//! Its other pages are the Process History tab ([`runs`], D-75, D-76).

mod axis;
mod layout;
mod panels;
mod runs;
mod strip;

pub(crate) use self::panels::history_gpus;

use dgxtop_core::identity::EntityId;
use dgxtop_core::tiers::BarSize;
use dgxtop_core::view::{DiskHistory, HISTORY_GUTTER_COLUMNS, HistoryView, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use self::axis::{Tick, clock_ticks, render_axis};
use self::layout::{HistoryLayout, plan_folded};
use self::panels::{Panel, Strip, panels, shown_gpu};
use self::runs::{opened_run_pages, render_runs};
use self::strip::{Window, plot_width, render_strip, strip_text, tallest_column};
use super::ModuleBuildError;
use super::common::{
    ManifestSpec, Output, RowCursor, build_manifest, draw_caption, draw_line, fitting_keys, gap,
    key_spans, message, no_data, panel_with_keys,
};
use super::folds::{fold_keys, render_folded};
use crate::format::{CellTone, clean, counted, display_width, format_span_ns};
use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, RenderOutput, Slot, controls,
};

/// Module id of the History view, whose pages are the Overview History and
/// Process History tabs (D-75).
pub const HISTORY_MODULE_ID: &str = "history";

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
        let several_gpus = model
            .modules
            .history
            .as_deref()
            .is_some_and(|history| history_gpus(history).len() > 1);
        // An opened run's charts on pages name the keys turning them.
        let run_paged = context.tab > 1
            && model
                .modules
                .process_history
                .as_deref()
                .is_some_and(|runs| {
                    let inner = ratatui::widgets::Block::bordered().inner(area);
                    opened_run_pages(runs, inner, context) > 1
                });
        // The runs' filter shows in the list's title (D-113).
        let filter = clean(model.session.filter.trim());
        let list_title = if filter.is_empty() {
            "Process History".to_owned()
        } else {
            format!("Process History · /{filter}")
        };
        let (title, keys): (&str, &[&str]) = match context.tab {
            0 if several_gpus => ("Overview History", BAR_SIZE_AND_GPU_KEYS),
            0 => ("Overview History", BAR_SIZE_KEYS),
            1 => (&list_title, PROCESS_LIST_KEYS),
            _ if run_paged => ("Process History · run", PROCESS_DETAIL_PAGE_KEYS),
            _ => ("Process History · run", PROCESS_DETAIL_KEYS),
        };
        let keys = fitting_keys(area.width, title, keys);
        let inner = panel_with_keys(frame, area, title, keys, context);
        if context.tab > 0 {
            match model.modules.process_history.as_deref() {
                Some(runs) => render_runs(
                    frame,
                    inner,
                    (runs, (&self.manifest, model)),
                    context,
                    &mut output,
                ),
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
        let gpu = shown_gpu(history, context.picked.as_ref());
        render_history(
            frame,
            inner,
            (history, gpu.as_ref()),
            (&self.manifest, model),
            context,
            &mut output,
        );
        output.finish()
    }
}

/// The bar-size row, the panels from the band scrolled to, and the axis
/// (module docs); the GPU panels of `gpu` alone when one is given.
fn render_history(
    frame: &mut Frame<'_>,
    inner: Rect,
    (history, gpu): (&HistoryView, Option<&EntityId>),
    (manifest, model): (&DisplayModuleManifest, &ViewModel),
    context: &DisplayContext,
    output: &mut Output,
) {
    // Folded panels keep their title row, first, and give the others
    // their rows; the others pair up as if they were not there (D-114).
    let (folded, open): (Vec<Panel<'_>>, Vec<Panel<'_>>) = panels(history, gpu)
        .into_iter()
        .partition(|panel| is_folded(panel, context));
    let folded_count = folded.len();
    let panels: Vec<Panel<'_>> = folded.into_iter().chain(open).collect();
    let strips: Vec<u16> = panels
        .iter()
        .map(|panel| saturate_u16(panel.strips.len()))
        .collect();
    let layout = plan_folded(
        inner.width,
        inner.height,
        (&strips, folded_count),
        usize::from(context.offset),
    );
    let mut rows = RowCursor::new(inner);
    if let Some(row) = rows.next_row() {
        let hidden = Hidden {
            above: layout.panels_above,
            below: layout.panels_below,
            scrolls: true,
        };
        let picker = GpuPicker {
            gpus: history_gpus(history),
            shown: gpu,
        };
        render_bar_size_row(frame, row, (history, &picker), hidden, context);
        publish_picker(output, (manifest, model), (row, context), &picker);
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
        (window, Some((manifest, model))),
        context,
        output,
    );
    if let Some(axis) = rows.next_row() {
        // The bars follow the local clock (D-82), so the axis reads it.
        let span = (utc_i64(window.start_ns), utc_i64(window.end_ns));
        render_axes(
            frame,
            axis,
            &layout,
            |width| clock_ticks(span, history.utc_offset_s, width),
            context,
        );
        // Tells the controller the bands above and below, so the keys
        // scroll from what this frame showed.
        for _ in 0..layout.bands_above {
            output.target(model, manifest, controls::PAGE_ABOVE, axis);
        }
        for _ in 0..layout.bands_below {
            output.target(model, manifest, controls::PAGE_BELOW, axis);
        }
    }
}

/// The GPUs of the bar-size row's picker and the one shown (D-107).
struct GpuPicker<'a> {
    gpus: Vec<(EntityId, String)>,
    shown: Option<&'a EntityId>,
}

impl GpuPicker<'_> {
    /// `[GPU 0]  GPU 1 `: each GPU's chip, the shown one marked; none on a
    /// host of one GPU.
    fn chips(&self) -> Vec<(String, bool)> {
        if self.gpus.len() < 2 {
            return Vec::new();
        }
        self.gpus
            .iter()
            .map(|(gpu, label)| {
                let picked = self.shown == Some(gpu);
                let text = if picked {
                    format!("[{label}]")
                } else {
                    format!(" {label} ")
                };
                (text, picked)
            })
            .collect()
    }
}

/// Where the bar-size row draws the picker: after the sizes and a gap.
fn picker_x(row: Rect, context: &DisplayContext) -> u16 {
    let sizes: usize = BarSize::fixed()
        .map(|size| display_width(size.token()) + 2)
        .sum();
    row.x
        .saturating_add(saturate_u16(sizes))
        .saturating_add(saturate_u16(2 * gap(context).width()))
}

/// Each chip of the picker as a click target picking its GPU.
fn publish_picker(
    output: &mut Output,
    (manifest, model): (&DisplayModuleManifest, &ViewModel),
    (row, context): (Rect, &DisplayContext),
    picker: &GpuPicker<'_>,
) {
    let mut x = picker_x(row, context);
    for ((text, _), (gpu, _)) in picker.chips().iter().zip(&picker.gpus) {
        let width = saturate_u16(display_width(text));
        if x.saturating_add(width) > row.right() {
            break;
        }
        output.entity_target(
            model,
            manifest,
            controls::HISTORY_GPU,
            Rect::new(x, row.y, width, 1),
            gpu.clone(),
        );
        x = x.saturating_add(width);
    }
}

/// The bands of `layout`, each its panels side by side, from the rows'
/// next row on.
fn render_bands(
    frame: &mut Frame<'_>,
    rows: &mut RowCursor,
    (layout, panels): (&HistoryLayout, &[Panel<'_>]),
    (window, frame_of): (Window, Option<(&DisplayModuleManifest, &ViewModel)>),
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
            let panel = &panels[position];
            // A folded panel keeps its title row, which a click unfolds.
            if let (Some(fold), Some(frame_of)) = (panel.fold, frame_of)
                && is_folded(panel, context)
            {
                render_folded(
                    frame,
                    panel_area,
                    (&panel.title, fold),
                    frame_of,
                    context,
                    output,
                );
                continue;
            }
            render_panel(frame, panel_area, panel, window, context);
            for strip in &panel.strips {
                output.label(format!("history {}: {}", panel.title, strip_text(strip)));
            }
        }
    }
}

/// A panel's title rule, then its strips sharing the rows under it: the
/// band's strip height each, and more for a panel of fewer strips than the
/// other panel of its band (the power panel leads with the host's power,
/// D-106), so no rows stay blank under it.
fn render_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    panel: &Panel<'_>,
    window: Window,
    context: &DisplayContext,
) {
    let mut rows = RowCursor::new(area);
    if let Some(row) = rows.next_row() {
        let error = panel.strips.iter().find_map(|strip| strip.series.error());
        draw_line(
            frame.buffer_mut(),
            row,
            &title_rule((&panel.title, panel.fold), error, row.width, context),
        );
    }
    let alone = panel.strips.len() == 1;
    let rate_scale = panel
        .is_rate()
        .then(|| tallest_column(&panel.strips, window, plot_width(area.width)))
        .flatten();
    let shares = strip_shares(rows.remaining(), panel.strips.len());
    for (strip, strip_rows) in panel.strips.iter().zip(shares) {
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

/// `rows` shared by `strips` strips, the first ones a row taller while
/// some are left over.
fn strip_shares(rows: u16, strips: usize) -> Vec<u16> {
    let count = saturate_u16(strips).max(1);
    let (each, extra) = (rows / count, rows % count);
    (0..count)
        .map(|index| each + u16::from(index < extra))
        .collect()
}

/// `GPU power ────── (p fold)`: the title, why a query failed, a rule to
/// the panel's edge and the key folding the panel away (D-114), left out
/// when the row has no room for it.
fn title_rule(
    (title, fold): (&str, Option<&str>),
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
    let keys = fold.map(fold_keys).filter(|keys| !keys.is_empty());
    let mut tail = keys.map_or_else(Vec::new, |keys| key_spans(&keys, context));
    let used: usize = spans.iter().chain(&tail).map(Span::width).sum();
    if used + 4 > usize::from(width) {
        tail.clear();
    }
    let used: usize = spans.iter().chain(&tail).map(Span::width).sum();
    let rule = usize::from(width).saturating_sub(used + 1);
    spans.push(Span::raw(" "));
    spans.push(Span::styled("─".repeat(rule), theme.border_style(false)));
    spans.extend(tail);
    Line::from(spans)
}

/// Whether the user folded `panel` (D-114).
fn is_folded(panel: &Panel<'_>, context: &DisplayContext) -> bool {
    panel.fold.is_some_and(|fold| context.folded.contains(fold))
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

/// The panels the view could not show: above the ones shown (scrolled
/// past) and below them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Hidden {
    above: usize,
    below: usize,
    /// The page scrolls to them (the Overview History, D-107); else they
    /// wait for a taller terminal (a run's charts).
    scrolls: bool,
}

impl Hidden {
    /// What did not fit, with the keys reaching it (`^U`/`^D` stand in
    /// for PgUp/PgDn, D-111), longest first: `PgUp/^U 1 panel above ·
    /// PgDn/^D 2 panels below`, then `^U 1 · ^D 2 panels`, so both
    /// directions stay told; or `+2 panels on a taller terminal`.
    fn notes(self) -> Vec<String> {
        let panels = |count: usize| counted(count, "panel");
        match (self.scrolls, self.above, self.below) {
            (true, 0, 0) | (false, _, 0) => Vec::new(),
            (true, above, 0) => vec![
                format!("PgUp/^U {} above", panels(above)),
                format!("^U {} above", panels(above)),
            ],
            (true, 0, below) => vec![
                format!("PgDn/^D {} below", panels(below)),
                format!("^D {} below", panels(below)),
            ],
            (true, above, below) => vec![
                format!(
                    "PgUp/^U {} above · PgDn/^D {} below",
                    panels(above),
                    panels(below)
                ),
                format!("^U {above} · ^D {}", panels(below)),
            ],
            (false, _, below) => vec![format!("+{} on a taller terminal", panels(below))],
        }
    }
}

/// The keys acting on the view, at the right of its title (D-70), the
/// fullest that fits first: the letters stand in for the arrows on a
/// keyboard without them (D-111).
const BAR_SIZE_KEYS: &[&str] = &["h/l/←/→ bar size", "h/l bar size"];
/// The same on a host of several GPUs (D-107). Each panel names its own
/// fold key (D-114).
const BAR_SIZE_AND_GPU_KEYS: &[&str] = &[
    "h/l/←/→ bar size · j/k/↑/↓ GPU",
    "h/l bar size · j/k GPU",
    "h/l bar size",
];
/// The keys of the process history's pages (D-72, D-75, D-108), the
/// fullest that fits first.
const PROCESS_LIST_KEYS: &[&str] = &[
    "j/k/↑/↓ select · g/G top/end · Space mark · a all · d delete · / filter · Enter open",
    "g top · Space mark · a all · d delete · / filter · Enter open",
    "Space mark · d delete · / filter · Enter open",
    "Space mark · d delete · Enter open",
];
/// The same when the run's charts take several pages (D-112).
const PROCESS_DETAIL_PAGE_KEYS: &[&str] = &[
    "PgDn/PgUp/^D/^U page · h/l/←/→ bar · Enter back · j/k/↑/↓ next · e command · y copy · d delete",
    "^D/^U page · h/l bar · Enter back · j/k next · e command · y copy · d delete",
    "^D/^U page · h/l bar · Enter back · y copy · d delete",
    "^D/^U page · Enter back",
];
const PROCESS_DETAIL_KEYS: &[&str] = &[
    "h/l/←/→ bar · Enter back · j/k/↑/↓ next · e command · y copy · d delete",
    "h/l bar · Enter back · j/k next · e command · y copy · d delete",
    "h/l bar · Enter back · e command · y copy · d delete",
    "h/l bar · Enter back · y copy · d delete",
    "Enter back · d delete",
];

/// ` 1m  5m  10m … 24h ` with the current size marked, the GPU picker on
/// a host of several, then at the right what did not fit and how much of
/// the charts' span is recorded, as much as fits.
fn render_bar_size_row(
    frame: &mut Frame<'_>,
    row: Rect,
    (history, picker): (&HistoryView, &GpuPicker<'_>),
    hidden: Hidden,
    context: &DisplayContext,
) {
    let theme = &context.theme;
    let current = clean(&history.window_label);
    let mut spans = Vec::new();
    // The Overview History's bar sizes (D-78): every size but `auto`.
    for size in BarSize::fixed().map(BarSize::token) {
        let style = if size == current {
            theme.selected_row_style()
        } else {
            theme.muted_style()
        };
        spans.push(Span::styled(format!(" {size} "), style));
    }
    let chips = picker.chips();
    if !chips.is_empty() {
        spans.push(gap(context));
        spans.push(gap(context));
        for (text, picked) in chips {
            let style = if picked {
                theme.selected_row_style()
            } else {
                theme.label_style()
            };
            spans.push(Span::styled(text, style));
        }
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

/// The bar-size row's notes in the order tried: each form of what did not
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

/// A wall-clock time of the view as the axis takes it.
fn utc_i64(utc_ns: u64) -> i64 {
    i64::try_from(utc_ns).unwrap_or(i64::MAX)
}

/// Seconds of `nanos`.
pub(super) fn nanos_to_seconds(nanos: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let nanos = nanos as f64;
    nanos / 1e9
}

/// `1h 12m`, `45m`, `2h`, `30s`: recorded time, in whole minutes past the
/// first.
fn recorded_text(seconds: u64) -> String {
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
    use crate::test_fixtures::history_view;

    /// A panel of fewer strips than its band's other panel shares the
    /// band's rows among them (D-106): no blank rows under it, the first
    /// strips a row taller while rows are left over.
    #[test]
    fn strips_share_the_rows_under_their_title() {
        assert_eq!(strip_shares(9, 3), vec![3, 3, 3]);
        assert_eq!(strip_shares(9, 1), vec![9]);
        assert_eq!(strip_shares(10, 3), vec![4, 3, 3]);
        assert_eq!(strip_shares(11, 3), vec![4, 4, 3]);
        assert_eq!(strip_shares(0, 2), vec![0, 0]);
        assert_eq!(strip_shares(4, 0), vec![4]);
    }

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
        // The whole six-row strip: below 50 % is green only at its foot (D-87).
        let theme = crate::test_fixtures::display_context().theme;
        let strip_rows = u16::try_from(gpu + 1).unwrap()..u16::try_from(gpu + 7).unwrap();
        let colors: Vec<ratatui::style::Color> = strip_rows
            .flat_map(|y| (0..width / 2).map(move |x| (x, y)))
            .map(|cell| buffer[cell].fg)
            .collect();
        assert!(colors.contains(&theme.level_color(1.0)), "{text}");
        assert!(colors.contains(&theme.level_color(0.1)), "{text}");
    }

    /// The Process History list (D-76): the runs in the order the history
    /// gives (the running ones first, D-115), in local time, with each
    /// run's figures and how it ended.
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
        assert!(killed < training, "in the projection's order: {text}");
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
                bar: dgxtop_core::tiers::BarSize::Auto,
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

    /// Renders the opened run's page `offset` at `width` × `height`: its
    /// rows and what the frame published.
    fn run_page(offset: u16, (width, height): (u16, u16)) -> (Vec<String>, RenderOutput) {
        let mut context = crate::test_fixtures::display_context();
        context.tab = 2;
        context.offset = offset;
        let mut model =
            crate::test_fixtures::view_model(dgxtop_core::commands::ViewKind::History, 1);
        model.modules.process_history = Some(std::sync::Arc::new(
            crate::test_fixtures::process_history_view(),
        ));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        let mut output = None;
        terminal
            .draw(|frame| {
                output = Some(HistoryModule::new().unwrap().render(
                    &context,
                    &model,
                    frame.area(),
                    frame,
                ));
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows = (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        (rows, output.unwrap())
    }

    /// An opened run whose charts would squeeze their strips past their
    /// legends shows them on pages (D-112): the first under the run's
    /// details, the next under its identity line, which says the page and
    /// what the next holds; the title names the keys and the frame tells
    /// the controller the pages around. A tall panel keeps one page.
    #[test]
    fn a_runs_charts_go_on_pages_with_their_legends() {
        let count = |output: &RenderOutput, control| {
            output
                .hit_targets
                .iter()
                .filter(|target| target.control_id == control)
                .count()
        };
        let (first, output) = run_page(0, (213, 37));
        let text = first.join("\n");
        assert!(first[0].contains("^D/^U page"), "{text}");
        assert!(
            first[1].contains("page 1 of 2 · next: Tasks, Disk, Network"),
            "{text}"
        );
        assert!(text.contains("folder /home/u/code/ComfyUI"), "{text}");
        assert!(
            text.contains("GPU utilization") && !text.contains("Tasks ─"),
            "{text}"
        );
        assert_eq!(
            text.matches("last ").count(),
            7,
            "every strip shows its last value: {text}"
        );
        assert_eq!(
            (
                count(&output, controls::PAGE_ABOVE),
                count(&output, controls::PAGE_BELOW)
            ),
            (0, 1)
        );
        let (second, output) = run_page(1, (213, 37));
        let text = second.join("\n");
        assert!(
            second[1].contains("PID 42") && second[1].contains("page 2 of 2"),
            "{text}"
        );
        assert!(
            !text.contains("folder "),
            "the details stay on the first page: {text}"
        );
        assert!(
            text.contains("Tasks ─") && text.contains("Network · host ─"),
            "{text}"
        );
        assert_eq!(
            (
                count(&output, controls::PAGE_ABOVE),
                count(&output, controls::PAGE_BELOW)
            ),
            (1, 0)
        );
        let (past, _) = run_page(9, (213, 37));
        assert_eq!(past, second, "the last page at most");
        let (tall, output) = run_page(0, (213, 60));
        let text = tall.join("\n");
        assert!(!text.contains("page 1 of"), "{text}");
        assert!(
            !tall[0].contains("page ·") && !tall[0].contains("^D/^U"),
            "{text}"
        );
        assert_eq!(count(&output, controls::PAGE_BELOW), 0);
    }

    /// The marked runs carry a mark and the warning color, and their labels
    /// say so (D-108).
    #[test]
    fn marked_runs_carry_a_mark() {
        let view = crate::test_fixtures::process_history_view();
        let killed = view.runs[0].key.clone();
        let mut context = crate::test_fixtures::display_context();
        context.tab = 1;
        context.marked = std::sync::Arc::new(std::collections::BTreeSet::from([killed]));
        let mut model =
            crate::test_fixtures::view_model(dgxtop_core::commands::ViewKind::History, 1);
        model.modules.process_history = Some(std::sync::Arc::new(view));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 10)).unwrap();
        let mut output = None;
        terminal
            .draw(|frame| {
                output = Some(HistoryModule::new().unwrap().render(
                    &context,
                    &model,
                    frame.area(),
                    frame,
                ));
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..10)
            .map(|y| (0..120).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let text = rows.join("\n");
        let row_of = |name: &str| rows.iter().position(|row| row.contains(name)).unwrap();
        let (killed_row, training_row) = (row_of("ComfyUI/main"), row_of("acme_llm.train"));
        assert!(rows[killed_row].starts_with("│✓"), "{text}");
        assert!(!rows[training_row].contains('✓'), "{text}");
        let name_x = rows[killed_row].chars().position(|c| c == 'C').unwrap();
        assert_eq!(
            buffer[(
                u16::try_from(name_x).unwrap(),
                u16::try_from(killed_row).unwrap()
            )]
                .fg,
            context.theme.warning
        );
        let labels = output.unwrap().accessibility_labels;
        assert!(
            labels
                .iter()
                .any(|label| label.contains("ComfyUI/main") && label.ends_with(", marked")),
            "{labels:?}"
        );
        assert!(
            labels
                .iter()
                .any(|label| label.contains("acme_llm.train") && !label.contains("marked")),
            "{labels:?}"
        );
    }

    /// The keys at the right of the title: the fullest set that fits the
    /// width, down to the deletion and Enter (D-108).
    #[test]
    fn the_title_keys_fit_the_width() {
        let view = crate::test_fixtures::process_history_view();
        // Tall enough for an opened run's charts on one page.
        let title_row = |tab: u8, width: u16| {
            let terminal = render_runs_at(Some(view.clone()), (tab, 0, false), (width, 80));
            let buffer = terminal.backend().buffer();
            (0..width)
                .map(|x| buffer[(x, 0)].symbol())
                .collect::<String>()
        };
        for (width, keys) in [
            (
                120,
                "j/k/↑/↓ select · g/G top/end · Space mark · a all · d delete · / filter · Enter open",
            ),
            (
                90,
                "g top · Space mark · a all · d delete · / filter · Enter open",
            ),
            (70, "Space mark · d delete · / filter · Enter open"),
            (60, "Space mark · d delete · Enter open"),
        ] {
            let row = title_row(1, width);
            assert!(row.contains(keys), "{width}: {row}");
        }
        assert!(!title_row(1, 50).contains("delete"), "no room for any");
        for (width, keys) in [
            (
                120,
                "h/l/←/→ bar · Enter back · j/k/↑/↓ next · e command · y copy · d delete",
            ),
            (
                100,
                "h/l bar · Enter back · j/k next · e command · y copy · d delete",
            ),
            (90, "h/l bar · Enter back · e command · y copy · d delete"),
            (80, "h/l bar · Enter back · y copy · d delete"),
            (60, "Enter back · d delete"),
        ] {
            let row = title_row(2, width);
            assert!(row.contains(keys), "{width}: {row}");
        }
        // Its charts on pages: the keys turning them come first (D-112).
        let (paged, _) = run_page(0, (213, 37));
        assert!(
            paged[0].contains(
                "PgDn/PgUp/^D/^U page · h/l/←/→ bar · Enter back · j/k/↑/↓ next · e command · y copy · d delete"
            ),
            "{}",
            paged[0]
        );
        let (narrow, _) = run_page(0, (60, 37));
        assert!(
            narrow[0].contains("^D/^U page · Enter back"),
            "{}",
            narrow[0]
        );
    }

    /// The Overview History of a host of two GPUs: the bar-size row's
    /// picker marks the GPU the GPU panels show, each chip a target, and
    /// the axis row tells the controller the bands above and below
    /// (D-107).
    #[test]
    fn the_overview_history_shows_the_gpu_picked_from_the_band_scrolled_to() {
        let history = history_view();
        let gpus = history_gpus(&history);
        let mut context = crate::test_fixtures::display_context();
        context.picked = Some(gpus[1].0.clone());
        context.offset = 1;
        let mut model =
            crate::test_fixtures::view_model(dgxtop_core::commands::ViewKind::History, 1);
        model.modules.history = Some(std::sync::Arc::new(history));
        let (width, height) = (100, 16);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        let mut output = None;
        terminal
            .draw(|frame| {
                output = Some(HistoryModule::new().unwrap().render(
                    &context,
                    &model,
                    frame.area(),
                    frame,
                ));
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let text = rows.join("\n");
        assert!(rows[0].contains("↑/↓ GPU"), "{text}");
        assert!(rows[1].contains(" GPU 0 [GPU 1]"), "{text}");
        assert!(rows[1].contains("^U 1 · ^D 2 panels"), "{text}");
        assert!(!text.contains("CPU utilization"), "scrolled past: {text}");
        let output = output.unwrap();
        let count = |control: &str| {
            output
                .hit_targets
                .iter()
                .filter(|target| target.control_id == control)
                .count()
        };
        assert_eq!(count(controls::HISTORY_GPU), 2);
        assert_eq!(count(controls::PAGE_ABOVE), 1);
        assert!(count(controls::PAGE_BELOW) > 0, "{text}");
        let labels = output.accessibility_labels.join("\n");
        assert!(labels.contains("GPU 1: "), "{labels}");
        assert!(!labels.contains("GPU 0: "), "one GPU at a time: {labels}");
    }

    /// The runs' filter shows in the list's title, and a filter no run
    /// holds says so and how to clear it (D-113).
    #[test]
    fn the_run_filter_shows_in_the_list() {
        let draw = |filter: &str, runs: bool| {
            let mut context = crate::test_fixtures::display_context();
            context.tab = 1;
            let mut model =
                crate::test_fixtures::view_model(dgxtop_core::commands::ViewKind::History, 1);
            model.session.filter = filter.to_owned();
            let mut view = crate::test_fixtures::process_history_view();
            if !runs {
                view.runs.clear();
                view.opened = None;
            }
            model.modules.process_history = Some(std::sync::Arc::new(view));
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 8)).unwrap();
            terminal
                .draw(|frame| {
                    HistoryModule::new()
                        .unwrap()
                        .render(&context, &model, frame.area(), frame);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..8)
                .map(|y| {
                    (0..140)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let filtered = draw("comfy", true);
        assert!(filtered.contains("Process History · /comfy"), "{filtered}");
        assert!(filtered.contains("/ filter"), "{filtered}");
        let unfiltered = draw("", true);
        assert!(!unfiltered.contains("Process History · /"), "{unfiltered}");
        let none = draw("nothing", false);
        assert!(
            none.contains("no run's name or PID holds \"nothing\""),
            "{none}"
        );
        assert!(draw("", false).contains("no GPU process was seen yet"));
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
        assert!(detail[0].contains("h/l bar"), "{text}");
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
        // Its charts, page by page (D-112).
        let pages: String = (0..4)
            .map(|offset| run_page(offset, (120, 30)).0.join("\n"))
            .collect();
        for title in [
            "CPU · 20 CPUs",
            "Memory · 128 GiB",
            "GPU utilization",
            "GPU memory · 120 GiB",
            "Tasks",
            "Disk",
            "Network · host",
        ] {
            assert!(pages.contains(title), "missing {title}: {pages}");
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
            chart.covered_ns = Some(720 * SECOND);
        }
        assert_eq!(
            recorded_note(&history).as_deref(),
            Some("recorded 12m of 1h")
        );
        // The 24 h window on a machine up five hours: the note is of the
        // time recorded.
        history.requested_span_ns = 24 * 3_600 * SECOND;
        assert_eq!(
            recorded_note(&history).as_deref(),
            Some("recorded 12m of 24h")
        );
        history.requested_span_ns = 3_600 * SECOND;
        for chart in &mut history.charts {
            chart.covered_ns = Some(3_600 * SECOND);
        }
        assert_eq!(recorded_note(&history), None);
    }

    const SECOND: u64 = 1_000_000_000;

    /// The panels left out, in the longest form first: the Overview
    /// History says both ways it scrolls even in its short form (D-107); a
    /// run's charts wait for a taller terminal.
    #[test]
    fn the_hidden_panels_are_told_both_ways() {
        let scrolled = |above, below| {
            Hidden {
                above,
                below,
                scrolls: true,
            }
            .notes()
        };
        assert!(scrolled(0, 0).is_empty());
        assert_eq!(
            scrolled(0, 1),
            ["PgDn/^D 1 panel below", "^D 1 panel below"]
        );
        assert_eq!(
            scrolled(2, 0),
            ["PgUp/^U 2 panels above", "^U 2 panels above"]
        );
        assert_eq!(
            scrolled(1, 2),
            [
                "PgUp/^U 1 panel above · PgDn/^D 2 panels below",
                "^U 1 · ^D 2 panels"
            ]
        );
        let waiting = Hidden {
            above: 0,
            below: 3,
            scrolls: false,
        };
        assert_eq!(waiting.notes(), ["+3 panels on a taller terminal"]);
        assert!(Hidden::default().notes().is_empty());
    }

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
            bar_notes(Hidden::default(), recorded, why_partly(&DiskHistory::On))[0],
            "recorded 5h 12m of 24h · only while dgxtop runs"
        );
    }
}
