use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table};

use crate::app::AppState;
use crate::ui::theme::Theme;
use crate::ui::widgets::gradient_gauge::GradientGauge;

/// Render the main overview dashboard (btop-inspired).
pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // Adaptive layout: top metrics + bottom process table
    let gpu_count = state.gpus.len();
    let gpu_panel_height = if gpu_count == 0 {
        0
    } else {
        (gpu_count as u16 * 3 + 2).min(14)
    };

    let [top_area, mid_area, bottom_area] = Layout::vertical([
        Constraint::Length(8),                // CPU + Memory
        Constraint::Length(gpu_panel_height), // GPUs
        Constraint::Fill(1),                  // Process table + I/O
    ])
    .areas(area);

    render_cpu_memory_row(frame, top_area, state, theme);

    if gpu_count > 0 {
        render_gpu_panel(frame, mid_area, state, theme);
    }

    render_bottom_section(frame, bottom_area, state, theme);
}

fn render_cpu_memory_row(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let [cpu_area, mem_area] =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(area);

    render_cpu_panel(frame, cpu_area, state, theme);
    render_memory_panel(frame, mem_area, state, theme);
}

fn render_cpu_panel(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            " CPU ",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let cpu = match &state.cpu {
        Some(c) => c,
        None => return,
    };

    let [info_area, bar_area, sparkline_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(if state.show_per_core {
            cpu.core_count.min(8) as u16 + 1
        } else {
            2
        }),
        Constraint::Fill(1),
    ])
    .areas(inner);

    // Info line: usage%, temp, freq, cores
    let temp_str = cpu
        .temperature_celsius
        .map(|t| format!("{t:.0}°C"))
        .unwrap_or_else(|| "N/A".to_owned());
    let temp_color = cpu
        .temperature_celsius
        .map(|t| theme.temp_color(t))
        .unwrap_or(theme.text_dim);

    let info = Line::from(vec![
        Span::styled(
            format!(" {:.1}%", cpu.usage_percent),
            Style::default()
                .fg(theme.percent_color(cpu.usage_percent))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" │ ", Style::default().fg(theme.border)),
        Span::styled(temp_str, Style::default().fg(temp_color)),
        Span::styled(" │ ", Style::default().fg(theme.border)),
        Span::styled(
            format!("{:.0}/{:.0} MHz", cpu.frequency_mhz, cpu.frequency_max_mhz),
            Style::default().fg(theme.text_dim),
        ),
        Span::styled(" │ ", Style::default().fg(theme.border)),
        Span::styled(
            format!("{} cores", cpu.core_count),
            Style::default().fg(theme.text_dim),
        ),
    ]);
    frame.render_widget(Paragraph::new(info), info_area);

    if state.show_per_core {
        // Per-core bars
        let max_cores = bar_area.height as usize;
        for (i, core) in cpu.cores.iter().take(max_cores).enumerate() {
            let y = bar_area.y + i as u16;
            let core_area = Rect::new(bar_area.x + 1, y, bar_area.width.saturating_sub(8), 1);
            let label = format!("{:4.0}%", core.usage_percent);
            let gauge = GradientGauge::new(core.usage_percent / 100.0)
                .label(&label)
                .colors(theme.gauge_low, theme.gauge_mid, theme.gauge_high);
            frame.render_widget(gauge, core_area);
        }
    } else {
        // Aggregate bar
        let bar_inner = Rect::new(
            bar_area.x + 1,
            bar_area.y,
            bar_area.width.saturating_sub(8),
            1,
        );
        let label = format!("{:4.0}%", cpu.usage_percent);
        let gauge = GradientGauge::new(cpu.usage_percent / 100.0)
            .label(&label)
            .colors(theme.gauge_low, theme.gauge_mid, theme.gauge_high);
        frame.render_widget(gauge, bar_inner);

        // Breakdown bar
        if bar_area.height > 1 {
            let breakdown = Line::from(vec![
                Span::styled(
                    format!(" usr:{:.0}%", cpu.user_percent),
                    Style::default().fg(theme.success),
                ),
                Span::styled(
                    format!(" sys:{:.0}%", cpu.system_percent),
                    Style::default().fg(theme.danger),
                ),
                Span::styled(
                    format!(" iow:{:.0}%", cpu.iowait_percent),
                    Style::default().fg(theme.warning),
                ),
            ]);
            frame.render_widget(
                Paragraph::new(breakdown),
                Rect::new(bar_area.x, bar_area.y + 1, bar_area.width, 1),
            );
        }
    }

    // Sparkline
    if sparkline_area.height > 0 {
        let data = state.cpu_history.usage.to_sparkline_data();
        let sparkline = Sparkline::default()
            .data(&data)
            .max(100)
            .style(Style::default().fg(theme.primary));
        frame.render_widget(sparkline, sparkline_area);
    }
}

