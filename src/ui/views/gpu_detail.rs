use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Sparkline};

use crate::app::AppState;
use crate::ui::theme::Theme;
use crate::ui::widgets::gradient_gauge::GradientGauge;

/// Render the GPU detail view with per-GPU cards showing full metrics and charts.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if state.gpus.is_empty() {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.border))
            .title(Span::styled(
                " GPUs ",
                Style::default()
                    .fg(theme.primary)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                "  No GPUs detected",
                Style::default().fg(theme.text_dim),
            )])),
            inner,
        );
        return;
    }

    let card_height = 10u16;
    let constraints: Vec<Constraint> = state
        .gpus
        .iter()
        .map(|_| Constraint::Length(card_height))
        .chain(std::iter::once(Constraint::Fill(1)))
        .collect();

    let areas = Layout::vertical(constraints).split(area);

    for (i, gpu) in state.gpus.iter().enumerate() {
        if i >= areas.len() {
            break;
        }
        let is_selected = i == state.selected_gpu_index;
        render_gpu_detail_card(frame, areas[i], gpu, state, i, is_selected, theme);
    }
}

fn render_gpu_detail_card(
    frame: &mut Frame,
    area: Rect,
    gpu: &crate::domain::gpu::GpuStats,
    state: &AppState,
    index: usize,
    is_selected: bool,
    theme: &Theme,
) {
    let border_color = if is_selected {
        theme.border_active
    } else {
        theme.border
    };

    let title = format!("GPU {} — {}", gpu.index, gpu.name);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(if is_selected {
                    theme.primary
                } else {
                    theme.text
                })
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 4 || inner.width < 30 {
        return;
    }

    let [metrics_area, charts_area] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(inner);

    // ── Metrics column ──
    let [
        util_label,
        util_bar,
        mem_label,
        mem_bar,
        power_area,
        clock_area,
        detail_area,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(metrics_area);

    // Utilization
    let util_pct = gpu.utilization_gpu;
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Util  ", Style::default().fg(theme.text_muted)),
            Span::styled(
                format!("{util_pct:.1}%"),
                Style::default()
                    .fg(theme.percent_color(util_pct))
                    .add_modifier(Modifier::BOLD),
            ),
        ])),
        util_label,
    );
    let gauge = GradientGauge::new(util_pct / 100.0)
        .colors(theme.gauge_low, theme.gauge_mid, theme.gauge_high)
        .bg_color(theme.gauge_bg);
    frame.render_widget(
        gauge,
        Rect::new(
            util_bar.x + 1,
            util_bar.y,
            util_bar.width.saturating_sub(2),
            1,
        ),
    );

    // VRAM
    let mem_pct = gpu.memory_usage_percent();
    let mem_gib_used = gpu.memory_used_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let mem_gib_total = gpu.memory_total_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" VRAM  ", Style::default().fg(theme.text_muted)),
            Span::styled(
                format!("{mem_gib_used:.1} / {mem_gib_total:.1} GB"),
                Style::default().fg(theme.text),
            ),
            Span::styled(
                format!("  {mem_pct:.1}%"),
                Style::default()
                    .fg(theme.percent_color(mem_pct))
                    .add_modifier(Modifier::BOLD),
            ),
        ])),
        mem_label,
    );
    let mem_gauge = GradientGauge::new(mem_pct / 100.0)
        .colors(theme.gauge_low, theme.gauge_mid, theme.gauge_high)
        .bg_color(theme.gauge_bg);
    frame.render_widget(
        mem_gauge,
        Rect::new(mem_bar.x + 1, mem_bar.y, mem_bar.width.saturating_sub(2), 1),
    );

    // Power
    let power_pct = gpu.power_usage_percent();
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Power ", Style::default().fg(theme.text_muted)),
            Span::styled(
                format!(
                    "⚡ {:.0}W / {:.0}W",
                    gpu.power_draw_watts, gpu.power_limit_watts
                ),
                Style::default().fg(theme.percent_color(power_pct)),
            ),
            Span::styled(
                format!("  ({power_pct:.0}%)"),
                Style::default().fg(theme.text_dim),
            ),
        ])),
        power_area,
    );

    // Clock
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Clock ", Style::default().fg(theme.text_muted)),
            Span::styled(
                format!(
                    "{:.0} / {:.0} MHz",
                    gpu.clock_graphics_mhz, gpu.clock_max_graphics_mhz
                ),
                Style::default().fg(theme.text),
            ),
            Span::styled(
                format!("  Mem: {:.0} MHz", gpu.clock_memory_mhz),
                Style::default().fg(theme.text_dim),
            ),
        ])),
        clock_area,
    );

    // Detail
    if detail_area.height > 0 {
        let mut details = vec![Span::styled(
            format!(" Temp: {:.0}°C", gpu.temperature),
            Style::default()
                .fg(theme.temp_color(gpu.temperature))
                .add_modifier(Modifier::BOLD),
        )];

        if let Some(fan) = gpu.fan_speed {
            details.push(Span::styled(
                format!("  Fan: {fan:.0}%"),
                Style::default().fg(theme.text_dim),
            ));
        }

        if let Some(ecc) = gpu.ecc_errors_uncorrected {
            let ecc_color = if ecc > 0 { theme.danger } else { theme.success };
            details.push(Span::styled(
                format!("  ECC: {ecc}"),
                Style::default().fg(ecc_color),
            ));
        }

        if let (Some(tx), Some(rx)) = (gpu.pcie_tx_bytes_per_sec, gpu.pcie_rx_bytes_per_sec) {
            details.push(Span::styled(
                format!(
                    "  PCIe: ↑{} ↓{}",
                    format_rate(tx as f64),
                    format_rate(rx as f64)
                ),
                Style::default().fg(theme.text_dim),
            ));
        }

        frame.render_widget(Paragraph::new(Line::from(details)), detail_area);
    }

    // ── Charts column ──
    if charts_area.width > 8 {
        let [label1, chart1, label2, chart2, label3, chart3] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(charts_area);

        if let Some(history) = state.gpu_histories.get(index) {
            // Utilization chart
            frame.render_widget(
                Paragraph::new(Span::styled(
                    " Utilization",
                    Style::default().fg(theme.text_muted),
                )),
                label1,
            );
            let util_data = history.utilization.to_sparkline_data();
            frame.render_widget(
                Sparkline::default()
                    .data(&util_data)
                    .max(100)
                    .style(Style::default().fg(theme.primary)),
                chart1,
            );

            // Memory chart
            frame.render_widget(
                Paragraph::new(Span::styled(
                    " Memory",
                    Style::default().fg(theme.text_muted),
                )),
                label2,
            );
            let mem_data = history.memory_usage.to_sparkline_data();
            frame.render_widget(
                Sparkline::default()
                    .data(&mem_data)
                    .max(100)
                    .style(Style::default().fg(theme.accent)),
                chart2,
            );

            // Temperature chart
            frame.render_widget(
                Paragraph::new(Span::styled(
                    " Temperature",
                    Style::default().fg(theme.text_muted),
                )),
                label3,
            );
            let temp_data = history.temperature.to_sparkline_data();
            frame.render_widget(
                Sparkline::default()
                    .data(&temp_data)
                    .max(100)
                    .style(Style::default().fg(theme.warning)),
                chart3,
            );
        }
    }
}

fn format_rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1}GB/s", bytes_per_sec / (1024.0 * 1024.0 * 1024.0))
    } else if bytes_per_sec >= 1024.0 * 1024.0 {
        format!("{:.0}MB/s", bytes_per_sec / (1024.0 * 1024.0))
    } else {
        format!("{:.0}KB/s", bytes_per_sec / 1024.0)
    }
}
