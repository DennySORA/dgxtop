use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, BorderType, Borders, Chart, Dataset, GraphType, Paragraph};

use crate::app::AppState;
use crate::domain::gpu::{GpuHistory, GpuStats};
use crate::ui::theme::Theme;

/// Render the GPU detail view — single selected GPU with full metrics.
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
            Paragraph::new(Line::from(Span::styled(
                "  No GPUs detected",
                Style::default().fg(theme.text_dim),
            ))),
            inner,
        );
        return;
    }

    let idx = state.selected_gpu_index.min(state.gpus.len() - 1);
    let gpu = &state.gpus[idx];

    // Title with GPU selector hint
    let gpu_selector = if state.gpus.len() > 1 {
        format!(" [{}/{}] h/l to switch ", idx + 1, state.gpus.len())
    } else {
        String::new()
    };
    let p_state = gpu.performance_state.as_deref().unwrap_or("—");
    let title = format!(
        " GPU {} — {} │ P:{p_state}{gpu_selector}",
        gpu.index, gpu.name
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border_active))
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 4 || inner.width < 40 {
        return;
    }

    // Split: metrics left, charts right
    let [left_col, right_col] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(inner);

    render_metrics_column(frame, left_col, gpu, state, idx, theme);
    render_charts_column(frame, right_col, state, idx, theme);
}

// ── Left column: all metrics ─────────────────────────────────────────

