use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::AppState;
use crate::ui::input::InputMode;
use crate::ui::theme::Theme;

/// Render the bottom status bar with keybindings and mode indicator.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // Fill footer background
    let bg = theme.header_bg;
    for x in area.x..area.x + area.width {
        frame
            .buffer_mut()
            .set_string(x, area.y, " ", Style::default().bg(bg));
    }

    let mode_line = match state.input_mode {
        InputMode::Normal => {
            let keys = vec![
                key_badge("q", "Quit", theme),
                spacer(theme),
                key_badge("Tab", "View", theme),
                spacer(theme),
                key_badge("j/k", "Nav", theme),
                spacer(theme),
                key_badge("s", "Sort", theme),
                spacer(theme),
                key_badge("K", "Kill", theme),
                spacer(theme),
                key_badge("/", "Filter", theme),
                spacer(theme),
                key_badge("e", "Cores", theme),
                spacer(theme),
                key_badge("n", "Net", theme),
                spacer(theme),
                key_badge("d", "Disk", theme),
                spacer(theme),
                key_badge("?", "Help", theme),
                spacer(theme),
                key_badge("+/-", "Speed", theme),
                Span::styled(
                    format!("  {:.1}s", state.config.update_interval_secs),
                    Style::default().fg(theme.text_muted).bg(bg),
                ),
            ];
            Line::from(keys)
        }
        InputMode::ProcessSort => Line::from(vec![
            mode_badge(" SORT ", theme.warning, theme),
            Span::styled(" ", Style::default().bg(bg)),
            key_badge("s", "Next", theme),
            spacer(theme),
            key_badge("r", "Reverse", theme),
            spacer(theme),
            key_badge("Esc", "Done", theme),
            Span::styled(
                format!(
                    "  sorting: {} {}",
                    state.process_sort.label(),
                    if state.process_sort_ascending {
                        "▲"
                    } else {
                        "▼"
                    }
                ),
                Style::default().fg(theme.warning).bg(bg),
            ),
        ]),
        InputMode::ProcessKill => Line::from(vec![
            mode_badge(" KILL ", theme.danger, theme),
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(
                format!("Kill PID {}? ", state.process_kill_confirm.unwrap_or(0)),
                Style::default()
                    .fg(theme.danger)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            key_badge("y", "Confirm", theme),
            spacer(theme),
            key_badge("Esc", "Cancel", theme),
        ]),
        InputMode::ProcessFilter => Line::from(vec![
            mode_badge(" FILTER ", theme.accent, theme),
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(
                &state.process_filter,
                Style::default().fg(theme.text).bg(bg),
            ),
            Span::styled("▎", Style::default().fg(theme.primary).bg(bg)),
            Span::styled(
                "  Esc:clear Enter:apply",
                Style::default().fg(theme.text_muted).bg(bg),
            ),
        ]),
        InputMode::Help | InputMode::Settings => Line::from(vec![key_badge("Esc", "Close", theme)]),
    };

    frame.render_widget(Paragraph::new(mode_line), area);
}

fn key_badge<'a>(key: &'a str, desc: &'a str, theme: &Theme) -> Span<'a> {
    Span::styled(
        format!(" {key} {desc} "),
        Style::default().fg(theme.text_dim).bg(theme.header_bg),
    )
}

fn mode_badge<'a>(text: &'a str, color: ratatui::style::Color, theme: &Theme) -> Span<'a> {
    Span::styled(
        text,
        Style::default()
            .fg(theme.header_bg)
            .bg(color)
            .add_modifier(Modifier::BOLD),
    )
}

fn spacer(theme: &Theme) -> Span<'static> {
    Span::styled("│", Style::default().fg(theme.border).bg(theme.header_bg))
}
