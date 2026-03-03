use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::AppState;
use crate::ui::input::InputMode;
use crate::ui::theme::Theme;

/// Render the bottom status bar with keybindings and mode indicator.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let mode_indicator = match state.input_mode {
        InputMode::Normal => {
            let keys = vec![
                key_span("q", "Quit", theme),
                sep(theme),
                key_span("Tab", "Switch View", theme),
                sep(theme),
                key_span("j/k", "Navigate", theme),
                sep(theme),
                key_span("s", "Sort", theme),
                sep(theme),
                key_span("K", "Kill", theme),
                sep(theme),
                key_span("/", "Filter", theme),
                sep(theme),
                key_span("?", "Help", theme),
                sep(theme),
                key_span("+/-", "Speed", theme),
                Span::styled(
                    format!(" [{:.1}s]", state.config.update_interval_secs),
                    Style::default().fg(theme.text_dim),
                ),
            ];
            Line::from(keys)
        }
        InputMode::ProcessSort => Line::from(vec![
            Span::styled(
                " SORT ",
                Style::default()
                    .fg(ratatui::style::Color::Black)
                    .bg(theme.warning),
            ),
            Span::raw(" "),
            key_span("s/Tab", "Cycle Column", theme),
            sep(theme),
            key_span("r", "Reverse", theme),
            sep(theme),
            key_span("Enter/Esc", "Confirm", theme),
            Span::styled(
                format!("  sorting by: {}", state.process_sort.label()),
                Style::default().fg(theme.warning),
            ),
        ]),
        InputMode::ProcessKill => Line::from(vec![
            Span::styled(
                " KILL ",
                Style::default()
                    .fg(ratatui::style::Color::Black)
                    .bg(theme.danger),
            ),
            Span::raw(" "),
            Span::styled(
                format!("Kill PID {}? ", state.process_kill_confirm.unwrap_or(0)),
                Style::default().fg(theme.danger),
            ),
            key_span("y/Enter", "Confirm", theme),
            sep(theme),
            key_span("n/Esc", "Cancel", theme),
        ]),
        InputMode::ProcessFilter => Line::from(vec![
            Span::styled(
                " FILTER ",
                Style::default()
                    .fg(ratatui::style::Color::Black)
                    .bg(theme.accent),
            ),
            Span::raw(" "),
            Span::styled(&state.process_filter, Style::default().fg(theme.text)),
            Span::styled("█", Style::default().fg(theme.primary)),
            Span::styled(
                "  (Esc to clear, Enter to apply)",
                Style::default().fg(theme.text_dim),
            ),
        ]),
        InputMode::Help | InputMode::Settings => Line::from(vec![key_span("Esc", "Close", theme)]),
    };

    frame.render_widget(Paragraph::new(mode_indicator), area);
}

fn key_span<'a>(key: &'a str, desc: &'a str, theme: &Theme) -> Span<'a> {
    // We return a single span with the key highlighted
    Span::styled(
        format!(" {key}:{desc}"),
        Style::default().fg(theme.text_dim),
    )
}

fn sep(theme: &Theme) -> Span<'static> {
    Span::styled(" │", Style::default().fg(theme.border))
}
