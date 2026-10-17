//! History module (D-46, D-48, D-50): the overview's resources over a
//! longer span, laid out like a monitoring dashboard ([`layout`]):
//!
//! - the bar-size row: the bar sizes `←`/`→` (and `w`/`W`) step through
//!   (D-78), on a host of several GPUs the GPU the GPU panels show, `↑`/`↓`
//!   (D-107), and how much of the charts' span is recorded (history starts
//!   with dgxtop);
//! - while the panels do not all fit, a divider joined to the panel's
//!   borders (D-116) that says at its right the panels above and below the
//!   ones shown and the keys scrolling to them, `PgUp`/`PgDn`;
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
mod run_header;
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
use self::layout::{AXIS_ROWS, BAR_ROWS, HistoryLayout, plan_folded};
use self::panels::{HistoryGpu, Panel, Strip, panels, shown_gpu};
use self::runs::render_runs;
use self::strip::{Window, plot_width, render_strip, strip_text, tallest_column};
use super::ModuleBuildError;
use super::common::{
    DividerForm, ManifestSpec, Output, RowCursor, build_manifest, draw_caption, draw_divider,
    draw_line, fitting_keys, gap, key_spans, message, no_data, panel_with_keys,
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
            // The keys turning a run's pages are on its divider (D-116).
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

/// The bar-size row, a divider while the panels scroll, the panels from
/// the band scrolled to, and the axis (module docs); the GPU panels of
/// `gpu` alone when one is given.
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
    let band_rows = inner.height.saturating_sub(BAR_ROWS + AXIS_ROWS);
    let first = usize::from(context.offset);
    let mut layout = plan_folded(inner.width, band_rows, (&strips, folded_count), first);
    // Panels that do not all fit scroll under a divider (D-116), which
    // takes a row from them.
    let scrolls = layout.bands_above + layout.bands_below > 0;
    if scrolls {
        let band_rows = band_rows.saturating_sub(1);
        layout = plan_folded(inner.width, band_rows, (&strips, folded_count), first);
    }
    let mut rows = RowCursor::new(inner);
    if let Some(row) = rows.next_row() {
        let picker = GpuPicker {
            gpus: history_gpus(history),
            shown: gpu,
        };
        render_bar_size_row(frame, row, (history, &picker), context);
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
    if scrolls && let Some(row) = rows.next_row() {
        let forms = scroll_forms(layout.panels_above, layout.panels_below);
        draw_divider(frame.buffer_mut(), row, &forms, context);
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
    gpus: Vec<HistoryGpu>,
    shown: Option<&'a EntityId>,
}

impl GpuPicker<'_> {
    /// `[GPU 0 · RTX 5080]  GPU 1 · RTX 3060 `: each GPU's chip, in the
    /// order of `gpus`, the shown one marked; none on a host of one GPU.
    fn chips(&self) -> Vec<(String, bool)> {
        if self.gpus.len() < 2 {
            return Vec::new();
        }
        self.gpus
            .iter()
            .map(|gpu| {
                let picked = self.shown == Some(&gpu.entity_id);
                let text = if picked {
                    format!("[{}]", gpu.chip)
                } else {
                    format!(" {} ", gpu.chip)
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
    for ((text, _), gpu) in picker.chips().iter().zip(&picker.gpus) {
        let width = saturate_u16(display_width(text));
        if x.saturating_add(width) > row.right() {
            break;
        }
        output.entity_target(
            model,
            manifest,
            controls::HISTORY_GPU,
            Rect::new(x, row.y, width, 1),
            gpu.entity_id.clone(),
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
            &title_rule(panel, error, row.width, context),
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
    panel: &Panel<'_>,
    error: Option<&str>,
    width: u16,
    context: &DisplayContext,
) -> Line<'static> {
    let theme = &context.theme;
    let mut spans = title_spans(panel, context);
    if let Some(error) = error {
        spans.push(gap(context));
        spans.push(Span::styled(
            format!("query failed: {}", clean(error)),
            theme.tone_style(CellTone::Conflict),
        ));
    }
    let keys = panel.fold.map(fold_keys).filter(|keys| !keys.is_empty());
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

/// The panel's title in its resource's color (D-74), each GPU it names in
/// that GPU's (D-123).
fn title_spans(panel: &Panel<'_>, context: &DisplayContext) -> Vec<Span<'static>> {
    let theme = &context.theme;
    let style = theme.section_title_style(&panel.title);
    if panel.title_parts.is_empty() {
        return vec![Span::styled(clean(&panel.title), style)];
    }
    panel
        .title_parts
        .iter()
        .map(|(text, gpu)| {
            let style = gpu
                .as_deref()
                .map_or(style, |gpu| theme.gpu_style(gpu, style));
            Span::styled(clean(text), style)
        })
        .collect()
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

/// The keys scrolling the Overview History's panels (D-107; `^U`/`^D`
/// stand in for PgUp/PgDn, D-111), the fullest first.
const SCROLL_KEYS: [&str; 2] = ["PgUp/PgDn/^U/^D scroll", "^U/^D scroll"];

/// What the Overview History's divider says while its panels scroll
/// (D-116): the panels above and below the ones shown, both ways told even
/// in the short form, with the keys scrolling to them; nothing while every
/// panel shows.
fn scroll_forms(above: usize, below: usize) -> Vec<DividerForm> {
    let panels = |count: usize| counted(count, "panel");
    let (long, short) = match (above, below) {
        (0, 0) => return Vec::new(),
        (above, 0) => (format!("{} above", panels(above)), format!("{above} above")),
        (0, below) => (format!("{} below", panels(below)), format!("{below} below")),
        (above, below) => (
            format!("{} above · {} below", panels(above), panels(below)),
            format!("{above} above · {below} below"),
        ),
    };
    vec![
        (long.clone(), SCROLL_KEYS[0]),
        (long, SCROLL_KEYS[1]),
        (short.clone(), SCROLL_KEYS[1]),
        (short, ""),
    ]
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
/// An opened run's: the keys turning its pages are on its divider (D-116).
const PROCESS_DETAIL_KEYS: &[&str] = &[
    "h/l/←/→ bar · Enter back · e command · y copy · d delete",
    "h/l bar · Enter back · e command · y copy · d delete",
    "h/l bar · Enter back · y copy · d delete",
    "Enter back · d delete",
];

/// ` 1m  5m  10m … 24h ` with the current size marked, the GPU picker on
/// a host of several, then at the right how much of the charts' span is
/// recorded, as much as fits.
fn render_bar_size_row(
    frame: &mut Frame<'_>,
    row: Rect,
    (history, picker): (&HistoryView, &GpuPicker<'_>),
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
        for ((text, picked), gpu) in chips.into_iter().zip(&picker.gpus) {
            let style = if picked {
                theme.selected_row_style()
            } else {
                theme.label_style()
            };
            // Each GPU in its color (D-123).
            let style = theme.gpu_style(&gpu.label, style);
            spans.push(Span::styled(text, style));
        }
    }
    let left = Line::from(spans);
    let room = usize::from(row.width).saturating_sub(left.width() + 2 * gap(context).width());
    let note = bar_notes(recorded_note(history), why_partly(&history.disk))
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

/// The bar-size row's notes in the order tried: how much of the window is
/// recorded and why, then the record alone; none while all of it is.
fn bar_notes(recorded: Option<String>, why: &str) -> Vec<String> {
    recorded.map_or_else(Vec::new, |recorded| {
        vec![format!("{recorded} · {why}"), recorded]
    })
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

    /// The text colors of `needle`'s first character wherever it shows.
    fn colors_of(buffer: &ratatui::buffer::Buffer, needle: &str) -> Vec<ratatui::style::Color> {
        let area = buffer.area;
        let mut colors = Vec::new();
        for y in area.top()..area.bottom() {
            let row: String = (area.left()..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            for (at, _) in row.match_indices(needle) {
                let x = u16::try_from(row[..at].chars().count()).unwrap();
                colors.push(buffer[(area.x + x, y)].fg);
            }
        }
        colors
    }

    /// Each GPU the History names is in its color (D-123): the picker's
    /// chips, the runs' GPUs in the list, an opened run's identity line,
    /// its GPU panel's title and the labels of its GPU strips; a run marked
    /// for deletion keeps its GPUs in the warning color.
    #[test]
    fn the_history_names_each_gpu_in_its_color() {
        let draw_runs = |context: &DisplayContext, runs: dgxtop_core::view::ProcessHistoryView| {
            let mut model =
                crate::test_fixtures::view_model(dgxtop_core::commands::ViewKind::History, 1);
            model.modules.history = Some(std::sync::Arc::new(history_view()));
            model.modules.process_history = Some(std::sync::Arc::new(runs));
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(213, 70)).unwrap();
            terminal
                .draw(|frame| {
                    let module = HistoryModule::new().unwrap();
                    module.render(context, &model, frame.area(), frame);
                })
                .unwrap();
            terminal.backend().buffer().clone()
        };
        let draw = |context: &DisplayContext| {
            draw_runs(context, crate::test_fixtures::process_history_view())
        };
        let mut context = crate::test_fixtures::display_context();
        let theme = context.theme;
        let (first, second) = (theme.gpu_color(0), theme.gpu_color(1));
        let overview = draw(&context);
        assert_eq!(colors_of(&overview, "[GPU 0 · GB10]"), vec![first]);
        assert_eq!(colors_of(&overview, " GPU 1 · GB10 "), vec![second]);
        context.tab = 1;
        let list = draw(&context);
        // The ended run on GPU 0, the training on GPU 0 and GPU 1.
        assert_eq!(colors_of(&list, "GPU 0 · GB10"), vec![first, first]);
        assert_eq!(colors_of(&list, "GPU 1 · GB10"), vec![second]);
        let training = crate::test_fixtures::process_history_view().runs[1]
            .key
            .clone();
        context.marked = std::sync::Arc::new([training].into());
        let marked = draw(&context);
        assert_eq!(
            colors_of(&marked, "GPU 0 · GB10, GPU 1 · GB10"),
            vec![theme.warning]
        );
        assert_eq!(colors_of(&marked, "GPU 1 · GB10"), vec![theme.warning]);
        context.marked = std::sync::Arc::default();
        context.tab = 2;
        let page = draw(&context);
        // The identity line and the GPU panel's title.
        assert_eq!(colors_of(&page, "GPU 0 · GB10"), vec![first, first]);
        for label in ["util 0", "mem 0"] {
            assert_eq!(colors_of(&page, label), vec![first], "{label}");
        }
        // The run under the cursor keeps its process's swatch color too, as
        // in the process table (D-64).
        let mut runs = crate::test_fixtures::process_history_view();
        runs.opened = None;
        (context.tab, context.scroll) = (1, 1);
        let cursor = draw_runs(&context, runs);
        assert_eq!(colors_of(&cursor, "■"), vec![theme.process_color(0)]);
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
            .position(|row| row.contains("│util 0┤"))
            .unwrap();
        // The whole strip down to the memory's: below 50 % is green only at
        // its foot (D-87).
        let foot = gpu
            + rows[gpu..]
                .iter()
                .position(|row| row.contains("│ mem 0┤"))
                .unwrap()
            - 1;
        let theme = crate::test_fixtures::display_context().theme;
        let strip_rows = u16::try_from(gpu).unwrap()..=u16::try_from(foot).unwrap();
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
            list[training].contains("GPU 0 · GB10, GPU 1 · GB10"),
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
    fn run_page(offset: u16, size: (u16, u16)) -> (Vec<String>, RenderOutput) {
        run_page_of(&crate::test_fixtures::process_history_view(), offset, size)
    }

    /// [`run_page`] of `view`'s opened run.
    fn run_page_of(
        view: &dgxtop_core::view::ProcessHistoryView,
        offset: u16,
        (width, height): (u16, u16),
    ) -> (Vec<String>, RenderOutput) {
        let mut context = crate::test_fixtures::display_context();
        context.tab = 2;
        context.offset = offset;
        let mut model =
            crate::test_fixtures::view_model(dgxtop_core::commands::ViewKind::History, 1);
        model.modules.process_history = Some(std::sync::Arc::new(view.clone()));
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
    /// legends shows them on pages (D-112) under its fixed top (D-116): the
    /// same header and bar sizes on every page, then a divider joined to
    /// the panel's borders that says the page, what the next holds and the
    /// keys turning pages; the frame tells the controller the pages
    /// around. A tall panel keeps one page under a bare divider.
    #[test]
    fn a_runs_charts_go_on_pages_under_its_fixed_top() {
        let count = |output: &RenderOutput, control| {
            output
                .hit_targets
                .iter()
                .filter(|target| target.control_id == control)
                .count()
        };
        // The title, the identity line, four rows of details, the bar
        // sizes, then the divider.
        const DIVIDER: usize = 7;
        let (first, output) = run_page(0, (213, 34));
        let text = first.join("\n");
        assert!(
            !first[0].contains("page"),
            "the divider names the pages: {text}"
        );
        assert!(first[DIVIDER - 1].contains(" auto "), "{text}");
        let divider = &first[DIVIDER];
        assert!(divider.starts_with('├') && divider.ends_with('┤'), "{text}");
        assert!(
            divider.ends_with(
                "─ page 1 of 2 · next: GPU 0 · GB10, Tasks, Disk, Network · host (j/k/↑/↓ page) ┤"
            ),
            "{text}"
        );
        assert!(text.contains("folder   /home/u/code/ComfyUI"), "{text}");
        assert!(
            text.contains("Memory · 128 GiB ─") && !text.contains("Tasks ─"),
            "{text}"
        );
        assert_eq!(
            text.matches("last ").count(),
            5,
            "every strip shows its last value: {text}"
        );
        assert_eq!(
            (
                count(&output, controls::PAGE_ABOVE),
                count(&output, controls::PAGE_BELOW)
            ),
            (0, 1)
        );
        let (second, output) = run_page(1, (213, 34));
        let text = second.join("\n");
        assert_eq!(
            second[..DIVIDER],
            first[..DIVIDER],
            "the top stays put: {text}"
        );
        assert!(
            second[DIVIDER].ends_with("─ page 2 of 2 (j/k/↑/↓ page) ┤"),
            "{text}"
        );
        assert!(
            text.contains("│GPU 0 · GB10 ─")
                && text.contains("Tasks ─")
                && text.contains("Network · host ─"),
            "{text}"
        );
        assert_eq!(
            (
                count(&output, controls::PAGE_ABOVE),
                count(&output, controls::PAGE_BELOW)
            ),
            (1, 0)
        );
        let (past, _) = run_page(9, (213, 34));
        assert_eq!(past, second, "the last page at most");
        let (tall, output) = run_page(0, (213, 60));
        let text = tall.join("\n");
        assert!(!text.contains("page 1 of"), "{text}");
        assert!(
            tall[DIVIDER].starts_with("├───") && tall[DIVIDER].ends_with("───┤"),
            "a bare divider: {text}"
        );
        assert!(
            tall[DIVIDER].trim_matches(['├', '─', '┤']).is_empty(),
            "{text}"
        );
        assert_eq!(count(&output, controls::PAGE_BELOW), 0);
    }

    /// Two panels a band, a run's panels pair up to the last on every page
    /// (D-117): no band leaves the right half of the panel empty. At the
    /// owner's 213×41 (a panel 213×39) they all fit one page.
    #[test]
    fn a_runs_panels_pair_up_on_every_page() {
        let view = crate::test_fixtures::process_history_view();
        for size in [(213, 39), (213, 37), (213, 30), (213, 60), (160, 37)] {
            for (index, page) in run_pages_of(&view, size).iter().enumerate() {
                assert_bands_pair(page, size.0, &format!("{size:?} page {}", index + 1));
            }
        }
        let fitting = run_pages_of(&view, (213, 39));
        assert_eq!(fitting.len(), 1, "{}", fitting[0].join("\n"));
    }

    /// A run whose open files dgxtop cannot read, another user's (D-77),
    /// lets that strip go from its Tasks panel: no chart stands without
    /// data, and the panels still pair up (D-117).
    #[test]
    fn a_run_without_open_files_shows_no_empty_chart() {
        let mut view = crate::test_fixtures::process_history_view();
        let detail = view.opened.as_mut().unwrap();
        let files = detail
            .charts
            .iter_mut()
            .find(|chart| chart.metric == dgxtop_core::process_history::RunMetric::OpenFiles)
            .unwrap();
        files.points.clear();
        (files.latest, files.mean, files.peak) = (None, None, None);
        for size in [(213, 39), (160, 37), (120, 30)] {
            for (index, page) in run_pages_of(&view, size).iter().enumerate() {
                let place = format!("{size:?} page {}", index + 1);
                let text = page.join("\n");
                assert!(
                    !text.contains("last n/a"),
                    "{place}: an empty chart: {text}"
                );
                assert!(!text.contains("files┤"), "{place}: {text}");
                if size.0 >= 152 {
                    assert_bands_pair(page, size.0, &place);
                }
            }
        }
    }

    /// The GPU panel shows each GPU the run used, in the order it first
    /// used them (D-120): its utilization there, else the GPU's own (`dev`)
    /// where the driver sampled no process (GB10), then its memory there.
    /// A run recorded before the GPUs were kept apart shows the sums.
    #[test]
    fn a_runs_gpu_panel_shows_each_gpu_it_used() {
        use dgxtop_core::process_history::{GpuFigure, RunMetric};
        let mut view = crate::test_fixtures::process_history_view();
        let gpus = vec!["GPU 1".to_owned(), "GPU 0".to_owned()];
        view.runs[0].gpus.clone_from(&gpus);
        view.runs[0].gpu_names = vec![
            "NVIDIA GeForce RTX 3060".to_owned(),
            "NVIDIA GeForce RTX 5080".to_owned(),
        ];
        let detail = view.opened.as_mut().unwrap();
        detail.info.gpus = gpus;
        let recorded = detail
            .charts
            .iter()
            .find(|chart| chart.metric == RunMetric::Gpu0Memory)
            .unwrap()
            .points
            .clone();
        for chart in &mut detail.charts {
            match chart.metric.gpu_slot() {
                Some((1, GpuFigure::Utilization)) => chart.points.clear(),
                Some((1, _)) => chart.points.clone_from(&recorded),
                _ => {}
            }
        }
        let text = run_page_of(&view, 0, (213, 60)).0.join("\n");
        let places: Vec<usize> = ["│util 1┤", "│ mem 1┤", "│ dev 0┤", "│ mem 0┤"]
            .iter()
            .map(|label| {
                text.find(label)
                    .unwrap_or_else(|| panic!("no {label}: {text}"))
            })
            .collect();
        assert!(places.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
        assert!(
            text.contains("│GPU 1 · RTX 3060, GPU 0 · RTX 5080 ─"),
            "{text}"
        );
        assert!(
            !text.contains("util 0┤"),
            "GPU 0 sampled no process: {text}"
        );
        let mut older = crate::test_fixtures::process_history_view();
        for chart in &mut older.opened.as_mut().unwrap().charts {
            if chart.metric.gpu_slot().is_some() {
                chart.points.clear();
            }
        }
        let text = run_page_of(&older, 0, (213, 60)).0.join("\n");
        assert!(text.contains("│GPU · 120 GiB ─"), "{text}");
        assert!(
            text.contains("│  util┤") && text.contains("│memory┤"),
            "{text}"
        );
        // A run on more than four GPUs adds the sums over all of them.
        let mut wide = crate::test_fixtures::process_history_view();
        let detail = wide.opened.as_mut().unwrap();
        detail.info.gpus = (0..6).map(|gpu| format!("GPU {gpu}")).collect();
        for chart in &mut detail.charts {
            if chart.metric.gpu_slot().is_some() {
                chart.points.clone_from(&recorded);
            }
        }
        let text = run_page_of(&wide, 0, (213, 90)).0.join("\n");
        let places: Vec<usize> = ["│ mem 3┤", "│util Σ┤", "│ mem Σ┤"]
            .iter()
            .map(|label| {
                text.find(label)
                    .unwrap_or_else(|| panic!("no {label}: {text}"))
            })
            .collect();
        assert!(places.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
    }

    /// Every page of `view`'s opened run at `size`.
    fn run_pages_of(
        view: &dgxtop_core::view::ProcessHistoryView,
        size: (u16, u16),
    ) -> Vec<Vec<String>> {
        let below = run_page_of(view, 0, size)
            .1
            .hit_targets
            .iter()
            .filter(|target| target.control_id == controls::PAGE_BELOW)
            .count();
        (0..=below)
            .map(|page| run_page_of(view, u16::try_from(page).unwrap(), size).0)
            .collect()
    }

    /// Panics unless every band of a two-column `page`, `width` wide,
    /// holds a panel at the right too.
    fn assert_bands_pair(page: &[String], width: u16, place: &str) {
        let panel = (width - 2 - dgxtop_core::view::HISTORY_PANEL_GAP) / 2;
        let right = usize::from(1 + panel + dgxtop_core::view::HISTORY_PANEL_GAP);
        let text = page.join("\n");
        let rows: Vec<Vec<char>> = page.iter().map(|row| row.chars().collect()).collect();
        // A band's title row: a title and its rule at the left.
        let titles = rows.iter().filter(|row| {
            let left: String = row[1..right].iter().collect();
            left.starts_with(char::is_alphanumeric) && left.contains(" ─")
        });
        for row in titles {
            assert!(
                row[right].is_alphanumeric(),
                "{place}: a band of one panel: {text}"
            );
        }
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
                "h/l/←/→ bar · Enter back · e command · y copy · d delete",
            ),
            (85, "h/l bar · Enter back · e command · y copy · d delete"),
            (75, "h/l bar · Enter back · y copy · d delete"),
            (60, "Enter back · d delete"),
        ] {
            let row = title_row(2, width);
            assert!(row.contains(keys), "{width}: {row}");
        }
        assert!(!title_row(2, 50).contains("delete"), "no room for any");
        // Its charts on pages: the divider names the keys turning them, the
        // title the same keys as on one page; no key switches the run
        // (D-116).
        let (paged, _) = run_page(0, (213, 37));
        assert!(
            paged[0].contains("(h/l/←/→ bar · Enter back · e command · y copy · d delete)"),
            "{}",
            paged[0]
        );
        assert!(!paged[0].contains("next") && !paged[0].contains("page"));
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
        context.picked = Some(gpus[1].entity_id.clone());
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
        assert!(rows[1].contains(" GPU 0 · GB10 [GPU 1 · GB10]"), "{text}");
        assert!(!rows[1].contains("panel"), "{text}");
        // Its panels scroll: a divider says what is above and below (D-116).
        assert!(rows[2].starts_with('├') && rows[2].ends_with('┤'), "{text}");
        assert!(rows[2].contains("─ 1 panel above · "), "{text}");
        assert!(
            rows[2].ends_with(" below (PgUp/PgDn/^U/^D scroll) ┤"),
            "{text}"
        );
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
        assert!(detail[0].contains("h/l/←/→ bar"), "{text}");
        assert!(
            text.contains(
                "ComfyUI/main │ PID 42 │ uid 1000 │ nice 0 │ GPU 0 · GB10 │ SIGKILL by dgxtop"
            ),
            "{text}"
        );
        for entry in [
            "started  2026-10-04 09:00:00",
            "ended    2026-10-04 09:30:00",
            "runtime  30m",
            "parent   7 bash",
            "folder   /home/u/code/ComfyUI",
            "program  /venv/bin/python3.12",
            "cgroup   /user.slice/user-1000.slice/session-3.scope",
            "command  /venv/bin/python main.py --listen",
        ] {
            assert!(text.contains(entry), "missing {entry}: {text}");
        }
        assert!(!text.contains("--preview-method"), "one line: {text}");
        // Its charts, page by page (D-112).
        let pages: String = (0..4)
            .map(|offset| run_page(offset, (120, 30)).0.join("\n"))
            .collect();
        for title in [
            "CPU · 20 CPUs",
            "Memory · 128 GiB",
            "GPU 0 · GB10 ─",
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

    /// While the Overview History's panels scroll its divider says what is
    /// above and below the ones shown, both ways even in its short form,
    /// with the keys scrolling (D-107, D-116); nothing while all show.
    #[test]
    fn the_scroll_divider_tells_both_ways() {
        assert!(scroll_forms(0, 0).is_empty());
        let below = |text: &str, keys| (text.to_owned(), keys);
        assert_eq!(
            scroll_forms(0, 1),
            [
                below("1 panel below", "PgUp/PgDn/^U/^D scroll"),
                below("1 panel below", "^U/^D scroll"),
                below("1 below", "^U/^D scroll"),
                below("1 below", ""),
            ]
        );
        assert_eq!(scroll_forms(2, 0)[0].0, "2 panels above");
        let both = scroll_forms(1, 2);
        assert_eq!(both[0].0, "1 panel above · 2 panels below");
        assert_eq!(both[3].0, "1 above · 2 below");
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
            bar_notes(recorded, why_partly(&DiskHistory::On)),
            [
                "recorded 5h 12m of 24h · only while dgxtop runs",
                "recorded 5h 12m of 24h"
            ]
        );
        assert!(bar_notes(None, why_partly(&DiskHistory::On)).is_empty());
    }
}
