//! Rendering helpers shared by the built-in modules: panels, row cursors,
//! evidence-preserving cell spans, gauges and a width-fitting table.

use std::ops::Range;

use dgxtop_core::identity::{EntityId, ModuleId, ProcessKey};
use dgxtop_core::metric::CanonicalUnit;
use dgxtop_core::view::{CellState, MetricCell, ViewModel};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

use super::ModuleBuildError;
use crate::format::{
    CellDetail, cell_tone, describe_cell, display_width, format_value, pad_left, pad_right,
    state_label_for_width, truncate_to_width, value_text,
};
use crate::num::saturate_u16;
use crate::registry::{
    DISPLAY_MODULE_API_VERSION, DisplayContext, DisplayModuleManifest, HitTarget, RenderOutput,
    Slot,
};
use crate::theme::ThemePalette;
use crate::widgets::{GapSparkline, Gauge, Part};

/// Upper bound of accessibility labels per module render.
pub(crate) const MAX_LABELS: usize = 64;

/// Static manifest description of a built-in module.
pub(crate) struct ManifestSpec {
    pub id: &'static str,
    pub title: &'static str,
    pub min_columns: u16,
    pub min_rows: u16,
    pub priority: u8,
    pub slots: &'static [Slot],
}

pub(crate) fn build_manifest(
    spec: ManifestSpec,
) -> Result<DisplayModuleManifest, ModuleBuildError> {
    let manifest = DisplayModuleManifest {
        module_id: ModuleId::new(spec.id)?,
        api_version: DISPLAY_MODULE_API_VERSION,
        title: spec.title.to_owned(),
        min_columns: spec.min_columns,
        min_rows: spec.min_rows,
        priority: spec.priority,
        slots: spec.slots.to_vec(),
        enabled_by_default: true,
    };
    manifest.validate()?;
    Ok(manifest)
}

/// Bounded accessibility labels plus hit targets of one render.
#[derive(Debug, Default)]
pub(crate) struct Output {
    labels: Vec<String>,
    targets: Vec<HitTarget>,
}

impl Output {
    pub fn label(&mut self, label: impl Into<String>) {
        if self.labels.len() < MAX_LABELS {
            self.labels.push(label.into());
        }
    }

    pub fn target(
        &mut self,
        model: &ViewModel,
        manifest: &DisplayModuleManifest,
        control_id: &'static str,
        rect: Rect,
    ) -> &mut HitTarget {
        self.targets.push(HitTarget {
            frame_revision: model.frame_revision,
            module_id: manifest.module_id.clone(),
            control_id,
            entity_id: None,
            process_key: None,
            rect,
        });
        let last = self.targets.len() - 1;
        &mut self.targets[last]
    }

    pub fn entity_target(
        &mut self,
        model: &ViewModel,
        manifest: &DisplayModuleManifest,
        control_id: &'static str,
        rect: Rect,
        entity_id: EntityId,
    ) {
        self.target(model, manifest, control_id, rect).entity_id = Some(entity_id);
    }

    pub fn process_target(
        &mut self,
        model: &ViewModel,
        manifest: &DisplayModuleManifest,
        control_id: &'static str,
        rect: Rect,
        process_key: ProcessKey,
    ) {
        self.target(model, manifest, control_id, rect).process_key = Some(process_key);
    }

    pub fn finish(self) -> RenderOutput {
        RenderOutput {
            hit_targets: self.targets,
            accessibility_labels: self.labels,
        }
    }
}

/// Hands out consecutive one-row (or n-row) slices of an area.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RowCursor {
    area: Rect,
    used: u16,
}

impl RowCursor {
    pub fn new(area: Rect) -> Self {
        Self { area, used: 0 }
    }

    pub fn remaining(&self) -> u16 {
        self.area.height.saturating_sub(self.used)
    }

    pub fn next_row(&mut self) -> Option<Rect> {
        self.take(1)
    }

    pub fn take(&mut self, rows: u16) -> Option<Rect> {
        let rows = rows.min(self.remaining());
        if rows == 0 || self.area.width == 0 {
            return None;
        }
        let rect = Rect::new(self.area.x, self.area.y + self.used, self.area.width, rows);
        self.used += rows;
        Some(rect)
    }

    pub fn rest(&mut self) -> Option<Rect> {
        self.take(self.remaining())
    }
}

/// Rounded panel with a styled title; returns the inner area.
pub(crate) fn panel(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    context: &DisplayContext,
) -> Rect {
    panel_with_keys(frame, area, title, "", context)
}

/// ` (k action · …) `: each key bright in the accent color, its action in
/// text color, the brackets and dots muted (D-70). `keys` is `key action`
/// items joined by ` · `.
pub(crate) fn key_spans(keys: &str, context: &DisplayContext) -> Vec<Span<'static>> {
    let theme = &context.theme;
    let key_style = Style::new()
        .fg(theme.accent)
        .add_modifier(ratatui::style::Modifier::BOLD);
    let mut spans = vec![Span::styled(" (", theme.muted_style())];
    for (index, item) in keys.split(KEY_SEPARATOR).enumerate() {
        if index > 0 {
            spans.push(Span::styled(KEY_SEPARATOR, theme.muted_style()));
        }
        let (key, action) = item.split_once(' ').unwrap_or((item, ""));
        spans.push(Span::styled(key.to_owned(), key_style));
        if !action.is_empty() {
            spans.push(Span::styled(format!(" {action}"), theme.text_style()));
        }
    }
    spans.push(Span::styled(") ", theme.muted_style()));
    spans
}

/// Between the items of a panel's keys.
const KEY_SEPARATOR: &str = " · ";

/// [`panel`] with the keys that act on it, `(k action · …)`, at the right
/// of its title row (D-70), so a key is shown where it acts; left out when
/// they would crowd the title, which says what the panel shows.
pub(crate) fn panel_with_keys(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    keys: &str,
    context: &DisplayContext,
) -> Rect {
    let style = context.theme.section_title_style(title);
    styled_panel_with_keys(frame, area, (title, style), keys, context)
}

