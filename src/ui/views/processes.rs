use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table};

use crate::app::AppState;
use crate::ui::input::InputMode;
use crate::ui::theme::Theme;

/// Render the full-screen process management view.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let [summary_area, table_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Fill(1)]).areas(area);

    render_summary(frame, summary_area, state, theme);
    render_table(frame, table_area, state, theme);
}

fn render_summary(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            " Summary ",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let total = state.gpu_processes.len();
    let total_gpu_mem: u64 = state.gpu_processes.iter().map(|p| p.gpu_memory_bytes).sum();
    let total_gpu_mem_gib = total_gpu_mem as f64 / (1024.0 * 1024.0 * 1024.0);

    let summary = Line::from(vec![
        Span::styled(" ", Style::default()),
        Span::styled(
            format!("{total}"),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" processes", Style::default().fg(theme.text_dim)),
        Span::styled("    ", Style::default()),
        Span::styled(
            format!("{total_gpu_mem_gib:.1} GB"),
            Style::default()
                .fg(theme.secondary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" GPU memory", Style::default().fg(theme.text_dim)),
        Span::styled("    ", Style::default()),
        Span::styled("sort: ", Style::default().fg(theme.text_muted)),
        Span::styled(
            format!(
                "{} {}",
                state.process_sort.label(),
                if state.process_sort_ascending {
                    "▲"
                } else {
                    "▼"
                }
            ),
            Style::default().fg(theme.warning),
        ),
        if !state.process_filter.is_empty() {
            Span::styled(
                format!("    filter: \"{}\"", state.process_filter),
                Style::default().fg(theme.accent),
            )
        } else {
            Span::raw("")
        },
    ]);
    frame.render_widget(Paragraph::new(summary), inner);
}

fn render_table(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let border_color = match state.input_mode {
        InputMode::ProcessSort => theme.warning,
        InputMode::ProcessKill => theme.danger,
        InputMode::ProcessFilter => theme.accent,
        _ => theme.border,
    };

    let title_color = match state.input_mode {
        InputMode::ProcessSort => theme.warning,
        InputMode::ProcessKill => theme.danger,
        InputMode::ProcessFilter => theme.accent,
        _ => theme.primary,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            " GPU Processes ",
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 2 {
        return;
    }

    let header = Row::new(vec![
        Cell::from("PID").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("USER").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("GPU").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("TYPE").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("GPU %").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("GPU MEM").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("CPU %").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("HOST MEM").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("COMMAND").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
    ])
    .height(1);

    let filtered = state.filtered_processes();
    let rows: Vec<Row> = filtered
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let is_selected = i == state.process_selected_index;
            let is_kill_target = state.process_kill_confirm == Some(p.pid);

            let alt_bg = if i % 2 == 1 {
                theme.row_alt_bg
            } else {
                theme.background
            };

            let style = if is_kill_target {
                Style::default()
                    .fg(theme.danger)
                    .add_modifier(Modifier::BOLD)
            } else if is_selected {
                Style::default()
                    .fg(theme.text)
                    .bg(theme.highlight_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text).bg(alt_bg)
            };

            let gpu_util_color = theme.percent_color(p.gpu_utilization);

            Row::new(vec![
                Cell::from(Span::styled(
                    format!("{}", p.pid),
                    Style::default().fg(theme.text_dim),
                )),
                Cell::from(truncate_str(&p.user, 8)),
                Cell::from(Span::styled(
                    format!("{}", p.gpu_index),
                    Style::default().fg(theme.secondary),
                )),
                Cell::from(Span::styled(
                    p.process_type.to_string(),
                    Style::default().fg(theme.text_dim),
                )),
                Cell::from(Span::styled(
                    format!("{:.0}%", p.gpu_utilization),
                    Style::default().fg(gpu_util_color),
                )),
                Cell::from(format_bytes(p.gpu_memory_bytes)),
                Cell::from(format!("{:.1}%", p.cpu_percent)),
                Cell::from(format_bytes(p.host_memory_bytes)),
                Cell::from(truncate_str(&p.command, 80)),
            ])
            .style(style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Length(4),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Fill(1),
        ],
    )
    .header(header);

    frame.render_widget(table, inner);
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.0} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if max_len == 0 {
        return String::new();
    }
    let char_count = s.chars().count();
    if char_count <= max_len {
        s.to_owned()
    } else {
        let end = s
            .char_indices()
            .nth(max_len.saturating_sub(1))
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}…", &s[..end])
    }
}