fn render_metrics_column(
    frame: &mut Frame,
    area: Rect,
    gpu: &GpuStats,
    state: &AppState,
    _idx: usize,
    theme: &Theme,
) {
    let mut lines: Vec<Line> = Vec::new();

    // ── Utilization & Memory ──
    lines.push(section_header("Utilization & Memory", theme));
    lines.push(metric_line(
        "GPU ",
        &format!("{:.1}%", gpu.utilization_gpu),
        theme.percent_color(gpu.utilization_gpu),
        theme,
    ));
    let mem_pct = gpu.memory_usage_percent();
    let mem_gib_used = gpu.memory_used_bytes as f64 / GIB;
    let mem_gib_total = gpu.memory_total_bytes as f64 / GIB;
    let mem_label = if gpu.memory_is_shared { "MGP " } else { "VRAM" };
    lines.push(metric_line(
        mem_label,
        &format!("{mem_gib_used:.1}/{mem_gib_total:.0}G ({mem_pct:.0}%)"),
        theme.percent_color(mem_pct),
        theme,
    ));

    // Memory bandwidth
    lines.push(metric_line(
        "MemBW",
        &format!(
            "{:.0}% util{}",
            gpu.utilization_memory,
            gpu.actual_mem_bandwidth_gbps()
                .map(|bw| format!(
                    " ~{bw:.0}/{:.0} GB/s",
                    gpu.theoretical_mem_bandwidth_gbps().unwrap_or(0.0)
                ))
                .unwrap_or_default()
        ),
        theme.percent_color(gpu.utilization_memory),
        theme,
    ));

    if let (Some(used), Some(total)) = (gpu.bar1_used_bytes, gpu.bar1_total_bytes) {
        let bar1_pct = if total > 0 {
            used as f64 / total as f64 * 100.0
        } else {
            0.0
        };
        lines.push(metric_line(
            "BAR1",
            &format!(
                "{:.0}/{:.0}M ({bar1_pct:.0}%)",
                used as f64 / MIB,
                total as f64 / MIB
            ),
            theme.text_dim,
            theme,
        ));
    }

    // ── Thermal & Power ──
    lines.push(Line::default());
    lines.push(section_header("Thermal & Power", theme));

    let temp_extra = match (gpu.temp_slowdown, gpu.temp_shutdown) {
        (Some(slow), Some(shut)) => format!(" (slow:{slow:.0}° shut:{shut:.0}°)"),
        _ => String::new(),
    };
    lines.push(metric_line(
        "Temp",
        &format!("{:.0}°C{temp_extra}", gpu.temperature),
        theme.temp_color(gpu.temperature),
        theme,
    ));

    if let Some(fan) = gpu.fan_speed {
        lines.push(metric_line("Fan", &format!("{fan:.0}%"), theme.text, theme));
    }

    let power_pct = gpu.power_usage_percent();
    lines.push(metric_line(
        "Power",
        &format!(
            "{:.0}/{:.0}W ({power_pct:.0}%)",
            gpu.power_draw_watts, gpu.power_limit_watts
        ),
        theme.percent_color(power_pct),
        theme,
    ));

    if let Some(energy_j) = gpu.total_energy_joules {
        let kwh = energy_j / 3_600_000.0;
        lines.push(metric_line(
            "Energy",
            &format!("{kwh:.2} kWh"),
            theme.text_dim,
            theme,
        ));
    }

    // Throttle
    let throttle_str = if gpu.throttle_reasons.is_empty() {
        "None".to_owned()
    } else {
        gpu.throttle_reasons.join(", ")
    };
    let throttle_color = if gpu.throttle_reasons.is_empty()
        || (gpu.throttle_reasons.len() == 1 && gpu.throttle_reasons[0] == "Idle")
    {
        theme.success
    } else {
        theme.danger
    };
    lines.push(metric_line(
        "Throttle",
        &throttle_str,
        throttle_color,
        theme,
    ));

    // ── Clocks ──
    lines.push(Line::default());
    lines.push(section_header("Clocks", theme));
    lines.push(metric_line(
        "Graph",
        &format!(
            "{:.0}/{:.0} MHz",
            gpu.clock_graphics_mhz, gpu.clock_max_graphics_mhz
        ),
        theme.text,
        theme,
    ));
    lines.push(metric_line(
        "SM",
        &format!("{:.0} MHz", gpu.clock_sm_mhz),
        theme.text_dim,
        theme,
    ));
    lines.push(metric_line(
        "Mem",
        &format!("{:.0} MHz", gpu.clock_memory_mhz),
        theme.text_dim,
        theme,
    ));
    if gpu.clock_video_mhz > 0.0 {
        lines.push(metric_line(
            "Video",
            &format!("{:.0} MHz", gpu.clock_video_mhz),
            theme.text_dim,
            theme,
        ));
    }

    // ── PCIe & Connectivity ──
    lines.push(Line::default());
    lines.push(section_header("PCIe & Connectivity", theme));
    if let (Some(cur_gen), Some(cur_width)) = (gpu.pcie_gen, gpu.pcie_width) {
        let max_info = match (gpu.pcie_max_gen, gpu.pcie_max_width) {
            (Some(mg), Some(mw)) if mg != cur_gen || mw != cur_width => {
                format!(" (max: Gen{mg} x{mw})")
            }
            _ => String::new(),
        };
        lines.push(metric_line(
            "PCIe",
            &format!("Gen{cur_gen} x{cur_width}{max_info}"),
            theme.text,
            theme,
        ));
    }
    if let (Some(tx), Some(rx)) = (gpu.pcie_tx_bytes_per_sec, gpu.pcie_rx_bytes_per_sec) {
        lines.push(metric_line(
            "Thru",
            &format!(
                "TX:{} RX:{}",
                format_rate(tx as f64),
                format_rate(rx as f64)
            ),
            theme.text_dim,
            theme,
        ));
    }

    // NVLink
    let my_links: Vec<_> = state
        .nvlink
        .iter()
        .filter(|l| l.gpu_index == gpu.index && l.is_active)
        .collect();
    if !my_links.is_empty() {
        let remotes: Vec<String> = my_links
            .iter()
            .filter_map(|l| l.remote_gpu_index.map(|r| format!("GPU{r}")))
            .collect();
        let unique: Vec<String> = {
            let mut v = remotes;
            v.sort();
            v.dedup();
            v
        };
        lines.push(metric_line(
            "NVLink",
            &format!("{} links → {}", my_links.len(), unique.join(", ")),
            theme.secondary,
            theme,
        ));
    }

    // Encoder/Decoder
    if gpu.encoder_utilization.is_some() || gpu.decoder_utilization.is_some() {
        let enc = gpu
            .encoder_utilization
            .map(|v| format!("{v:.0}%"))
            .unwrap_or_else(|| "—".to_owned());
        let dec = gpu
            .decoder_utilization
            .map(|v| format!("{v:.0}%"))
            .unwrap_or_else(|| "—".to_owned());
        lines.push(metric_line(
            "Enc/Dec",
            &format!("{enc} / {dec}"),
            theme.text_dim,
            theme,
        ));
    }

    // ── Health ──
    lines.push(Line::default());
    lines.push(section_header("Health & Info", theme));
    if let (Some(corr), Some(uncorr)) = (gpu.ecc_errors_corrected, gpu.ecc_errors_uncorrected) {
        let ecc_color = if uncorr > 0 {
            theme.danger
        } else if corr > 0 {
            theme.warning
        } else {
            theme.success
        };
        lines.push(metric_line(
            "ECC",
            &format!("corr:{corr} uncorr:{uncorr}"),
            ecc_color,
            theme,
        ));
    }
    if let (Some(sbe), Some(dbe)) = (gpu.retired_pages_sbe, gpu.retired_pages_dbe) {
        let ret_color = if dbe > 0 {
            theme.danger
        } else if sbe > 0 {
            theme.warning
        } else {
            theme.success
        };
        lines.push(metric_line(
            "Retired",
            &format!("SBE:{sbe} DBE:{dbe}"),
            ret_color,
            theme,
        ));
    }
    if let Some(cm) = &gpu.compute_mode {
        lines.push(metric_line("Compute", cm, theme.text_dim, theme));
    }
    if let Some(persist) = gpu.persistence_mode {
        lines.push(metric_line(
            "Persist",
            if persist { "On" } else { "Off" },
            if persist {
                theme.success
            } else {
                theme.text_muted
            },
            theme,
        ));
    }
    if let Some(uuid) = &gpu.uuid {
        let short = if uuid.len() > 20 { &uuid[..20] } else { uuid };
        lines.push(metric_line("UUID", short, theme.text_muted, theme));
    }

    // ── Per-GPU processes ──
    let gpu_procs: Vec<_> = state
        .gpu_processes
        .iter()
        .filter(|p| p.gpu_index == gpu.index)
        .collect();
    if !gpu_procs.is_empty() {
        lines.push(Line::default());
        let proc_title = format!("Processes ({})", gpu_procs.len());
        lines.push(section_header(&proc_title, theme));
        for p in gpu_procs.iter().take(6) {
            let mem_str = format_bytes(p.gpu_memory_bytes);
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<7}", p.pid),
                    Style::default().fg(theme.text_dim),
                ),
                Span::styled(
                    format!("{:>4.0}%", p.gpu_utilization),
                    Style::default().fg(theme.percent_color(p.gpu_utilization)),
                ),
                Span::styled(format!(" {mem_str:>7}"), Style::default().fg(theme.text)),
                Span::styled(
                    format!(" {}", truncate_str(&p.command, 25)),
                    Style::default().fg(theme.text_dim),
                ),
            ]));
        }
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, area);
}