/// [`panel_with_keys`] with its title in `style`: a GPU's in its color
/// (D-123).
pub(crate) fn styled_panel_with_keys(
    frame: &mut Frame<'_>,
    area: Rect,
    (title, style): (&str, Style),
    keys: &str,
    context: &DisplayContext,
) -> Rect {
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(context.theme.border_style(false))
        .title(Line::from(Span::styled(format!(" {title} "), style)));
    if !keys.is_empty() && keys_fit(area.width, title, keys) {
        block = block.title(Line::from(key_spans(keys, context)).right_aligned());
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// Whether `keys` fit at the right of the title of a panel `width` wide
/// titled `title`.
fn keys_fit(width: u16, title: &str, keys: &str) -> bool {
    let title_width = display_width(title) + 2;
    let keys_width = display_width(keys) + 4;
    usize::from(width) >= title_width + keys_width + 4
}

/// The first of `choices` (the fullest first) that fits at the right of
/// the title of a panel `width` wide titled `title`; none when none does.
pub(crate) fn fitting_keys<'a>(width: u16, title: &str, choices: &[&'a str]) -> &'a str {
    choices
        .iter()
        .copied()
        .find(|keys| keys_fit(width, title, keys))
        .unwrap_or("")
}

/// Wrapped message (used for "no data" and error explanations).
pub(crate) fn message(frame: &mut Frame<'_>, area: Rect, text: &str, style: Style) {
    if area.is_empty() {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text.to_owned(), style))).wrap(Wrap { trim: true }),
        area,
    );
}

/// Text shown when the projection does not carry a topic.
pub(crate) fn no_data(topic: &str) -> String {
    format!("no {topic} data in this projection")
}

/// Draws a line into the first row of `area`, clipped to its width.
pub(crate) fn draw_line(buffer: &mut Buffer, area: Rect, line: &Line<'_>) {
    if area.is_empty() {
        return;
    }
    buffer.set_line(area.x, area.y, line, area.width);
}

/// Primary text styled by tone, followed by muted annotations.
pub(crate) fn cell_spans(
    cell: &MetricCell,
    detail: CellDetail,
    context: &DisplayContext,
) -> Vec<Span<'static>> {
    let text = describe_cell(cell, detail, context.projection_age_ms);
    let mut spans = vec![Span::styled(
        text.primary,
        context.theme.tone_style(text.tone),
    )];
    for annotation in text.annotations {
        spans.push(Span::styled(" · ", context.theme.muted_style()));
        spans.push(Span::styled(annotation, context.theme.label_style()));
    }
    spans
}

/// [`cell_spans`] of a temperature: a value read as it should be is
/// written in its level color, green up to 50 °C (D-104, D-126); a
/// fallback, stale, conflicting or missing one keeps the tone that says so.
pub(crate) fn temperature_spans(
    cell: &MetricCell,
    detail: CellDetail,
    context: &DisplayContext,
) -> Vec<Span<'static>> {
    value_styled(
        cell_spans(cell, detail, context),
        context.theme.temperature_style(cell),
    )
}

/// [`cell_spans`] of a fan speed, in its level color like a temperature:
/// green up to 50 % (D-126).
pub(crate) fn fan_speed_spans(
    cell: &MetricCell,
    detail: CellDetail,
    context: &DisplayContext,
) -> Vec<Span<'static>> {
    value_styled(
        cell_spans(cell, detail, context),
        context.theme.fan_style(cell),
    )
}

/// `spans` with the value, their first, in `style`.
fn value_styled(mut spans: Vec<Span<'static>>, style: Style) -> Vec<Span<'static>> {
    if let Some(first) = spans.first_mut() {
        first.style = style;
    }
    spans
}

/// `label value · annotations` of a temperature ([`temperature_spans`]).
pub(crate) fn labeled_temperature(
    label: &str,
    cell: &MetricCell,
    detail: CellDetail,
    context: &DisplayContext,
) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        format!("{label} "),
        context.theme.label_style(),
    )];
    spans.extend(temperature_spans(cell, detail, context));
    spans
}

/// `⚡` before a power, in the theme's amber, as every headline writes it.
pub(crate) fn power_icon(context: &DisplayContext) -> Span<'static> {
    Span::styled("⚡", Style::new().fg(context.theme.warning))
}

/// Whether a line lists `cell`: not when the device does not support it or
/// reports nothing for it (D-56); a read that failed still shows its state.
pub(crate) fn reported(cell: &MetricCell) -> bool {
    cell.value.is_some() || !matches!(cell.state, CellState::Unsupported | CellState::Unavailable)
}

/// Whether `cell` holds a current value to show.
pub(crate) fn has_value(cell: &MetricCell) -> bool {
    cell.state.has_value() && cell.value.is_some()
}

/// `label value · annotations` spans.
pub(crate) fn labeled_cell(
    label: &str,
    cell: &MetricCell,
    detail: CellDetail,
    context: &DisplayContext,
) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        format!("{label} "),
        context.theme.label_style(),
    )];
    spans.extend(cell_spans(cell, detail, context));
    spans
}

/// Plain text of a labeled cell, for accessibility labels.
pub(crate) fn cell_label(label: &str, cell: &MetricCell, context: &DisplayContext) -> String {
    format!(
        "{label}: {}",
        describe_cell(cell, CellDetail::SOURCED, context.projection_age_ms).joined()
    )
}

/// Separator span between items on one line.
pub(crate) fn gap(context: &DisplayContext) -> Span<'static> {
    Span::styled("  ", context.theme.muted_style())
}

/// ` │ ` between the parts of a line that say different things (D-66),
/// the glyph the footer sets its keys apart with.
pub(crate) fn separator(context: &DisplayContext) -> Span<'static> {
    Span::styled(" │ ", context.theme.muted_style())
}

/// Percent cell as a 0..=1 ratio.
pub(crate) fn percent_ratio(cell: &MetricCell) -> Option<f64> {
    cell.value_f64().map(|value| value / 100.0)
}

/// `part / whole` when both values exist and `whole > 0` (display fill only).
pub(crate) fn fraction_ratio(part: &MetricCell, whole: &MetricCell) -> Option<f64> {
    let part = part.value_f64()?;
    let whole = whole.value_f64()?;
    (whole > 0.0).then(|| part / whole)
}

/// `label  [gauge] value` on one row; the label column has a fixed width.
pub(crate) fn gauge_row(
    frame: &mut Frame<'_>,
    row: Rect,
    label: &str,
    label_width: u16,
    ratio: Option<f64>,
    value: &str,
    context: &DisplayContext,
) {
    if row.is_empty() {
        return;
    }
    let label_width = label_width.min(row.width);
    frame.buffer_mut().set_string(
        row.x,
        row.y,
        pad_right(label, usize::from(label_width)),
        context.theme.label_style(),
    );
    let gauge_area = Rect::new(
        row.x + label_width,
        row.y,
        row.width.saturating_sub(label_width),
        1,
    );
    frame.render_widget(Gauge::new(ratio, value, &context.theme), gauge_area);
}

