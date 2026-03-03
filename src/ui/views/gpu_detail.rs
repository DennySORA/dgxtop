use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Sparkline};

use crate::app::AppState;
use crate::ui::theme::Theme;
use crate::ui::widgets::gradient_gauge::GradientGauge;

/// Render the GPU detail view — per-GPU cards with full metrics and history charts.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if state.gpus.is_empty() {
        let msg = Paragraph::new(" No GPUs detected").style(Style::default().fg(theme.text_dim));
        frame.render_widget(msg, area);
        return;
    }

    // Split into per-GPU cards
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

    let title = format!(" GPU {} — {} ", gpu.index, gpu.name,);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title,
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

    // Metrics column
    let [util_area, mem_area, power_area, clock_area, detail_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(metrics_area);

    // Utilization bar
    let util_label = format!("{:5.1}%", gpu.utilization_gpu);
    let gauge = GradientGauge::new(gpu.utilization_gpu / 100.0)
        .label(&util_label)
        .colors(theme.gauge_low, theme.gauge_mid, theme.gauge_high);
    frame.render_widget(
        Paragraph::new(Span::styled(" Util ", Style::default().fg(theme.text_dim))),
        Rect::new(util_area.x, util_area.y, 6, 1),
    );
    frame.render_widget(
        gauge,
        Rect::new(
            util_area.x + 6,
            util_area.y,
            util_area.width.saturating_sub(14),
            1,
        ),
    );

    // Memory bar
    let mem_pct = gpu.memory_usage_percent();
    let mem_gib_used = gpu.memory_used_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let mem_gib_total = gpu.memory_total_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let mem_label = format!("{:.1}/{:.1}G", mem_gib_used, mem_gib_total);
    let mem_gauge = GradientGauge::new(mem_pct / 100.0)
        .label(&mem_label)
        .colors(theme.gauge_low, theme.gauge_mid, theme.gauge_high);
    frame.render_widget(
        Paragraph::new(Span::styled(" VRAM ", Style::default().fg(theme.text_dim))),
        Rect::new(mem_area.x, mem_area.y, 6, 1),
    );
    frame.render_widget(
        mem_gauge,
        Rect::new(
            mem_area.x + 6,
            mem_area.y,
            mem_area.width.saturating_sub(14),
            1,
        ),
    );

    // Power
    let power_pct = gpu.power_usage_percent();
    let power_info = Line::from(vec![
        Span::styled(" Pwr  ", Style::default().fg(theme.text_dim)),
        Span::styled(
            format!(
                "{:.0}W / {:.0}W",
                gpu.power_draw_watts, gpu.power_limit_watts
            ),
            Style::default().fg(theme.percent_color(power_pct)),
        ),
        Span::styled(
            format!("  ({:.0}%)", power_pct),
            Style::default().fg(theme.text_dim),
        ),
    ]);
    frame.render_widget(Paragraph::new(power_info), power_area);

    // Clock
    let clock_info = Line::from(vec![
        Span::styled(" Clk  ", Style::default().fg(theme.text_dim)),
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
    ]);
    frame.render_widget(Paragraph::new(clock_info), clock_area);

    // Detail info (temperature, fan, ECC, PCIe)
    if detail_area.height > 0 {
        let mut details = vec![Span::styled(
            format!(" Temp: {:.0}°C", gpu.temperature),
            Style::default().fg(theme.temp_color(gpu.temperature)),
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

    // Charts column
    if charts_area.width > 5 {
        let [util_chart, mem_chart, temp_chart] = Layout::vertical([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .areas(charts_area);

        if let Some(history) = state.gpu_histories.get(index) {
            // Utilization sparkline
            let util_data = history.utilization.to_sparkline_data();
            let util_spark = Sparkline::default()
                .data(&util_data)
                .max(100)
                .style(Style::default().fg(theme.primary));
            frame.render_widget(util_spark, util_chart);

            // Memory sparkline
            let mem_data = history.memory_usage.to_sparkline_data();
            let mem_spark = Sparkline::default()
                .data(&mem_data)
                .max(100)
                .style(Style::default().fg(theme.accent));
            frame.render_widget(mem_spark, mem_chart);

            // Temperature sparkline
            let temp_data = history.temperature.to_sparkline_data();
            let temp_spark = Sparkline::default()
                .data(&temp_data)
                .max(100)
                .style(Style::default().fg(theme.warning));
            frame.render_widget(temp_spark, temp_chart);
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
