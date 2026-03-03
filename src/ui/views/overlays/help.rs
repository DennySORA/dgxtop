use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use crate::ui::theme::Theme;

/// Render the help overlay (modal).
pub fn render(frame: &mut Frame, area: Rect, theme: &Theme) {
    let popup_area = centered_rect(55, 70, area);

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.primary))
        .title(Span::styled(
            " Keyboard Shortcuts ",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(Span::styled(
            " Esc to close ",
            Style::default().fg(theme.text_muted),
        )));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let help_text = vec![
        Line::raw(""),
        section_header("Navigation", theme),
        help_line("Tab / Shift+Tab", "Switch between views", theme),
        help_line("1 / 2 / 3", "Jump to Overview / GPU / Processes", theme),
        help_line("j / k / ↑ / ↓", "Navigate up / down", theme),
        help_line("h / l / ← / →", "Select GPU (detail view)", theme),
        Line::raw(""),
        section_header("Process Management", theme),
        help_line("s", "Sort mode (cycle columns)", theme),
        help_line("r", "Reverse sort order", theme),
        help_line("/", "Filter processes by name", theme),
        help_line("K", "Kill selected process", theme),
        Line::raw(""),
        section_header("Display", theme),
        help_line("e", "Toggle per-core CPU bars", theme),
        help_line("+ / -", "Faster / slower refresh", theme),
        Line::raw(""),
        section_header("General", theme),
        help_line("?", "Toggle this help", theme),
        help_line("q / Ctrl+C", "Quit", theme),
    ];

    let paragraph = Paragraph::new(help_text).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);
}

fn section_header<'a>(title: &'a str, theme: &Theme) -> Line<'a> {
    Line::from(vec![Span::styled(
        format!("  {title}"),
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    )])
}

fn help_line<'a>(key: &'a str, desc: &'a str, theme: &Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("    {key:>16}"), Style::default().fg(theme.warning)),
        Span::styled(format!("  {desc}"), Style::default().fg(theme.text_dim)),
    ])
}

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