/// Narrowest rate history worth drawing beside its `peak` text.
const MIN_HISTORY_WIDTH: u16 = 8;
/// Text of a history without a finite point (e.g. shed to stay in budget).
const NO_HISTORY: &str = "no trend data";

/// `text` right-aligned in `area`.
pub(crate) fn draw_value(buffer: &mut Buffer, area: Rect, text: &str, style: Style) {
    if area.is_empty() {
        return;
    }
    buffer.set_stringn(
        area.x,
        area.y,
        pad_left(text, usize::from(area.width)),
        usize::from(area.width),
        style,
    );
}

/// Label column of a bar row (`util `, `used `, `VRAM `); the chart under
/// the bar starts after it, at the gauge's left edge.
pub(crate) const BAR_LABEL_WIDTH: u16 = 5;

/// Columns of a bar row (D-43), left to right: the label, the gauge of the
/// current value and the value right after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BarRowAreas {
    pub label: Rect,
    pub gauge: Rect,
    pub value: Rect,
}

/// Splits `row` into a bar row; rows that share the value width line their
/// gauges up.
pub(crate) fn bar_row_areas(row: Rect, value_width: u16) -> BarRowAreas {
    let label_width = BAR_LABEL_WIDTH.min(row.width);
    let gauge_width = row
        .width
        .saturating_sub(label_width.saturating_add(1).saturating_add(value_width));
    let at = |x: u16, width: u16| {
        let x = x.min(row.right());
        Rect::new(x, row.y, width.min(row.right() - x), row.height.min(1))
    };
    let label = at(row.x, label_width);
    let gauge = at(label.right(), gauge_width);
    let value = at(gauge.right().saturating_add(1), value_width);
    BarRowAreas {
        label,
        gauge,
        value,
    }
}

/// One bar of the current value (D-43).
pub(crate) struct BarRow<'a> {
    pub label: &'a str,
    /// The level the value is colored by (`None`: missing): its share of
    /// the scale, or a temperature's place on its own scale (D-126).
    pub ratio: Option<f64>,
    /// Drawn right after the gauge, so it reads as the bar's figure; values
    /// of one kind padded on the left end on one column.
    pub value: &'a str,
    /// Value column width, shared by rows whose gauges should line up.
    pub value_width: u16,
}

/// Draws a bar row's label and value and returns the gauge area, where the
/// caller draws the bar of the current value.
pub(crate) fn render_bar_row(
    frame: &mut Frame<'_>,
    row: Rect,
    bar: &BarRow<'_>,
    context: &DisplayContext,
) -> Rect {
    // A value wider than the shared column (a state label) is never cut.
    let value_width = bar.value_width.max(saturate_u16(display_width(bar.value)));
    let areas = bar_row_areas(row, value_width);
    frame.buffer_mut().set_stringn(
        areas.label.x,
        areas.label.y,
        bar.label,
        usize::from(areas.label.width),
        context.theme.label_style(),
    );
    let value_style = bar.ratio.map_or_else(
        || context.theme.muted_style(),
        |ratio| Style::new().fg(context.theme.level_color(ratio)),
    );
    frame.buffer_mut().set_stringn(
        areas.value.x,
        areas.value.y,
        bar.value,
        usize::from(areas.value.width),
        value_style,
    );
    areas.gauge
}

/// The rows of `area` from the gauge's left edge on: where the chart under
/// a bar goes.
pub(crate) fn under_bar(area: Rect) -> Rect {
    let indent = BAR_LABEL_WIDTH.min(area.width);
    Rect::new(area.x + indent, area.y, area.width - indent, area.height)
}

/// The chart under a bar (D-43): its history in block columns, one per
/// sample with the newest at the right, each column in the gauge color of
/// its share of `max`, so a busy stretch reads red at a glance; a muted
/// "no trend data" on the bottom row when the history has no finite point.
pub(crate) fn render_chart(
    frame: &mut Frame<'_>,
    area: Rect,
    points: &[Option<f64>],
    max: Option<f64>,
    context: &DisplayContext,
) {
    render_chart_with_parts(frame, area, points, (Vec::new(), None), max, context);
}

