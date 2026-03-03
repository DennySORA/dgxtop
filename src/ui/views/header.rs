use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::AppState;
use crate::ui::theme::Theme;
use crate::ui::views::ActiveTab;

/// Render the top header bar with branding, tab navigation, and system summary.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // Fill header background
    let bg_style = Style::default().bg(theme.header_bg);
    for x in area.x..area.x + area.width {
        frame.buffer_mut().set_string(x, area.y, " ", bg_style);
    }

    let [title_area, tabs_area, info_area] = Layout::horizontal([
        Constraint::Length(10),
        Constraint::Fill(1),
        Constraint::Length(50),
    ])
    .areas(area);

    // Branding
    let title = Paragraph::new(Line::from(vec![
        Span::styled(" ", Style::default().bg(theme.header_bg)),
        Span::styled(
            "DGX",
            Style::default()
                .fg(theme.primary)
                .bg(theme.header_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "TOP",
            Style::default()
                .fg(theme.text)
                .bg(theme.header_bg)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    frame.render_widget(title, title_area);

    // Tab bar with pill-style active indicator
    let mut tab_spans: Vec<Span> = Vec::new();
    for (i, tab) in ActiveTab::all().iter().enumerate() {
        let is_active = *tab == state.active_tab;

        if i > 0 {
            tab_spans.push(Span::styled(" ", Style::default().bg(theme.header_bg)));
        }

        if is_active {
            tab_spans.push(Span::styled(
                format!(" {} {} ", i + 1, tab.label()),
                Style::default()
                    .fg(theme.tab_active_fg)
                    .bg(theme.tab_active_bg)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            tab_spans.push(Span::styled(
                format!(" {} ", i + 1),
                Style::default().fg(theme.text_muted).bg(theme.header_bg),
            ));
            tab_spans.push(Span::styled(
                format!("{} ", tab.label()),
                Style::default().fg(theme.text_dim).bg(theme.header_bg),
            ));
        }
    }

    frame.render_widget(Paragraph::new(Line::from(tab_spans)), tabs_area);

    // System info bar
    let gpu_info = if state.system_info.gpu_count > 0 {
        format!("{}x GPU", state.system_info.gpu_count,)
    } else {
        "No GPU".to_owned()
    };

    let driver = state
        .system_info
        .gpu_driver_version
        .as_deref()
        .unwrap_or("-");

    let info_spans = vec![
        Span::styled(
            format!(" {} ", state.system_info.hostname),
            Style::default()
                .fg(theme.primary)
                .bg(theme.header_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {gpu_info} "),
            Style::default().fg(theme.text_dim).bg(theme.header_bg),
        ),
        Span::styled(
            format!(" drv:{driver} "),
            Style::default().fg(theme.text_muted).bg(theme.header_bg),
        ),
        Span::styled(
            format!(" up {} ", state.system_info.uptime_display()),
            Style::default().fg(theme.text_muted).bg(theme.header_bg),
        ),
    ];

    frame.render_widget(Paragraph::new(Line::from(info_spans)), info_area);
}