fn render_memory_panel(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            " Memory ",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let mem = match &state.memory {
        Some(m) => m,
        None => return,
    };

    let [info_area, bar_area, swap_area, sparkline_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(inner);

    let usage_pct = mem.usage_percent();
    let info = Line::from(vec![
        Span::styled(
            format!(
                " {:.1} / {:.1} GB",
                bytes_to_gib(mem.used_bytes),
                bytes_to_gib(mem.total_bytes)
            ),
            Style::default().fg(theme.text),
        ),
        Span::styled(
            format!("  ({:.1}%)", usage_pct),
            Style::default()
                .fg(theme.percent_color(usage_pct))
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(info), info_area);

    // RAM gauge
    let ram_label = format!("{:.0}%", usage_pct);
    let gauge = GradientGauge::new(usage_pct / 100.0)
        .label(&ram_label)
        .colors(theme.gauge_low, theme.gauge_mid, theme.gauge_high);
    frame.render_widget(
        gauge,
        Rect::new(
            bar_area.x + 1,
            bar_area.y,
            bar_area.width.saturating_sub(6),
            1,
        ),
    );

    // Swap
    let swap_pct = mem.swap_usage_percent();
    let swap_info = Line::from(vec![
        Span::styled(" Swap: ", Style::default().fg(theme.text_dim)),
        Span::styled(
            format!(
                "{:.1}/{:.1} GB",
                bytes_to_gib(mem.swap_used_bytes),
                bytes_to_gib(mem.swap_total_bytes)
            ),
            Style::default().fg(if swap_pct > 50.0 {
                theme.warning
            } else {
                theme.text_dim
            }),
        ),
    ]);
    frame.render_widget(Paragraph::new(swap_info), swap_area);

    // Sparkline
    if sparkline_area.height > 0 {
        let data = state.memory_history.usage.to_sparkline_data();
        let sparkline = Sparkline::default()
            .data(&data)
            .max(100)
            .style(Style::default().fg(theme.accent));
        frame.render_widget(sparkline, sparkline_area);
    }
}

fn render_gpu_panel(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            format!(" GPUs ({}) ", state.gpus.len()),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Lay out GPU cards vertically, 3 lines each
    let constraints: Vec<Constraint> = state.gpus.iter().map(|_| Constraint::Length(3)).collect();

    let gpu_areas = Layout::vertical(constraints).split(inner);

    for (i, gpu) in state.gpus.iter().enumerate() {
        if i >= gpu_areas.len() {
            break;
        }
        render_gpu_card(frame, gpu_areas[i], gpu, state, i, theme);
    }
}

fn render_gpu_card(
    frame: &mut Frame,
    area: Rect,
    gpu: &crate::domain::gpu::GpuStats,
    _state: &AppState,
    _gpu_index: usize,
    theme: &Theme,
) {
    if area.height == 0 || area.width < 20 {
        return;
    }

    let [header_line, util_line, mem_line] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    // GPU header: name, temp, power, fan
    let temp_color = theme.temp_color(gpu.temperature);
    let power_pct = gpu.power_usage_percent();

    let header = Line::from(vec![
        Span::styled(
            format!(" GPU {}: ", gpu.index),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(truncate_str(&gpu.name, 20), Style::default().fg(theme.text)),
        Span::styled(" │ ", Style::default().fg(theme.border)),
        Span::styled(
            format!("{:.0}°C", gpu.temperature),
            Style::default().fg(temp_color),
        ),
        Span::styled(" │ ", Style::default().fg(theme.border)),
        Span::styled(
            format!("{:.0}/{:.0}W", gpu.power_draw_watts, gpu.power_limit_watts),
            Style::default().fg(theme.percent_color(power_pct)),
        ),
        Span::styled(" │ ", Style::default().fg(theme.border)),
        Span::styled(
            format!("{:.0} MHz", gpu.clock_graphics_mhz),
            Style::default().fg(theme.text_dim),
        ),
        gpu.fan_speed
            .map(|f| {
                Span::styled(
                    format!(" │ Fan {f:.0}%"),
                    Style::default().fg(theme.text_dim),
                )
            })
            .unwrap_or_default(),
    ]);
    frame.render_widget(Paragraph::new(header), header_line);

    // Utilization bar
    let util_bar_width = area.width.saturating_sub(14);
    let util_label = format!(" GPU {:.0}%", gpu.utilization_gpu);
    let gauge = GradientGauge::new(gpu.utilization_gpu / 100.0)
        .label(&util_label)
        .colors(theme.gauge_low, theme.gauge_mid, theme.gauge_high);
    frame.render_widget(
        gauge,
        Rect::new(util_line.x + 1, util_line.y, util_bar_width, 1),
    );

    // Memory bar
    let mem_pct = gpu.memory_usage_percent();
    let mem_used_gib = gpu.memory_used_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let mem_total_gib = gpu.memory_total_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let mem_label = format!(" VRAM {:.1}/{:.1}GB", mem_used_gib, mem_total_gib);
    let mem_gauge = GradientGauge::new(mem_pct / 100.0)
        .label(&mem_label)
        .colors(theme.gauge_low, theme.gauge_mid, theme.gauge_high);
    frame.render_widget(
        mem_gauge,
        Rect::new(mem_line.x + 1, mem_line.y, util_bar_width, 1),
    );
}

fn render_bottom_section(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let [io_area, process_area] =
        Layout::vertical([Constraint::Length(6), Constraint::Fill(1)]).areas(area);

    render_io_row(frame, io_area, state, theme);
    render_process_table(frame, process_area, state, theme);
}

fn render_io_row(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let [disk_area, net_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);

    // Disk I/O table
    let disk_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            " Disk I/O ",
            Style::default().fg(theme.primary),
        ));
    let disk_inner = disk_block.inner(disk_area);
    frame.render_widget(disk_block, disk_area);

    let disk_header = Row::new(vec!["Device", "Read", "Write", "IOPS R/W", "Await"])
        .style(Style::default().fg(theme.text_dim));
    let disk_rows: Vec<Row> = state
        .disks
        .iter()
        .take(disk_inner.height as usize)
        .map(|d| {
            Row::new(vec![
                Cell::from(d.device_name.clone()),
                Cell::from(format_throughput(d.read_bytes_per_sec)),
                Cell::from(format_throughput(d.write_bytes_per_sec)),
                Cell::from(format!("{:.0}/{:.0}", d.read_iops, d.write_iops)),
                Cell::from(format!("{:.1}ms", d.await_read_ms.max(d.await_write_ms))),
            ])
        })
        .collect();

    let disk_table = Table::new(
        disk_rows,
        [
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(8),
        ],
    )
    .header(disk_header);
    frame.render_widget(disk_table, disk_inner);

    // Network table
    let net_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            " Network ",
            Style::default().fg(theme.primary),
        ));
    let net_inner = net_block.inner(net_area);
    frame.render_widget(net_block, net_area);

    let net_header = Row::new(vec!["Interface", "RX/s", "TX/s", "Pkts R/s", "Errors"])
        .style(Style::default().fg(theme.text_dim));
    let net_rows: Vec<Row> = state
        .networks
        .iter()
        .take(net_inner.height as usize)
        .map(|n| {
            let status_color = if n.is_up {
                theme.success
            } else {
                theme.text_muted
            };
            Row::new(vec![
                Cell::from(Span::styled(&n.name, Style::default().fg(status_color))),
                Cell::from(format_throughput(n.rx_bytes_per_sec)),
                Cell::from(format_throughput(n.tx_bytes_per_sec)),
                Cell::from(format!("{:.0}", n.rx_packets_per_sec)),
                Cell::from(format!("{}", n.rx_errors + n.tx_errors)),
            ])
        })
        .collect();

    let net_table = Table::new(
        net_rows,
        [
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(7),
        ],
    )
    .header(net_header);
    frame.render_widget(net_table, net_inner);
}

