//! Frame chrome: header, tab bar / view selector, footer key hints, the
//! "terminal too small" and "waiting" screens, module-error boxes and notes.

use dgxtop_core::commands::ViewKind;
use dgxtop_core::identity::ModuleId;
use dgxtop_core::view::{OverloadState, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

use super::banners::wrap_words;
use super::hitmap::HitMap;
use super::layout::{MIN_COLUMNS, MIN_ROWS};
use super::ui_state::{ModeView, UiState};
use crate::controller::keymap::{self, ModeKind};
use crate::format::{
    clean, display_width, format_age, format_interval_ms, format_span_seconds, truncate_to_width,
};
use crate::num::saturate_u16;
use crate::registry::HitTarget;
use crate::theme::ThemePalette;
use crate::views::{OFFERED_TABS, Tab};

const SEPARATOR: &str = " │ ";
/// Footer hints that close the row, in this order.
const CLOSING_HINTS: [&str; 2] = ["help", INTERVAL_HINT];
const INTERVAL_HINT: &str = "interval";

/// One header item; narrow terminals keep the highest priorities.
struct Segment {
    text: String,
    style: Style,
    priority: u8,
}

fn fit_segments(segments: Vec<Segment>, width: usize, separator: &str) -> Vec<Segment> {
    let mut order: Vec<usize> = (0..segments.len()).collect();
    order.sort_by(|a, b| {
        segments[*b]
            .priority
            .cmp(&segments[*a].priority)
            .then(a.cmp(b))
    });
    let mut keep = vec![false; segments.len()];
    let mut used = 0usize;
    for index in order {
        let separator_width = if used == 0 {
            0
        } else {
            display_width(separator)
        };
        let needed = display_width(&segments[index].text) + separator_width;
        if used + needed <= width {
            used += needed;
            keep[index] = true;
        }
    }
    segments
        .into_iter()
        .zip(keep)
        .filter_map(|(segment, kept)| kept.then_some(segment))
        .collect()
}

fn segments_line(segments: Vec<Segment>, separator: &'static str, style: Style) -> Line<'static> {
    let mut spans = Vec::new();
    for segment in segments {
        if !spans.is_empty() {
            spans.push(Span::styled(separator, style));
        }
        spans.push(Span::styled(segment.text, segment.style));
    }
    Line::from(spans)
}

/// Between the header's right-aligned facts: the footer's separator
/// (D-66).
const HEADER_GAP: &str = SEPARATOR;
/// A projection older than this many sampling intervals is flagged.
const OLD_PROJECTION_INTERVALS: u64 = 3;
/// Floor of the old-projection threshold.
const OLD_PROJECTION_MIN_MS: u64 = 5_000;
/// Metadata key of the GPU driver version.
const DRIVER_VERSION_KEY: &str = "driver_version";

/// What the header row shows besides the projection.
pub struct HeaderInput<'a> {
    pub model: &'a ViewModel,
    /// The tab shown; `None` for a hidden view.
    pub tab: Option<Tab>,
    /// Draw the tabs in this row (the wide tiers).
    pub show_tabs: bool,
    pub projection_age_ms: u64,
    /// Module id stamped on the tab hit targets.
    pub chrome_id: Option<&'a ModuleId>,
}

/// Header row: the brand and the tabs on the left; on the right the host,
/// GPU count, driver and uptime, and any warning (simulated namespace,
/// read-only, overload, unhealthy hosts, an old projection). Narrow rows
/// keep the highest priorities.
pub fn render_header(
    frame: &mut Frame<'_>,
    area: Rect,
    input: &HeaderInput<'_>,
    palette: &ThemePalette,
    hit_map: &mut HitMap,
) {
    if area.is_empty() {
        return;
    }
    let background = Style::new().bg(palette.bar_background);
    frame.buffer_mut().set_style(area, background);
    let brand = Line::from(Span::styled(
        " DGXTOP ",
        palette.title_style().bg(palette.bar_background),
    ));
    frame
        .buffer_mut()
        .set_line(area.x, area.y, &brand, area.width);
    let mut x = area
        .x
        .saturating_add(saturate_u16(brand.width()))
        .min(area.right());
    let facts = header_facts(input, palette);
    if input.show_tabs {
        let reserved = facts
            .iter()
            .filter(|fact| fact.priority >= HOST_PRIORITY)
            .map(|fact| display_width(&fact.text) + display_width(HEADER_GAP))
            .sum::<usize>();
        let tab_width = area
            .right()
            .saturating_sub(x)
            .saturating_sub(saturate_u16(reserved));
        let tabs = Rect::new(x, area.y, tab_width, 1);
        x = draw_tabs(frame, tabs, input.tab, palette, input.chrome_id, hit_map);
    }
    let room = usize::from(area.right().saturating_sub(x).saturating_sub(1));
    let line = segments_line(
        fit_segments(facts, room, HEADER_GAP),
        HEADER_GAP,
        palette.muted_style(),
    );
    let width = saturate_u16(line.width());
    let right_x = area.right().saturating_sub(width.saturating_add(1)).max(x);
    frame
        .buffer_mut()
        .set_line(right_x, area.y, &line, area.right().saturating_sub(right_x));
}

/// Priority of the host name; facts at or above it keep room beside the tabs.
const HOST_PRIORITY: u8 = 95;

fn header_facts(input: &HeaderInput<'_>, palette: &ThemePalette) -> Vec<Segment> {
    let header = &input.model.header;
    let warning = Style::new()
        .fg(palette.warning)
        .add_modifier(Modifier::BOLD);
    let mut facts = Vec::new();
    if let Some(namespace) = header.namespace_label.as_deref() {
        facts.push(Segment {
            text: format!("[{} · simulated data]", clean(namespace).to_uppercase()),
            style: Style::new().fg(palette.accent).add_modifier(Modifier::BOLD),
            priority: 99,
        });
    }
    if header.read_only {
        facts.push(Segment {
            text: "READ-ONLY".to_owned(),
            style: warning,
            priority: 98,
        });
    }
    if header.overload != OverloadState::Normal {
        facts.push(Segment {
            text: format!("overload {}", header.overload.label()),
            style: warning,
            priority: 97,
        });
    }
    if header.hosts_healthy < header.hosts_total {
        facts.push(Segment {
            text: format!(
                "hosts {}/{} healthy",
                header.hosts_healthy, header.hosts_total
            ),
            style: warning,
            priority: 96,
        });
    }
    if projection_is_old(input.projection_age_ms, header.effective_interval_ms) {
        facts.push(Segment {
            text: format!("updated {}", format_age(input.projection_age_ms)),
            style: warning,
            priority: 96,
        });
    }
    facts.push(Segment {
        text: clean(&header.host_name),
        style: palette.title_style(),
        priority: HOST_PRIORITY,
    });
    let cards = input
        .model
        .modules
        .gpu
        .as_deref()
        .map_or(&[][..], |gpu| gpu.cards.as_slice());
    if !cards.is_empty() {
        facts.push(Segment {
            text: format!("{}x GPU", cards.len()),
            style: palette.label_style(),
            priority: 60,
        });
    }
    if let Some(driver) = cards.iter().find_map(|card| {
        card.metadata
            .iter()
            .find(|(key, _)| key == DRIVER_VERSION_KEY)
            .map(|(_, value)| clean(value))
    }) {
        facts.push(Segment {
            text: format!("drv:{driver}"),
            style: palette.muted_style(),
            priority: 40,
        });
    }
    if let Some(uptime) = header.uptime_s {
        facts.push(Segment {
            text: format!("up {}", format_span_seconds(uptime)),
            style: palette.muted_style(),
            priority: 50,
        });
    }
    facts.push(Segment {
        text: format!("v{}", clean(&header.version)),
        style: palette.muted_style(),
        priority: 20,
    });
    facts
}

/// Whether the projection is older than a few sampling intervals.
fn projection_is_old(age_ms: u64, interval_ms: u32) -> bool {
    let threshold = (u64::from(interval_ms) * OLD_PROJECTION_INTERVALS).max(OLD_PROJECTION_MIN_MS);
    age_ms > threshold
}

/// Draws one hit target per tab from `area.x`; returns the column after the
/// last tab. Tabs that do not fit their labels show only their numbers.
fn draw_tabs(
    frame: &mut Frame<'_>,
    area: Rect,
    current: Option<Tab>,
    palette: &ThemePalette,
    chrome_id: Option<&ModuleId>,
    hit_map: &mut HitMap,
) -> u16 {
    if area.is_empty() {
        return area.x;
    }
    let full: Vec<String> = OFFERED_TABS
        .iter()
        .enumerate()
        .map(|(index, tab)| format!(" {} {} ", index + 1, tab.title()))
        .collect();
    let total: usize = full.iter().map(|label| display_width(label) + 1).sum();
    let compact = total > usize::from(area.width);
    let mut x = area.x;
    for (index, tab) in OFFERED_TABS.iter().enumerate() {
        let active = Some(*tab) == current;
        let label = if compact && !active {
            format!(" {} ", index + 1)
        } else {
            full.get(index).cloned().unwrap_or_default()
        };
        let width = saturate_u16(display_width(&label));
        if x.saturating_add(width) > area.right() {
            break;
        }
        let style = if active {
            Style::new()
                .fg(palette.tab_active_foreground)
                .bg(palette.tab_active_background)
                .add_modifier(Modifier::BOLD)
        } else {
            palette.label_style().bg(palette.bar_background)
        };
        frame.buffer_mut().set_string(x, area.y, &label, style);
        if let Some(module_id) = chrome_id {
            hit_map.push_clipped(
                HitTarget {
                    frame_revision: hit_map.frame_revision(),
                    module_id: module_id.clone(),
                    control_id: tab.control_id(),
                    entity_id: None,
                    process_key: None,
                    rect: Rect::new(x, area.y, width, 1),
                },
                area,
            );
        }
        x = x.saturating_add(width).saturating_add(1);
    }
    x.min(area.right())
}

/// `1 s`, or `2 s (effective 4 s)` while the overload control stretches it.
pub fn interval_text(model: &ViewModel) -> String {
    let header = &model.header;
    if header.configured_interval_ms == header.effective_interval_ms {
        format_interval_ms(header.configured_interval_ms)
    } else {
        format!(
            "{} (effective {})",
            format_interval_ms(header.configured_interval_ms),
            format_interval_ms(header.effective_interval_ms)
        )
    }
}

/// One-line view selector of the minimal tier.
pub fn render_selector(
    frame: &mut Frame<'_>,
    area: Rect,
    current: Option<Tab>,
    palette: &ThemePalette,
) {
    if area.is_empty() {
        return;
    }
    let tab = current.unwrap_or(Tab::Overview);
    let line = Line::from(vec![
        Span::styled(
            format!(
                " ◀ {}/{} {} ▶",
                tab.number(),
                OFFERED_TABS.len(),
                tab.title()
            ),
            palette.title_style(),
        ),
        Span::styled(
            format!("  Tab/Shift-Tab · 1-{}", OFFERED_TABS.len()),
            palette.muted_style(),
        ),
    ]);
    frame
        .buffer_mut()
        .set_line(area.x, area.y, &line, area.width);
}

fn mode_kind(mode: &ModeView) -> ModeKind {
    match mode {
        ModeView::Normal => ModeKind::Normal,
        ModeView::Filter { .. } => ModeKind::Filter,
        ModeView::Confirm { .. } => ModeKind::Confirm,
        ModeView::Help { .. } => ModeKind::Help,
    }
}

/// Filter text with a cursor bar, windowed so the cursor stays visible.
fn filter_display(buffer: &str, cursor: usize, room: usize) -> String {
    let cursor = cursor.min(buffer.len());
    let (before, after) = buffer.split_at(if buffer.is_char_boundary(cursor) {
        cursor
    } else {
        buffer.len()
    });
    let text = format!("{before}▏{after}");
    if display_width(&text) <= room {
        return text;
    }
    // Keep the tail around the cursor: drop characters from the front.
    let mut shown: String = format!("{before}▏");
    while display_width(&shown) + 1 > room && !shown.is_empty() {
        shown.remove(0);
    }
    truncate_to_width(&format!("…{shown}{after}"), room)
}

/// Footer: the quit hint first (always visible), then mode content and
/// hints; help and the sampling interval (with its current value) close the
/// row.
pub fn render_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    ui: &UiState,
    view: ViewKind,
    interval: Option<&str>,
    palette: &ThemePalette,
) {
    if area.is_empty() {
        return;
    }
    let background = Style::new().bg(palette.bar_background);
    frame.buffer_mut().set_style(area, background);
    let mode = mode_kind(&ui.mode);
    let key_style = Style::new()
        .fg(palette.primary)
        .bg(palette.bar_background)
        .add_modifier(Modifier::BOLD);
    let text_style = palette.label_style().bg(palette.bar_background);
    let hints = keymap::hints(mode, view);
    let (quit_keys, quit_hint) = hints
        .iter()
        .copied()
        .find(|(_, hint)| *hint == "quit")
        .unwrap_or(("q", "quit"));
    let mut spans = vec![
        Span::styled(format!(" {quit_keys} "), key_style),
        Span::styled(quit_hint, text_style),
    ];
    let badge = match &ui.mode {
        ModeView::Normal => None,
        ModeView::Filter { target, .. } => Some(format!(" FILTER {} ", target.label())),
        ModeView::Confirm { .. } => Some(" CONFIRM ".to_owned()),
        ModeView::Help { .. } => Some(" HELP ".to_owned()),
    };
    if let Some(badge) = badge {
        spans.push(Span::styled(SEPARATOR, text_style));
        spans.push(Span::styled(
            badge,
            Style::new()
                .fg(palette.tab_active_foreground)
                .bg(palette.warning)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let ModeView::Filter { buffer, cursor, .. } = &ui.mode {
        let used: usize = spans.iter().map(|span| display_width(&span.content)).sum();
        let room = usize::from(area.width).saturating_sub(used + 30).max(8);
        spans.push(Span::styled(
            format!(" /{}", filter_display(buffer, *cursor, room)),
            palette.text_style().bg(palette.bar_background),
        ));
    }
    let mut ordered: Vec<(&str, &str)> = hints
        .iter()
        .copied()
        .filter(|(_, hint)| *hint != "quit")
        .collect();
    ordered.sort_by_key(|(_, hint)| CLOSING_HINTS.contains(hint));
    for (keys, hint) in ordered {
        spans.push(Span::styled(SEPARATOR, text_style));
        spans.push(Span::styled(format!("{keys} "), key_style));
        spans.push(Span::styled(hint, text_style));
        if hint == INTERVAL_HINT
            && let Some(interval) = interval
        {
            spans.push(Span::styled(
                format!(" {interval}"),
                palette.text_style().bg(palette.bar_background),
            ));
        }
    }
    frame
        .buffer_mut()
        .set_line(area.x, area.y, &Line::from(spans), area.width);
}

/// Below 40×10: explain and keep the quit (and confirmation) keys visible.
pub fn render_too_small(frame: &mut Frame<'_>, area: Rect, ui: &UiState, palette: &ThemePalette) {
    if area.is_empty() {
        return;
    }
    let quit = if ui.is_confirming() {
        "q quit · y send · n cancel"
    } else {
        "q quit"
    };
    let bottom = area.bottom() - 1;
    frame.buffer_mut().set_stringn(
        area.x,
        bottom,
        quit,
        usize::from(area.width),
        palette.title_style(),
    );
    let message = format!("terminal too small (need {MIN_COLUMNS}x{MIN_ROWS})");
    let lines = wrap_words(&message, usize::from(area.width));
    for (offset, line) in lines.iter().enumerate() {
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        let y = area.y + offset;
        if y >= bottom {
            break;
        }
        frame.buffer_mut().set_stringn(
            area.x,
            y,
            line,
            usize::from(area.width),
            Style::new().fg(palette.warning),
        );
    }
}

/// Before the first projection arrives.
pub fn render_waiting(frame: &mut Frame<'_>, area: Rect, ui: &UiState, palette: &ThemePalette) {
    let mut rows = area;
    let header = Rect::new(rows.x, rows.y, rows.width, 1);
    frame
        .buffer_mut()
        .set_style(header, Style::new().bg(palette.bar_background));
    frame.buffer_mut().set_stringn(
        header.x,
        header.y,
        " DGXTOP · waiting for the first projection from the manager",
        usize::from(header.width),
        palette.title_style().bg(palette.bar_background),
    );
    rows.y += 1;
    rows.height = rows.height.saturating_sub(2);
    let middle = Rect::new(rows.x, rows.y + rows.height / 2, rows.width, 1);
    frame.buffer_mut().set_stringn(
        middle.x + 2.min(middle.width),
        middle.y,
        "waiting for data… (no values are shown until the manager publishes them)",
        usize::from(middle.width.saturating_sub(2)),
        palette.muted_style(),
    );
    let footer = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    render_footer(frame, footer, ui, ViewKind::Overview, None, palette);
}

/// Replaces a panicked module's area; other modules are unaffected.
pub fn render_module_error(
    frame: &mut Frame<'_>,
    area: Rect,
    module_id: &str,
    message: &str,
    palette: &ThemePalette,
) {
    if area.is_empty() {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(palette.danger))
        .title(Span::styled(
            format!(" module error · {module_id} "),
            Style::new().fg(palette.danger).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text = format!(
        "module error: {module_id} failed while rendering ({}); other modules keep updating",
        clean(message)
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::new().fg(palette.danger),
        )))
        .wrap(Wrap { trim: true }),
        inner,
    );
}

/// Shown where the active detail module does not fit.
pub fn render_body_note(frame: &mut Frame<'_>, area: Rect, text: &str, palette: &ThemePalette) {
    if area.is_empty() {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text.to_owned(),
            palette.muted_style(),
        )))
        .wrap(Wrap { trim: true }),
        area,
    );
}
