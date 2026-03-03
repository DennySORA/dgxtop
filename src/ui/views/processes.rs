use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

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
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            " Process Summary ",
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
        Span::styled(
            format!(" Total: {total} processes"),
            Style::default().fg(theme.text),
        ),
        Span::styled(" │ ", Style::default().fg(theme.border)),
        Span::styled(
            format!("GPU Mem: {total_gpu_mem_gib:.1} GB"),
            Style::default().fg(theme.primary),
        ),
        Span::styled(" │ ", Style::default().fg(theme.border)),
        Span::styled(
            format!(
                "Sort: {} {}",
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
                format!(" │ Filter: \"{}\"", state.process_filter),
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

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            " GPU Processes ",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 2 {
        return;
    }

    let header = Row::new(vec![
        "PID", "USER", "GPU", "TYPE", "GPU %", "GPU MEM", "CPU %", "HOST MEM", "COMMAND",
    ])
    .style(
        Style::default()
            .fg(theme.text_dim)
            .add_modifier(Modifier::BOLD),
    )
    .height(1);

    let filtered = state.filtered_processes();
    let rows: Vec<Row> = filtered
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let is_selected = i == state.process_selected_index;
            let is_kill_target = state.process_kill_confirm == Some(p.pid);

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
                Style::default().fg(theme.text)
            };

            let gpu_util_color = if p.gpu_utilization >= 90.0 {
                theme.gauge_high
            } else if p.gpu_utilization >= 50.0 {
                theme.gauge_mid
            } else {
                theme.gauge_low
            };

            Row::new(vec![
                Cell::from(format!("{}", p.pid)),
                Cell::from(truncate_str(&p.user, 8)),
                Cell::from(format!("{}", p.gpu_index)),
                Cell::from(p.process_type.to_string()),
                Cell::from(Span::styled(
                    format!("{:.0}%", p.gpu_utilization),
                    Style::default().fg(gpu_util_color),
                )),
                Cell::from(format_bytes(p.gpu_memory_bytes)),
                Cell::from(format!("{:.1}%", p.cpu_percent)),
                Cell::from(format_bytes(p.host_memory_bytes)),
                Cell::from(truncate_str(&p.command, 60)),
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