// ── Right column: charts + stats ─────────────────────────────────────

fn render_charts_column(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    idx: usize,
    theme: &Theme,
) {
    let history = match state.gpu_histories.get(idx) {
        Some(h) => h,
        None => return,
    };

    // Decide how many charts fit: each needs label(1) + chart(min 2) = 3
    let available = area.height;
    let stats_h: u16 = 2; // stats at bottom
    let chart_budget = available.saturating_sub(stats_h);
    // 4 charts: util, mem, temp, power. Each gets label(1) + Fill
    let num_charts = (chart_budget / 3).min(4) as usize;

    if num_charts == 0 {
        // Just show stats
        if available >= stats_h {
            let lines = build_gpu_stats_lines(history, theme);
            frame.render_widget(Paragraph::new(lines), area);
        }
        return;
    }

    let mut constraints: Vec<Constraint> = Vec::new();
    for _ in 0..num_charts {
        constraints.push(Constraint::Length(1)); // label
        constraints.push(Constraint::Fill(1)); // chart
    }
    constraints.push(Constraint::Length(stats_h)); // stats

    let areas = Layout::vertical(constraints).split(area);

    let chart_configs: Vec<(&str, &[f64], f64, f64, ratatui::style::Color)> = vec![
        ("Utilization (%)", &[], 0.0, 100.0, theme.primary),
        ("Memory (%)", &[], 0.0, 100.0, theme.accent),
        (
            "Temperature (°C)",
            &[],
            0.0,
            history.temperature.max_value().max(100.0),
            theme.warning,
        ),
        (
            "Power (W)",
            &[],
            0.0,
            history.power.max_value().max(1.0),
            theme.danger,
        ),
    ];

    let chart_data_sources = [
        history.utilization.to_chart_data(),
        history.memory_usage.to_chart_data(),
        history.temperature.to_chart_data(),
        history.power.to_chart_data(),
    ];

    for i in 0..num_charts {
        let label_area = areas[i * 2];
        let chart_area = areas[i * 2 + 1];
        let (label, _, y_min, y_max, color) = &chart_configs[i];

        frame.render_widget(
            Paragraph::new(Span::styled(
                format!(" {label}"),
                Style::default().fg(theme.text_muted),
            )),
            label_area,
        );

        render_metric_chart(
            frame,
            chart_area,
            &chart_data_sources[i],
            *y_min,
            *y_max,
            *color,
            theme,
        );
    }

    // Stats at bottom
    let stats_area = areas[num_charts * 2];
    if stats_area.height > 0 {
        let lines = build_gpu_stats_lines(history, theme);
        frame.render_widget(Paragraph::new(lines), stats_area);
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const MIB: f64 = 1024.0 * 1024.0;

fn section_header(title: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        format!(" ── {title} "),
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    ))
}