/// [`render_chart`] with parts of each point (what each GPU process holds
/// of the used RAM, D-64) stacked from the foot of its column, each in its
/// style; a column of parts lower than a row is drawn a row high, split
/// between them and the rest of it, so both show (D-134).
/// `whole` colors what the parts leave of each column; `None` colors it by
/// level.
pub(crate) fn render_chart_with_parts(
    frame: &mut Frame<'_>,
    area: Rect,
    points: &[Option<f64>],
    (parts, whole): (Vec<Part<'_>>, Option<Color>),
    max: Option<f64>,
    context: &DisplayContext,
) {
    if area.is_empty() {
        return;
    }
    if !points.iter().flatten().any(|value| value.is_finite()) {
        frame.buffer_mut().set_stringn(
            area.x,
            area.bottom() - 1,
            NO_HISTORY,
            usize::from(area.width),
            context.theme.muted_style(),
        );
        return;
    }
    let sparkline = match whole {
        Some(color) => GapSparkline::new(points, Style::new().fg(color)),
        None => GapSparkline::new(points, Style::new()).level_colors(&context.theme),
    }
    .max(max)
    .parts(parts)
    .raise_low_columns();
    frame.render_widget(sparkline, area);
}

/// A chart's caption row: `left` from the start and, muted at the right
/// edge, `right` when it fits a gap after `left`.
pub(crate) fn draw_caption(
    buffer: &mut Buffer,
    row: Rect,
    left: &Line<'_>,
    right: Option<&str>,
    context: &DisplayContext,
) {
    draw_line(buffer, row, left);
    let Some(right) = right else { return };
    let width = display_width(right);
    if left.width() + display_width(&gap(context).content) + width > usize::from(row.width) {
        return;
    }
    buffer.set_string(
        row.right() - saturate_u16(width),
        row.y,
        right,
        context.theme.muted_style(),
    );
}

/// What a divider says at its right end (D-116): a note and the keys acting
/// on the part under it.
pub(crate) type DividerForm = (String, &'static str);

/// The keys turning a module's pages (D-110, D-116), the fullest first.
pub(crate) const PAGE_KEYS: [&str; 2] = ["j/k/↑/↓ page", "j/k page"];

/// Columns of rule a divider keeps left of its note.
const DIVIDER_LEAD: usize = 4;

/// The divider over pages (D-116): `page 1 of 3 · next: PCIe, Thermal`,
/// then the next page's count of `noun`s, then the place alone, each with
/// the keys turning pages, then shorter keys and `1/3`; nothing on one
/// page. `next` names what the next page holds.
pub(crate) fn page_divider_forms(
    (page, count): (usize, usize),
    next: &[String],
    noun: &str,
) -> Vec<DividerForm> {
    if count < 2 {
        return Vec::new();
    }
    let place = format!("page {} of {count}", page + 1);
    let mut forms = Vec::new();
    if !next.is_empty() {
        forms.push((format!("{place} · next: {}", next.join(", ")), PAGE_KEYS[0]));
        forms.push((
            format!(
                "{place} · next: {}",
                crate::format::counted(next.len(), noun)
            ),
            PAGE_KEYS[0],
        ));
    }
    let short = format!("{}/{count}", page + 1);
    forms.extend([
        (place.clone(), PAGE_KEYS[0]),
        (place, PAGE_KEYS[1]),
        (short.clone(), PAGE_KEYS[1]),
        (short, ""),
    ]);
    forms
}

/// `──── page 1 of 2 · next: PCIe (j/k page) `: a divider's line `width`
/// wide, ending in the first of `forms` that fits after a few columns of
/// rule, as a panel's title row ends in its keys; a bare rule when none
/// does.
pub(crate) fn divider_line(
    width: u16,
    forms: &[DividerForm],
    context: &DisplayContext,
) -> Line<'static> {
    let width = usize::from(width);
    let spans_width = |spans: &[Span<'static>]| spans.iter().map(Span::width).sum::<usize>();
    let tail = forms
        .iter()
        .map(|(note, keys)| divider_tail(note, keys, context))
        .find(|tail| spans_width(tail) + DIVIDER_LEAD <= width)
        .unwrap_or_default();
    let rule = width.saturating_sub(spans_width(&tail));
    let mut spans = vec![Span::styled(
        "─".repeat(rule),
        context.theme.border_style(false),
    )];
    spans.extend(tail);
    Line::from(spans)
}

/// ` page 1 of 2 (j/k page) `: the note, then the keys as [`key_spans`]
/// writes them.
fn divider_tail(note: &str, keys: &str, context: &DisplayContext) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if !note.is_empty() {
        spans.push(Span::styled(
            format!(" {note}"),
            context.theme.label_style(),
        ));
    }
    if !keys.is_empty() {
        spans.extend(key_spans(keys, context));
    } else if !spans.is_empty() {
        spans.push(Span::raw(" "));
    }
    spans
}

/// [`divider_line`] across `row`, a row inside a bordered panel, joined to
/// the panel's side borders: `├` and `┤` take the place of their `│`, so
/// the divider parts the panel's fixed top from its pages (D-116).
pub(crate) fn draw_divider(
    buffer: &mut Buffer,
    row: Rect,
    forms: &[DividerForm],
    context: &DisplayContext,
) {
    if row.is_empty() {
        return;
    }
    draw_line(buffer, row, &divider_line(row.width, forms, context));
    let style = context.theme.border_style(false);
    for (x, joint) in [(row.x.checked_sub(1), "├"), (Some(row.right()), "┤")] {
        if let Some(cell) = x.and_then(|x| buffer.cell_mut((x, row.y))) {
            cell.set_symbol(joint).set_style(style);
        }
    }
}

/// A context for measuring text in the sizing hooks, which get none: the
/// projection is taken as fresh.
pub(crate) fn measuring_context(model: &ViewModel, slot: Slot) -> DisplayContext {
    DisplayContext {
        theme: ThemePalette::for_model(model),
        viewport: Rect::default(),
        tick_for_animation: 0,
        compact: false,
        slot,
        projection_age_ms: 0,
        expanded: false,
        scroll: 0,
        tab: 0,
        offset: 0,
        marked: std::sync::Arc::default(),
        picked: None,
        folded: std::sync::Arc::default(),
    }
}

/// `first` and `second` on one line, set apart by a [`separator`], when
/// both fit `width`; otherwise each on its own line.
pub(crate) fn join_if_fits(
    first: Line<'static>,
    second: Line<'static>,
    width: u16,
    context: &DisplayContext,
) -> Vec<Line<'static>> {
    let joined = first.width() + display_width(&separator(context).content) + second.width();
    if joined > usize::from(width) {
        return vec![first, second];
    }
    let mut spans = first.spans;
    spans.push(separator(context));
    spans.extend(second.spans);
    vec![Line::from(spans)]
}

/// Rows of the rate histories below the disk and network tables (D-41).
pub(crate) const RATE_ROWS: u16 = 2;

/// Rows a table panel gives its rate histories: both once the table keeps
/// its header and one row, else none; the table takes the rest.
pub(crate) fn rate_rows(inner_height: u16) -> u16 {
    if inner_height >= 2 + RATE_ROWS {
        RATE_ROWS
    } else {
        0
    }
}

/// One rate of a disk or network panel: `R`/`W` or `↓`/`↑`, its color, the
/// current rate and its history.
pub(crate) struct RateHistory<'a> {
    pub label: &'static str,
    pub color: Color,
    pub current: &'a MetricCell,
    pub points: &'a [Option<f64>],
}

/// Width of the current-rate column (`1023.9 KiB/s`).
const RATE_VALUE_WIDTH: u16 = 12;

