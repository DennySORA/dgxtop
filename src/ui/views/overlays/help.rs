use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::ui::theme::Theme;

/// Render the help overlay (modal).
pub fn render(frame: &mut Frame, area: Rect, theme: &Theme) {
    let popup_area = centered_rect(60, 70, area);

    // Clear the background behind the popup
    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.primary))
        .title(Span::styled(
            " Keyboard Shortcuts ",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let help_text = vec![
        Line::from(Span::styled(
            "Navigation",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )),
        help_line("Tab / Shift+Tab", "Switch between views", theme),
        help_line(
            "1 / 2 / 3",
            "Jump to Overview / GPU Detail / Processes",
            theme,
        ),
        help_line("j / k / ↑ / ↓", "Navigate up/down", theme),
        help_line("h / l / ← / →", "Select GPU (in GPU Detail view)", theme),
        Line::raw(""),
        Line::from(Span::styled(
            "Process Management",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )),
        help_line("s", "Enter sort mode (cycle columns with s/Tab)", theme),
        help_line("r", "Reverse sort order (in sort mode)", theme),
        help_line("/", "Enter filter mode (type to search)", theme),
        help_line("K", "Kill selected process (with confirmation)", theme),
        Line::raw(""),
        Line::from(Span::styled(
            "Display",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )),
        help_line("e", "Toggle per-core CPU display", theme),
        help_line("+ / -", "Increase / decrease refresh rate", theme),
        Line::raw(""),
        Line::from(Span::styled(
            "General",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )),
        help_line("?", "Toggle this help", theme),
        help_line("q / Ctrl+C", "Quit", theme),
    ];

    let paragraph = Paragraph::new(help_text).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);
}

fn help_line<'a>(key: &'a str, desc: &'a str, theme: &Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!("  {key:>18}"),
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {desc}"), Style::default().fg(theme.text)),
    ])
}

/// Create a centered rectangle of given percentage width and height.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let [_, center_v, _] = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .areas(area);

    let [_, center, _] = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .areas(center_v);

    center
}