fn metric_line(
    label: &str,
    value: &str,
    value_color: ratatui::style::Color,
    theme: &Theme,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {label:<7} "),
            Style::default().fg(theme.text_muted),
        ),
        Span::styled(value.to_owned(), Style::default().fg(value_color)),
    ])
}

fn render_metric_chart(
    frame: &mut Frame,
    area: Rect,
    data: &[(f64, f64)],
    y_min: f64,
    y_max: f64,
    color: ratatui::style::Color,
    theme: &Theme,
) {
    if area.height == 0 || area.width < 4 || data.is_empty() {
        return;
    }

    let x_bound = data.len().max(1) as f64;
    let dataset = Dataset::default()
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(color))
        .data(data);

    let chart = Chart::new(vec![dataset])
        .x_axis(
            Axis::default()
                .style(Style::default().fg(theme.text_muted))
                .bounds([0.0, x_bound]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(theme.text_muted))
                .labels(vec![
                    Span::raw(format!("{y_min:.0}")),
                    Span::raw(format!("{y_max:.0}")),
                ])
                .bounds([y_min, y_max]),
        );
    frame.render_widget(chart, area);
}

fn build_gpu_stats_lines<'a>(history: &GpuHistory, theme: &Theme) -> Vec<Line<'a>> {
    let elapsed = history.utilization_agg.elapsed_hours();
    let windows: Vec<usize> = [1, 6, 12, 24]
        .iter()
        .copied()
        .filter(|&h| elapsed >= h as f64)
        .collect();

    if windows.is_empty() {
        return vec![];
    }

    windows
        .iter()
        .map(|&h| {
            let label = format!("{h:>2}h");
            let u_avg = history.utilization_agg.average_over_hours(h);
            let u_max = history.utilization_agg.max_over_hours(h);
            let t_avg = history.temperature_agg.average_over_hours(h);
            let t_max = history.temperature_agg.max_over_hours(h);
            let p_avg = history.power_agg.average_over_hours(h);
            let m_avg = history.memory_agg.average_over_hours(h);

            Line::from(vec![
                Span::styled(format!(" {label}"), Style::default().fg(theme.text_muted)),
                Span::styled("  util ", Style::default().fg(theme.text_dim)),
                Span::styled(
                    format!("{u_avg:.0}%"),
                    Style::default().fg(theme.percent_color(u_avg)),
                ),
                Span::styled(format!("/{u_max:.0}%"), Style::default().fg(theme.text_dim)),
                Span::styled("  temp ", Style::default().fg(theme.text_dim)),
                Span::styled(
                    format!("{t_avg:.0}°"),
                    Style::default().fg(theme.temp_color(t_avg)),
                ),
                Span::styled(
                    format!("/{t_max:.0}°"),
                    Style::default().fg(theme.temp_color(t_max)),
                ),
                Span::styled("  pwr ", Style::default().fg(theme.text_dim)),
                Span::styled(format!("{p_avg:.0}W"), Style::default().fg(theme.text)),
                Span::styled("  mem ", Style::default().fg(theme.text_dim)),
                Span::styled(
                    format!("{m_avg:.0}%"),
                    Style::default().fg(theme.percent_color(m_avg)),
                ),
            ])
        })
        .collect()
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

fn format_bytes(bytes: u64) -> String {
    if bytes as f64 >= GIB {
        format!("{:.1}GB", bytes as f64 / GIB)
    } else if bytes as f64 >= MIB {
        format!("{:.0}MB", bytes as f64 / MIB)
    } else {
        format!("{:.0}KB", bytes as f64 / 1024.0)
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