/// Rate rows (D-41): the colored label, the current rate, the history scaled
/// to its own peak, then `peak <rate>` so the scale is never a guess.
pub(crate) fn render_rate_rows(
    frame: &mut Frame<'_>,
    area: Rect,
    rates: &[RateHistory<'_>],
    context: &DisplayContext,
) {
    let mut rows = RowCursor::new(area);
    for rate in rates {
        let Some(row) = rows.next_row() else { break };
        let label_width = saturate_u16(display_width(rate.label)) + 1;
        frame.buffer_mut().set_stringn(
            row.x,
            row.y,
            rate.label,
            usize::from(row.width),
            Style::new().fg(rate.color),
        );
        let value = Rect::new(
            row.x.saturating_add(label_width).min(row.right()),
            row.y,
            RATE_VALUE_WIDTH.min(row.width.saturating_sub(label_width)),
            1,
        );
        draw_value(
            frame.buffer_mut(),
            value,
            &value_text(rate.current),
            context.theme.tone_style(cell_tone(rate.current)),
        );
        let history_x = value.right().saturating_add(2).min(row.right());
        let mut history = Rect::new(history_x, row.y, row.right() - history_x, 1);
        let peak = GapSparkline::new(rate.points, Style::new()).peak(history.width);
        if let Some(peak) = peak {
            let text = format!("peak {}", format_value(peak, CanonicalUnit::BytesPerSecond));
            let width = saturate_u16(display_width(&text));
            if history.width > width.saturating_add(MIN_HISTORY_WIDTH + 2) {
                history.width -= width + 2;
                frame.buffer_mut().set_string(
                    history.right() + 2,
                    row.y,
                    &text,
                    context.theme.muted_style(),
                );
            }
        }
        if peak.is_none() {
            frame.buffer_mut().set_stringn(
                history.x,
                history.y,
                NO_HISTORY,
                usize::from(history.width),
                context.theme.muted_style(),
            );
            continue;
        }
        frame.render_widget(
            GapSparkline::new(rate.points, Style::new().fg(rate.color)),
            history,
        );
    }
}

/// Header row whose titles each have their own style (a chart legend).
pub(crate) fn draw_colored_header(
    buffer: &mut Buffer,
    row: Rect,
    specs: &[ColumnSpec],
    columns: &[FittedColumn],
    style_of: impl Fn(usize) -> Style,
) {
    for column in columns {
        let title = specs.get(column.spec).map_or("", |spec| spec.title);
        draw_aligned(buffer, row, column, title, style_of(column.spec));
    }
}

/// Horizontal split with a one-column gutter.
pub(crate) fn split_columns(area: Rect, left_width: u16) -> (Rect, Rect) {
    let left_width = left_width.min(area.width);
    let left = Rect::new(area.x, area.y, left_width, area.height);
    let right_x = area.x.saturating_add(left_width).saturating_add(1);
    let right = Rect::new(
        right_x.min(area.right()),
        area.y,
        area.right().saturating_sub(right_x),
        area.height,
    );
    (left, right)
}

/// Text alignment inside a table column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Align {
    Left,
    Right,
}

/// Declarative column: dropped by priority when the width is short, grown
/// toward `preferred` before flexible columns absorb the rest.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ColumnSpec {
    pub title: &'static str,
    pub min_width: u16,
    pub preferred: u16,
    pub priority: u8,
    pub flex: bool,
    pub align: Align,
    /// One space after the column before it, never a separator between
    /// them: a marker and what it marks.
    pub attached: bool,
}

impl ColumnSpec {
    pub const fn left(title: &'static str, min_width: u16, priority: u8) -> Self {
        Self {
            title,
            min_width,
            preferred: min_width,
            priority,
            flex: false,
            align: Align::Left,
            attached: false,
        }
    }

    pub const fn right(title: &'static str, min_width: u16, priority: u8) -> Self {
        Self {
            title,
            min_width,
            preferred: min_width,
            priority,
            flex: false,
            align: Align::Right,
            attached: false,
        }
    }

    pub const fn attached(mut self) -> Self {
        self.attached = true;
        self
    }

    pub const fn flexible(mut self) -> Self {
        self.flex = true;
        self
    }

    pub const fn prefer(mut self, width: u16) -> Self {
        self.preferred = width;
        self
    }
}

/// A column chosen by [`fit_columns`]: `spec` indexes the input specs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FittedColumn {
    pub spec: usize,
    pub x: u16,
    pub width: u16,
    pub align: Align,
    /// A `│` goes in the gap before it ([`fit_separated_columns`]).
    pub separated: bool,
}

const COLUMN_SPACING: u16 = 1;
/// The gap ` │ ` between separated columns.
const SEPARATED_SPACING: u16 = 3;
const COLUMN_SEPARATOR: &str = "│";
/// Columns below this priority are optional: they give way when a more
/// important column cannot reach its preferred width.
const OPTIONAL_PRIORITY: u8 = 50;

/// How far apart a table's columns are set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Spacing {
    Space,
    Separator,
}

impl Spacing {
    /// The gap before `spec` when a column comes before it.
    fn before(self, spec: &ColumnSpec) -> u16 {
        match self {
            Self::Separator if !spec.attached => SEPARATED_SPACING,
            _ => COLUMN_SPACING,
        }
    }
}

/// Keeps the highest-priority columns that fit, in display order. Leftover
/// width first grows fixed columns toward their preferred width (by
/// priority), then flexible ones, and the remainder goes to flexible columns
/// (or the last column).
pub(crate) fn fit_columns(specs: &[ColumnSpec], available: u16) -> Vec<FittedColumn> {
    fit_spaced_columns(specs, available, Spacing::Space)
}

/// [`fit_columns`] with ` │ ` between the columns, attached ones aside;
/// [`draw_column_separators`] draws the `│`.
pub(crate) fn fit_separated_columns(specs: &[ColumnSpec], available: u16) -> Vec<FittedColumn> {
    fit_spaced_columns(specs, available, Spacing::Separator)
}

fn fit_spaced_columns(specs: &[ColumnSpec], available: u16, spacing: Spacing) -> Vec<FittedColumn> {
    let mut chosen = choose_columns(specs, available, spacing);
    loop {
        let widths = grow_columns(specs, &chosen, available, spacing);
        let starved = chosen.iter().zip(&widths).any(|(index, width)| {
            let spec = &specs[*index];
            spec.priority >= OPTIONAL_PRIORITY && *width < spec.preferred
        });
        let optional = chosen
            .iter()
            .enumerate()
            .filter(|(_, index)| specs[**index].priority < OPTIONAL_PRIORITY)
            .min_by_key(|(_, index)| specs[**index].priority)
            .map(|(position, _)| position);
        match (starved, optional) {
            (true, Some(position)) => {
                chosen.remove(position);
            }
            _ => return position_columns(specs, &chosen, &widths, spacing),
        }
    }
}

/// Indices (display order) of the columns whose minimum widths fit.
fn choose_columns(specs: &[ColumnSpec], available: u16, spacing: Spacing) -> Vec<usize> {
    let mut by_priority: Vec<usize> = (0..specs.len()).collect();
    by_priority.sort_by(|a, b| specs[*b].priority.cmp(&specs[*a].priority).then(a.cmp(b)));
    let mut chosen = vec![false; specs.len()];
    let mut used = 0u16;
    for index in by_priority {
        let gap = if used == 0 {
            0
        } else {
            spacing.before(&specs[index])
        };
        let needed = specs[index].min_width.saturating_add(gap);
        if used.saturating_add(needed) <= available {
            used = used.saturating_add(needed);
            chosen[index] = true;
        }
    }
    (0..specs.len()).filter(|index| chosen[*index]).collect()
}

