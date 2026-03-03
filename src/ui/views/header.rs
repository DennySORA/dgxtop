use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::AppState;
use crate::ui::theme::Theme;
use crate::ui::views::ActiveTab;

/// Render the top header bar with app title, tab bar, and system info.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let [title_area, tabs_area, info_area] = Layout::horizontal([
        Constraint::Length(12),
        Constraint::Fill(1),
        Constraint::Length(45),
    ])
    .areas(area);

    // Title
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " DGX",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "TOP ",
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
    ]));
    frame.render_widget(title, title_area);

    // Tab bar
    let tabs: Vec<Span> = ActiveTab::all()
        .iter()
        .enumerate()
        .flat_map(|(i, tab)| {
            let is_active = *tab == state.active_tab;
            let num = Span::styled(
                format!(" {} ", i + 1),
                if is_active {
                    Style::default()
                        .fg(Color::Black)
                        .bg(theme.primary)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text_dim)
                },
            );
            let label = Span::styled(
                format!("{} ", tab.label()),
                if is_active {
                    Style::default()
                        .fg(theme.primary)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text_dim)
                },
            );
            vec![num, label]
        })
        .collect();

    frame.render_widget(Paragraph::new(Line::from(tabs)), tabs_area);

    // System info
    let info_text = format!(
        "{} │ {} │ up {}",
        state.system_info.hostname,
        state
            .system_info
            .gpu_driver_version
            .as_deref()
            .unwrap_or("no GPU"),
        state.system_info.uptime_display(),
    );
    let info = Paragraph::new(Line::from(Span::styled(
        info_text,
        Style::default().fg(theme.text_dim),
    )));
    frame.render_widget(info, info_area);
}

use ratatui::style::Color;
