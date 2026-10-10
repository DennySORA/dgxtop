//! Help overlay generated from the keymap table (the same data that drives
//! dispatch and the footer).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear};

use crate::controller::keymap::{Section, section_bindings};
use crate::format::pad_right;
use crate::num::saturate_u16;
use crate::theme::ThemePalette;

const KEY_COLUMN: usize = 11;
const MAX_WIDTH: u16 = 96;

/// Every help line, section by section.
pub fn help_lines(palette: &ThemePalette) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for section in Section::ORDER {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            section.title(),
            palette.title_style(),
        )));
        for binding in section_bindings(section) {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {}", pad_right(binding.keys, KEY_COLUMN)),
                    Style::new().fg(palette.warning),
                ),
                Span::styled(binding.description, palette.label_style()),
            ]));
        }
    }
    lines
}

/// Centered help box; `scroll` is clamped to the content.
pub fn render_help(frame: &mut Frame<'_>, area: Rect, scroll: u16, palette: &ThemePalette) {
    if area.width < 10 || area.height < 4 {
        return;
    }
    let lines = help_lines(palette);
    let width = area.width.saturating_sub(4).min(MAX_WIDTH);
    let height = area
        .height
        .saturating_sub(2)
        .min(saturate_u16(lines.len()).saturating_add(2));
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(palette.primary))
        .title(Span::styled(" Keyboard shortcuts ", palette.title_style()))
        .title_bottom(Line::from(Span::styled(
            " Esc close · j/k scroll ",
            palette.muted_style(),
        )));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let visible = usize::from(inner.height);
    let max_scroll = lines.len().saturating_sub(visible);
    let start = usize::from(scroll).min(max_scroll);
    for (offset, line) in lines.iter().skip(start).take(visible).enumerate() {
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        frame
            .buffer_mut()
            .set_line(inner.x, inner.y + offset, line, inner.width);
    }
}