fn grow_columns(
    specs: &[ColumnSpec],
    chosen: &[usize],
    available: u16,
    spacing: Spacing,
) -> Vec<u16> {
    let mut widths: Vec<u16> = chosen.iter().map(|index| specs[*index].min_width).collect();
    let gaps = chosen.iter().skip(1).fold(0u16, |sum, index| {
        sum.saturating_add(spacing.before(&specs[*index]))
    });
    let used = widths
        .iter()
        .fold(gaps, |sum, width| sum.saturating_add(*width));
    let mut leftover = available.saturating_sub(used);
    let mut growth_order: Vec<usize> = (0..chosen.len()).collect();
    growth_order.sort_by(|a, b| {
        let (left, right) = (&specs[chosen[*a]], &specs[chosen[*b]]);
        left.flex
            .cmp(&right.flex)
            .then(right.priority.cmp(&left.priority))
            .then(a.cmp(b))
    });
    for position in growth_order {
        let grow = specs[chosen[position]]
            .preferred
            .saturating_sub(widths[position])
            .min(leftover);
        widths[position] += grow;
        leftover -= grow;
    }
    let flex_positions: Vec<usize> = (0..chosen.len())
        .filter(|position| specs[chosen[*position]].flex)
        .collect();
    let receivers = if flex_positions.is_empty() {
        chosen.len().checked_sub(1).into_iter().collect()
    } else {
        flex_positions
    };
    if !receivers.is_empty() {
        let share = leftover / saturate_u16(receivers.len());
        for position in &receivers {
            widths[*position] = widths[*position].saturating_add(share);
            leftover = leftover.saturating_sub(share);
        }
        if let Some(last) = receivers.last() {
            widths[*last] = widths[*last].saturating_add(leftover);
        }
    }
    widths
}

fn position_columns(
    specs: &[ColumnSpec],
    chosen: &[usize],
    widths: &[u16],
    spacing: Spacing,
) -> Vec<FittedColumn> {
    let mut x = 0u16;
    chosen
        .iter()
        .zip(widths)
        .enumerate()
        .map(|(position, (index, width))| {
            let spec = &specs[*index];
            if position > 0 {
                x = x.saturating_add(spacing.before(spec));
            }
            let column = FittedColumn {
                spec: *index,
                x,
                width: *width,
                align: spec.align,
                separated: position > 0 && spacing.before(spec) == SEPARATED_SPACING,
            };
            x = x.saturating_add(*width);
            column
        })
        .collect()
}

/// One table cell's text and style.
#[derive(Debug, Clone)]
pub(crate) struct TableCell {
    pub text: String,
    pub style: Style,
    /// Parts of `text` in a color of their own, which they keep on a
    /// highlighted row: each GPU the cell names in its color (D-123), a
    /// process's swatch in the process's.
    pub tints: Vec<Tint>,
}

/// Columns of a cell's text, counted from its start, in a color of their
/// own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tint {
    pub columns: Range<usize>,
    pub color: Color,
}

impl TableCell {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            tints: Vec::new(),
        }
    }

    /// `text` all in `color`, which a highlighted row keeps: a process's
    /// swatch.
    pub fn colored(text: impl Into<String>, color: Color) -> Self {
        Self::of_parts([(text.into(), Some(color))], Style::new().fg(color))
    }

    /// A cell of `parts` in a row, each in its own color, else in `style`.
    pub fn of_parts(
        parts: impl IntoIterator<Item = (String, Option<Color>)>,
        style: Style,
    ) -> Self {
        let mut cell = Self::new(String::new(), style);
        for (part, color) in parts {
            let start = display_width(&cell.text);
            cell.text.push_str(&part);
            if let Some(color) = color {
                cell.tints.push(Tint {
                    columns: start..display_width(&cell.text),
                    color,
                });
            }
        }
        cell
    }
}

/// Table cell for a metric: value text, or a state label that fits `width`.
pub(crate) fn metric_table_cell(
    cell: &MetricCell,
    palette: &ThemePalette,
    width: usize,
) -> TableCell {
    let text = if cell.state.has_value() && cell.value.is_some() {
        value_text(cell)
    } else {
        state_label_for_width(cell.state, width).to_owned()
    };
    TableCell::new(text, palette.tone_style(cell_tone(cell)))
}

/// Header row of a fitted table.
pub(crate) fn draw_table_header(
    buffer: &mut Buffer,
    row: Rect,
    specs: &[ColumnSpec],
    columns: &[FittedColumn],
    style: Style,
) {
    for column in columns {
        let title = specs.get(column.spec).map_or("", |spec| spec.title);
        draw_aligned(buffer, row, column, title, style);
    }
}

/// Body row of a fitted table; `cells` is indexed like the specs.
pub(crate) fn draw_table_row(
    buffer: &mut Buffer,
    row: Rect,
    columns: &[FittedColumn],
    cells: &[TableCell],
    row_style: Option<Style>,
) {
    if let Some(style) = row_style {
        buffer.set_style(row, style);
    }
    for column in columns {
        if let Some(cell) = cells.get(column.spec) {
            let style = row_style.map_or(cell.style, |row| cell.style.patch(row));
            draw_aligned(buffer, row, column, &cell.text, style);
            draw_tints(buffer, row, column, cell);
        }
    }
}

/// Recolors the parts of `cell` its tints name where `column` shows them;
/// what the column's width cuts off is not shown, so not tinted either.
fn draw_tints(buffer: &mut Buffer, row: Rect, column: &FittedColumn, cell: &TableCell) {
    let x = row.x.saturating_add(column.x);
    if cell.tints.is_empty() || x >= row.right() || row.height == 0 {
        return;
    }
    let width = usize::from(column.width.min(row.right() - x));
    let fitted = truncate_to_width(&cell.text, width);
    let fitted_width = display_width(&fitted);
    // A cut text ends with the ellipsis, which no part owns.
    let shown = if fitted == cell.text {
        fitted_width
    } else {
        fitted_width.saturating_sub(1)
    };
    let start = match column.align {
        Align::Left => 0,
        Align::Right => width.saturating_sub(fitted_width),
    };
    for tint in &cell.tints {
        for offset in tint.columns.start..tint.columns.end.min(shown) {
            let at = x.saturating_add(saturate_u16(start + offset));
            // Like the text, what lies past the buffer is not drawn.
            if let Some(drawn) = buffer.cell_mut((at, row.y)) {
                drawn.set_fg(tint.color);
            }
        }
    }
}

/// The `│` in the gap before each separated column on `row`.
pub(crate) fn draw_column_separators(
    buffer: &mut Buffer,
    row: Rect,
    columns: &[FittedColumn],
    style: Style,
) {
    for column in columns.iter().filter(|column| column.separated) {
        let x = row.x.saturating_add(column.x).saturating_sub(2);
        if x < row.right() && row.height > 0 {
            buffer.set_stringn(x, row.y, COLUMN_SEPARATOR, 1, style);
        }
    }
}