fn render_process_table(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let title = match state.input_mode {
        crate::ui::input::InputMode::ProcessSort => {
            format!(" GPU Processes [SORT: {}] ", state.process_sort.label())
        }
        crate::ui::input::InputMode::ProcessKill => " GPU Processes [KILL] ".to_owned(),
        crate::ui::input::InputMode::ProcessFilter => {
            format!(" GPU Processes [FILTER: {}] ", state.process_filter)
        }
        _ => " GPU Processes ".to_owned(),
    };

    let border_color = match state.input_mode {
        crate::ui::input::InputMode::ProcessSort => theme.warning,
        crate::ui::input::InputMode::ProcessKill => theme.danger,
        crate::ui::input::InputMode::ProcessFilter => theme.accent,
        _ => theme.border,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(title, Style::default().fg(theme.primary)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 2 {
        return;
    }

    let header = Row::new(vec![
        "PID", "USER", "GPU", "TYPE", "GPU%", "GPU MEM", "CPU%", "HOST MEM", "COMMAND",
    ])
    .style(
        Style::default()
            .fg(theme.text_dim)
            .add_modifier(Modifier::BOLD),
    );

    let filtered = state.filtered_processes();
    let rows: Vec<Row> = filtered
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let is_selected = i == state.process_selected_index;
            let style = if is_selected {
                Style::default()
                    .fg(theme.text)
                    .bg(theme.highlight_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };

            Row::new(vec![
                Cell::from(format!("{}", p.pid)),
                Cell::from(truncate_str(&p.user, 8)),
                Cell::from(format!("{}", p.gpu_index)),
                Cell::from(p.process_type.to_string()),
                Cell::from(format!("{:.0}%", p.gpu_utilization)),
                Cell::from(format_bytes(p.gpu_memory_bytes)),
                Cell::from(format!("{:.0}%", p.cpu_percent)),
                Cell::from(format_bytes(p.host_memory_bytes)),
                Cell::from(truncate_str(&p.command, 40)),
            ])
            .style(style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Length(4),
            Constraint::Length(8),
            Constraint::Length(5),
            Constraint::Length(9),
            Constraint::Length(5),
            Constraint::Length(9),
            Constraint::Fill(1),
        ],
    )
    .header(header);
    frame.render_widget(table, inner);
}

// --- Formatting helpers ---

fn bytes_to_gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
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

fn format_throughput(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} GB/s", bytes_per_sec / (1024.0 * 1024.0 * 1024.0))
    } else if bytes_per_sec >= 1024.0 * 1024.0 {
        format!("{:.1} MB/s", bytes_per_sec / (1024.0 * 1024.0))
    } else if bytes_per_sec >= 1024.0 {
        format!("{:.1} KB/s", bytes_per_sec / 1024.0)
    } else {
        format!("{:.0} B/s", bytes_per_sec)
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