fn draw_aligned(buffer: &mut Buffer, row: Rect, column: &FittedColumn, text: &str, style: Style) {
    let x = row.x.saturating_add(column.x);
    if x >= row.right() || row.height == 0 {
        return;
    }
    let width = column.width.min(row.right() - x);
    let fitted = match column.align {
        Align::Left => pad_right(text, usize::from(width)),
        Align::Right => pad_left(text, usize::from(width)),
    };
    buffer.set_stringn(x, row.y, fitted, usize::from(width), style);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{display_context, missing, ok};
    use dgxtop_core::view::CellState;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Cells after `draw` ran on a `width`×`height` terminal.
    fn render_area(width: u16, height: u16, draw: impl FnOnce(&mut Frame<'_>, Rect)) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, Rect::new(0, 0, width, height)))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    /// A temperature read as it should be is written in the level color
    /// of its share of 100 °C (D-104); a stale or missing one keeps the
    /// tone that says so.
    #[test]
    fn a_temperature_is_written_in_its_level_color() {
        let context = display_context();
        let theme = &context.theme;
        let hot = ok(83.0, dgxtop_core::metric::CanonicalUnit::DegC);
        let spans = temperature_spans(&hot, CellDetail::GLANCE, &context);
        assert_eq!(spans[0].content, "83°C");
        assert_eq!(spans[0].style.fg, Some(theme.temperature_color(83.0)));
        assert_ne!(
            spans[0].style.fg,
            Some(theme.gauge_high),
            "red only at 100 °C"
        );
        let stale =
            crate::test_fixtures::stale(dgxtop_core::metric::CanonicalUnit::DegC, 83.0, 9_000);
        let spans = temperature_spans(&stale, CellDetail::GLANCE, &context);
        assert_eq!(
            spans[0].style,
            theme.tone_style(crate::format::CellTone::Stale)
        );
        let gone = missing(
            dgxtop_core::metric::CanonicalUnit::DegC,
            CellState::ReadFailed,
        );
        let spans = labeled_temperature("temp", &gone, CellDetail::GLANCE, &context);
        assert_eq!(
            spans[1].style,
            theme.tone_style(crate::format::CellTone::Missing)
        );
    }

    #[test]
    fn a_value_wider_than_its_column_is_never_cut() {
        let context = display_context();
        let buffer = render_area(40, 1, |frame, row| {
            render_bar_row(
                frame,
                row,
                &BarRow {
                    label: "used",
                    ratio: None,
                    value: "12.3 GiB",
                    value_width: 6,
                },
                &context,
            );
        });
        let text = row_text(&buffer, 0);
        assert!(text.ends_with(" 12.3 GiB"), "{text:?}");
        let value_x = saturate_u16(text.find("12.3").unwrap());
        assert_eq!(
            buffer[(value_x, 0)].fg,
            context.theme.muted_style().fg.unwrap()
        );
    }

    #[test]
    fn the_chart_under_a_bar_colors_every_column_by_its_level() {
        let context = display_context();
        let palette = &context.theme;
        let history = [Some(10.0), None, Some(95.0)];
        let buffer = render_area(10, 2, |frame, area| {
            render_chart(frame, area, &history, Some(100.0), &context);
        });
        // Two rows less the headroom give 14 levels: the newest sample
        // (95 %) at the right edge fills the bottom row and most of the top
        // one; the gap stays blank; 10 % is a low step on the bottom row.
        let cell = |x: u16, y: u16| (buffer[(x, y)].symbol(), buffer[(x, y)].fg);
        assert_eq!(cell(9, 1), ("█", palette.level_color(0.95)));
        assert_eq!(cell(9, 0), ("▅", palette.level_color(0.95)));
        assert_eq!((cell(8, 0).0, cell(8, 1).0), (" ", " "));
        assert_eq!(cell(7, 1), ("▁", palette.level_color(0.1)));
        assert_eq!(cell(7, 0).0, " ");
        let empty = render_area(20, 3, |frame, area| {
            render_chart(frame, area, &[None], Some(100.0), &context);
        });
        assert!(row_text(&empty, 2).starts_with("no trend data"));
    }

    #[test]
    fn rate_rows_scale_to_their_own_peak_and_name_it() {
        let context = display_context();
        let palette = &context.theme;
        let mib = 1024.0 * 1024.0;
        let read = ok(mib, CanonicalUnit::BytesPerSecond);
        let write = missing(CanonicalUnit::BytesPerSecond, CellState::Warmup);
        let read_points = [Some(0.0), Some(0.5 * mib), Some(mib)];
        let rates = [
            RateHistory {
                label: "R",
                color: palette.success,
                current: &read,
                points: &read_points,
            },
            RateHistory {
                label: "W",
                color: palette.warning,
                current: &write,
                points: &[],
            },
        ];
        let buffer = render_area(60, 2, |frame, area| {
            render_rate_rows(frame, area, &rates, &context);
        });
        let read_row = row_text(&buffer, 0);
        assert!(read_row.starts_with("R    1.0 MiB/s  "), "{read_row:?}");
        assert!(read_row.ends_with("▁▃▆  peak 1.0 MiB/s"), "{read_row:?}");
        assert_eq!(buffer[(43, 0)].fg, palette.success);
        let write_row = row_text(&buffer, 1);
        assert!(write_row.contains("no trend data"), "{write_row:?}");
        assert!(!write_row.contains("peak"), "{write_row:?}");
    }

    /// Separated columns sit ` │ ` apart, an attached one a space after the
    /// column before it, and the `│` goes in the middle of each gap.
    #[test]
    fn separated_columns_are_set_apart_by_a_bar() {
        let specs = [
            ColumnSpec::left("", 1, 90),
            ColumnSpec::left("ab", 2, 80).attached(),
            ColumnSpec::right("cde", 3, 70),
            ColumnSpec::left("f", 1, 60).flexible(),
        ];
        let columns = fit_separated_columns(&specs, 20);
        let at: Vec<(u16, u16, bool)> = columns
            .iter()
            .map(|column| (column.x, column.width, column.separated))
            .collect();
        assert_eq!(
            at,
            vec![(0, 1, false), (2, 2, false), (7, 3, true), (13, 7, true)]
        );
        let plain = fit_columns(&specs, 20);
        assert_eq!(plain[2].x, 5, "one space apart");
        assert!(plain.iter().all(|column| !column.separated));
        let buffer = render_area(20, 1, |frame, area| {
            draw_table_header(frame.buffer_mut(), area, &specs, &columns, Style::new());
            draw_column_separators(frame.buffer_mut(), area, &columns, Style::new());
        });
        assert_eq!(row_text(&buffer, 0), "  ab │ cde │ f      ");
    }

    /// A cell's tinted parts keep their colors where its column shows them,
    /// right- or left-aligned and on a highlighted row; what the column
    /// cuts off and the ellipsis keep the cell's style, and a row narrower
    /// than its columns or reaching past the buffer draws what fits (D-123).
    #[test]
    fn tinted_parts_keep_their_colors_where_they_show() {
        let (blue, pink) = (Color::Rgb(0, 0, 255), Color::Rgb(255, 0, 255));
        let gray = Style::new().fg(Color::Gray);
        let gpus = TableCell::of_parts(
            [
                ("0".to_owned(), Some(blue)),
                (",".to_owned(), None),
                ("1".to_owned(), Some(pink)),
            ],
            gray,
        );
        assert_eq!(gpus.text, "0,1");
        let cut = TableCell::of_parts(
            [("ab".to_owned(), Some(blue)), ("cd".to_owned(), Some(pink))],
            gray,
        );
        let specs = [
            ColumnSpec::right("GPU", 5, 90),
            ColumnSpec::left("x", 3, 80),
        ];
        let columns = fit_columns(&specs, 9);
        let highlight = Style::new().fg(Color::White).bg(Color::DarkGray);
        let cells = [gpus, cut];
        let buffer = render_area(9, 4, |frame, _| {
            let buffer = frame.buffer_mut();
            draw_table_row(buffer, Rect::new(0, 0, 9, 1), &columns, &cells, None);
            let row = Rect::new(0, 1, 9, 1);
            draw_table_row(buffer, row, &columns, &cells, Some(highlight));
            draw_table_row(buffer, Rect::new(0, 2, 4, 1), &columns, &cells, None);
            draw_table_row(buffer, Rect::new(6, 3, 9, 1), &columns, &cells, None);
        });
        assert_eq!(row_text(&buffer, 0), "  0,1 ab…");
        let colors =
            |y: u16, at: &[u16]| -> Vec<Color> { at.iter().map(|x| buffer[(*x, y)].fg).collect() };
        let (gray, white) = (Color::Gray, Color::White);
        let shown = [2, 3, 4, 6, 7, 8];
        assert_eq!(colors(0, &shown), vec![blue, gray, pink, blue, blue, gray]);
        assert_eq!(
            colors(1, &shown),
            vec![blue, white, pink, blue, blue, white]
        );
        assert_eq!(buffer[(2, 1)].bg, Color::DarkGray);
        assert_eq!(row_text(&buffer, 2), " 0,1     ");
        assert_eq!(colors(2, &[1, 2, 3]), vec![blue, gray, pink]);
        assert_eq!(row_text(&buffer, 3), "        0");
        assert_eq!(colors(3, &[8]), vec![blue]);
    }

    /// A panel's keys: each key bright in the accent color, its action in
    /// text color, brackets and dots muted (D-70).
    #[test]
    fn panel_keys_light_the_key_and_name_its_action() {
        let context = display_context();
        let spans = key_spans("j/k select · F9 kill", &context);
        let parts: Vec<(&str, Style)> = spans
            .iter()
            .map(|span| (span.content.as_ref(), span.style))
            .collect();
        let key = Style::new()
            .fg(context.theme.accent)
            .add_modifier(ratatui::style::Modifier::BOLD);
        let muted = context.theme.muted_style();
        let text = context.theme.text_style();
        assert_eq!(
            parts,
            vec![
                (" (", muted),
                ("j/k", key),
                (" select", text),
                (" · ", muted),
                ("F9", key),
                (" kill", text),
                (") ", muted),
            ]
        );
    }

    /// A divider (D-116) ends in the fullest note and keys that fit after
    /// a few columns of rule, the next page's titles giving way first,
    /// then the keys' arrows, then the words; with nothing to say it is a
    /// bare rule. Drawn in a bordered panel it joins the side borders.
    #[test]
    fn a_divider_ends_in_the_fullest_note_that_fits() {
        let context = display_context();
        let next = ["Tasks".to_owned(), "Disk".to_owned()];
        let forms = page_divider_forms((0, 2), &next, "panel");
        let text = |width: u16, forms: &[DividerForm]| -> String {
            divider_line(width, forms, &context)
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        let full = text(60, &forms);
        assert_eq!(full.chars().count(), 60);
        assert!(
            full.ends_with("─ page 1 of 2 · next: Tasks, Disk (j/k/↑/↓ page) "),
            "{full}"
        );
        assert!(full.starts_with("────"), "{full}");
        assert!(text(50, &forms).ends_with("─ page 1 of 2 · next: 2 panels (j/k/↑/↓ page) "));
        assert!(text(32, &forms).ends_with("─ page 1 of 2 (j/k/↑/↓ page) "));
        assert!(text(30, &forms).ends_with("─ page 1 of 2 (j/k page) "));
        assert!(text(22, &forms).ends_with("─ 1/2 (j/k page) "));
        assert!(text(9, &forms).ends_with("─ 1/2 "));
        assert_eq!(text(5, &forms), "─────", "no room for any note");
        let last = page_divider_forms((1, 2), &[], "panel");
        assert!(text(60, &last).ends_with("─ page 2 of 2 (j/k/↑/↓ page) "));
        assert!(page_divider_forms((0, 1), &next, "panel").is_empty());
        assert_eq!(text(8, &[]), "────────");
        // In a bordered panel the divider replaces the borders' `│`.
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 3));
        let inner = Rect::new(1, 1, 10, 1);
        draw_divider(&mut buffer, inner, &[], &context);
        let row: String = (0..12).map(|x| buffer[(x, 1)].symbol()).collect();
        assert_eq!(row, "├──────────┤");
        assert_eq!(
            buffer[(0, 1)].fg,
            context.theme.border_style(false).fg.unwrap()
        );
    }

    #[test]
    fn split_columns_handles_tiny_areas() {
        let (left, right) = split_columns(Rect::new(0, 0, 10, 2), 4);
        assert_eq!(left.width, 4);
        assert_eq!(right.x, 5);
        assert_eq!(right.width, 5);
        let (left, right) = split_columns(Rect::new(0, 0, 3, 1), 5);
        assert_eq!(left.width, 3);
        assert_eq!(right.width, 0);
    }
}
