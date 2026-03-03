use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Sparkline, Table};

use crate::app::AppState;
use crate::ui::theme::Theme;
use crate::ui::widgets::gradient_gauge::GradientGauge;

/// Render the main overview dashboard.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let gpu_count = state.gpus.len();
    let gpu_panel_height = if gpu_count == 0 {
        0
    } else {
        (gpu_count as u16 * 3 + 2).min(14)
    };

    let [top_area, mid_area, bottom_area] = Layout::vertical([
        Constraint::Length(9),
        Constraint::Length(gpu_panel_height),
        Constraint::Fill(1),
    ])
    .areas(area);

    render_cpu_memory_row(frame, top_area, state, theme);

    if gpu_count > 0 {
        render_gpu_panel(frame, mid_area, state, theme);
    }

    render_bottom_section(frame, bottom_area, state, theme);
}

fn styled_block<'a>(title: &'a str, theme: &Theme) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ))
}

fn styled_block_active<'a>(
    title: &'a str,
    border_color: ratatui::style::Color,
    title_color: ratatui::style::Color,
) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        ))
}

// ── CPU + Memory ──────────────────────────────────────────────────────

fn render_cpu_memory_row(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let [cpu_area, mem_area] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(area);

    render_cpu_panel(frame, cpu_area, state, theme);
    render_memory_panel(frame, mem_area, state, theme);
}

fn render_cpu_panel(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let block = styled_block("CPU", theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let cpu = match &state.cpu {
        Some(c) => c,
        None => return,
    };

    let per_core_rows = if state.show_per_core {
        cpu.core_count.min(8) as u16
    } else {
        0
    };

    let [info_area, bar_area, core_area, sparkline_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(per_core_rows),
        Constraint::Fill(1),
    ])
    .areas(inner);

    // ── Info line ──
    let temp_str = cpu
        .temperature_celsius
        .map(|t| format!("{t:.0}°C"))
        .unwrap_or_else(|| "—".to_owned());
    let temp_color = cpu
        .temperature_celsius
        .map(|t| theme.temp_color(t))
        .unwrap_or(theme.text_muted);

    let freq_str = if cpu.frequency_mhz > 0.0 {
        format!("{:.0} MHz", cpu.frequency_mhz)
    } else {
        "— MHz".to_owned()
    };

    let info = Line::from(vec![
        Span::styled(
            format!(" {:.1}%", cpu.usage_percent),
            Style::default()
                .fg(theme.percent_color(cpu.usage_percent))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(format!(" {temp_str}"), Style::default().fg(temp_color)),
        Span::styled("  ", Style::default()),
        Span::styled(format!(" {freq_str}"), Style::default().fg(theme.text_dim)),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!(" {} cores", cpu.core_count),
            Style::default().fg(theme.text_muted),
        ),
    ]);
    frame.render_widget(Paragraph::new(info), info_area);

    // ── Main gauge ──
    let gauge = GradientGauge::new(cpu.usage_percent / 100.0)
        .colors(theme.gauge_low, theme.gauge_mid, theme.gauge_high)
        .bg_color(theme.gauge_bg)
        .show_percentage();
    frame.render_widget(
        gauge,
        Rect::new(
            bar_area.x + 1,
            bar_area.y,
            bar_area.width.saturating_sub(2),
            1,
        ),
    );

    // ── Breakdown ──
    if bar_area.height > 1 {
        let breakdown = Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled("▪", Style::default().fg(theme.success)),
            Span::styled(
                format!(" usr {:.0}%", cpu.user_percent),
                Style::default().fg(theme.text_dim),
            ),
            Span::styled("  ", Style::default()),
            Span::styled("▪", Style::default().fg(theme.danger)),
            Span::styled(
                format!(" sys {:.0}%", cpu.system_percent),
                Style::default().fg(theme.text_dim),
            ),
            Span::styled("  ", Style::default()),
            Span::styled("▪", Style::default().fg(theme.warning)),
            Span::styled(
                format!(" iow {:.0}%", cpu.iowait_percent),
                Style::default().fg(theme.text_dim),
            ),
            Span::styled("  ", Style::default()),
            Span::styled("▪", Style::default().fg(theme.text_muted)),
            Span::styled(
                format!(" idle {:.0}%", cpu.idle_percent),
                Style::default().fg(theme.text_muted),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(breakdown),
            Rect::new(bar_area.x, bar_area.y + 1, bar_area.width, 1),
        );
    }

    // ── Per-core mini bars ──
    if state.show_per_core && core_area.height > 0 {
        let half_width = core_area.width / 2;
        for (i, core) in cpu
            .cores
            .iter()
            .take(core_area.height as usize * 2)
            .enumerate()
        {
            let col = i % 2;
            let row = i / 2;
            if row as u16 >= core_area.height {
                break;
            }
            let x = core_area.x + col as u16 * half_width;
            let w = half_width.saturating_sub(1);

            // Core label
            let label = format!("{:>2}", i);
            frame.buffer_mut().set_string(
                x,
                core_area.y + row as u16,
                &label,
                Style::default().fg(theme.text_muted),
            );

            // Mini gauge
            let gauge_w = w.saturating_sub(8);
            if gauge_w > 0 {
                let pct_label = format!("{:>3.0}%", core.usage_percent);
                let gauge = GradientGauge::new(core.usage_percent / 100.0)
                    .label(&pct_label)
                    .colors(theme.gauge_low, theme.gauge_mid, theme.gauge_high)
                    .bg_color(theme.gauge_bg);
                frame.render_widget(
                    gauge,
                    Rect::new(x + 3, core_area.y + row as u16, gauge_w, 1),
                );
            }
        }
    }

    // ── Sparkline ──
    if sparkline_area.height > 0 {
        let data = state.cpu_history.usage.to_sparkline_data();
        let sparkline = Sparkline::default()
            .data(&data)
            .max(100)
            .style(Style::default().fg(theme.sparkline_color));
        frame.render_widget(sparkline, sparkline_area);
    }
}

fn render_memory_panel(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let block = styled_block("Memory", theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let mem = match &state.memory {
        Some(m) => m,
        None => return,
    };

    let [
        ram_label_area,
        ram_bar_area,
        swap_label_area,
        swap_bar_area,
        detail_area,
        sparkline_area,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(inner);

    let usage_pct = mem.usage_percent();
    let swap_pct = mem.swap_usage_percent();

    // ── RAM label ──
    let ram_info = Line::from(vec![
        Span::styled(" RAM ", Style::default().fg(theme.text_dim)),
        Span::styled(
            format!(
                "{:.1} / {:.1} GB",
                bytes_to_gib(mem.used_bytes),
                bytes_to_gib(mem.total_bytes),
            ),
            Style::default().fg(theme.text),
        ),
        Span::styled(
            format!("  {:.1}%", usage_pct),
            Style::default()
                .fg(theme.percent_color(usage_pct))
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(ram_info), ram_label_area);

    // ── RAM gauge ──
    let ram_gauge = GradientGauge::new(usage_pct / 100.0)
        .colors(theme.gauge_low, theme.gauge_mid, theme.gauge_high)
        .bg_color(theme.gauge_bg);
    frame.render_widget(
        ram_gauge,
        Rect::new(
            ram_bar_area.x + 1,
            ram_bar_area.y,
            ram_bar_area.width.saturating_sub(2),
            1,
        ),
    );

    // ── Swap label ──
    let swap_info = Line::from(vec![
        Span::styled(" Swap", Style::default().fg(theme.text_muted)),
        Span::styled(
            format!(
                " {:.1} / {:.1} GB",
                bytes_to_gib(mem.swap_used_bytes),
                bytes_to_gib(mem.swap_total_bytes),
            ),
            Style::default().fg(if swap_pct > 50.0 {
                theme.warning
            } else {
                theme.text_dim
            }),
        ),
    ]);
    frame.render_widget(Paragraph::new(swap_info), swap_label_area);

    // ── Swap gauge ──
    let swap_gauge = GradientGauge::new(swap_pct / 100.0)
        .colors(theme.gauge_low, theme.gauge_mid, theme.gauge_high)
        .bg_color(theme.gauge_bg);
    frame.render_widget(
        swap_gauge,
        Rect::new(
            swap_bar_area.x + 1,
            swap_bar_area.y,
            swap_bar_area.width.saturating_sub(2),
            1,
        ),
    );

    // ── Detail ──
    let detail = Line::from(vec![
        Span::styled(" buf ", Style::default().fg(theme.text_muted)),
        Span::styled(
            format_bytes_short(mem.buffers_bytes),
            Style::default().fg(theme.text_dim),
        ),
        Span::styled("  cache ", Style::default().fg(theme.text_muted)),
        Span::styled(
            format_bytes_short(mem.cached_bytes),
            Style::default().fg(theme.text_dim),
        ),
        Span::styled("  avail ", Style::default().fg(theme.text_muted)),
        Span::styled(
            format_bytes_short(mem.available_bytes),
            Style::default().fg(theme.success),
        ),
    ]);
    frame.render_widget(Paragraph::new(detail), detail_area);

    // ── Sparkline ──
    if sparkline_area.height > 0 {
        let data = state.memory_history.usage.to_sparkline_data();
        let sparkline = Sparkline::default()
            .data(&data)
            .max(100)
            .style(Style::default().fg(theme.accent));
        frame.render_widget(sparkline, sparkline_area);
    }
}

// ── GPU Panel ─────────────────────────────────────────────────────────

fn render_gpu_panel(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let title = format!("GPUs  {}x", state.gpus.len());
    let block = styled_block(&title, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let constraints: Vec<Constraint> = state.gpus.iter().map(|_| Constraint::Length(3)).collect();
    let gpu_areas = Layout::vertical(constraints).split(inner);

    for (i, gpu) in state.gpus.iter().enumerate() {
        if i >= gpu_areas.len() {
            break;
        }
        render_gpu_card(frame, gpu_areas[i], gpu, theme);
    }
}

fn render_gpu_card(
    frame: &mut Frame,
    area: Rect,
    gpu: &crate::domain::gpu::GpuStats,
    theme: &Theme,
) {
    if area.height == 0 || area.width < 30 {
        return;
    }

    let [header_line, util_line, mem_line] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    let temp_color = theme.temp_color(gpu.temperature);
    let power_pct = gpu.power_usage_percent();

    // ── GPU info line ──
    let header = Line::from(vec![
        Span::styled(
            format!("  GPU {}  ", gpu.index),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(truncate_str(&gpu.name, 22), Style::default().fg(theme.text)),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("{:.0}°C", gpu.temperature),
            Style::default().fg(temp_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!(
                "⚡{:.0}/{:.0}W",
                gpu.power_draw_watts, gpu.power_limit_watts
            ),
            Style::default().fg(theme.percent_color(power_pct)),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("{:.0} MHz", gpu.clock_graphics_mhz),
            Style::default().fg(theme.text_dim),
        ),
        gpu.fan_speed
            .map(|f| {
                Span::styled(
                    format!("  Fan {f:.0}%"),
                    Style::default().fg(theme.text_muted),
                )
            })
            .unwrap_or_default(),
    ]);
    frame.render_widget(Paragraph::new(header), header_line);

    // ── Utilization bar ──
    let bar_start = area.x + 2;
    let bar_width = area.width.saturating_sub(18);

    frame.buffer_mut().set_string(
        bar_start,
        util_line.y,
        "GPU",
        Style::default().fg(theme.text_muted),
    );

    let gauge = GradientGauge::new(gpu.utilization_gpu / 100.0)
        .colors(theme.gauge_low, theme.gauge_mid, theme.gauge_high)
        .bg_color(theme.gauge_bg)
        .show_percentage();
    frame.render_widget(gauge, Rect::new(bar_start + 4, util_line.y, bar_width, 1));

    // ── VRAM bar ──
    let mem_pct = gpu.memory_usage_percent();
    let mem_gib_used = gpu.memory_used_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let mem_gib_total = gpu.memory_total_bytes as f64 / (1024.0 * 1024.0 * 1024.0);

    frame.buffer_mut().set_string(
        bar_start,
        mem_line.y,
        "MEM",
        Style::default().fg(theme.text_muted),
    );

    let mem_label = format!("{:.1}/{:.0}G", mem_gib_used, mem_gib_total);
    let mem_gauge = GradientGauge::new(mem_pct / 100.0)
        .label(&mem_label)
        .colors(theme.gauge_low, theme.gauge_mid, theme.gauge_high)
        .bg_color(theme.gauge_bg);
    frame.render_widget(
        mem_gauge,
        Rect::new(bar_start + 4, mem_line.y, bar_width, 1),
    );
}

// ── Bottom Section (I/O + Processes) ──────────────────────────────────

fn render_bottom_section(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let [io_area, process_area] =
        Layout::vertical([Constraint::Length(7), Constraint::Fill(1)]).areas(area);

    render_io_row(frame, io_area, state, theme);
    render_process_table(frame, process_area, state, theme);
}

fn render_io_row(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let [disk_area, net_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);

    // ── Disk I/O ──
    let disk_block = styled_block("Disk I/O", theme);
    let disk_inner = disk_block.inner(disk_area);
    frame.render_widget(disk_block, disk_area);

    let disk_header = Row::new(vec![
        Cell::from("Device").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("Read").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("Write").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("IOPS").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("Await").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    let disk_rows: Vec<Row> = state
        .disks
        .iter()
        .take(disk_inner.height.saturating_sub(1) as usize)
        .enumerate()
        .map(|(i, d)| {
            let bg = if i % 2 == 1 {
                Style::default().bg(theme.row_alt_bg)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(Span::styled(
                    &d.device_name,
                    Style::default().fg(theme.secondary),
                )),
                Cell::from(format_throughput(d.read_bytes_per_sec)),
                Cell::from(format_throughput(d.write_bytes_per_sec)),
                Cell::from(format!("{:.0}/{:.0}", d.read_iops, d.write_iops)),
                Cell::from(format!("{:.1}ms", d.await_read_ms.max(d.await_write_ms))),
            ])
            .style(bg)
        })
        .collect();

    let disk_table = Table::new(
        disk_rows,
        [
            Constraint::Length(10),
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Length(9),
            Constraint::Length(8),
        ],
    )
    .header(disk_header);
    frame.render_widget(disk_table, disk_inner);

    // ── Network ──
    let net_block = styled_block("Network", theme);
    let net_inner = net_block.inner(net_area);
    frame.render_widget(net_block, net_area);

    let net_header = Row::new(vec![
        Cell::from("Interface").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("RX/s").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("TX/s").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("Pkts").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("Errors").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    let net_rows: Vec<Row> = state
        .networks
        .iter()
        .take(net_inner.height.saturating_sub(1) as usize)
        .enumerate()
        .map(|(i, n)| {
            let status = if n.is_up { "●" } else { "○" };
            let status_color = if n.is_up {
                theme.success
            } else {
                theme.text_muted
            };
            let bg = if i % 2 == 1 {
                Style::default().bg(theme.row_alt_bg)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(Line::from(vec![
                    Span::styled(format!("{status} "), Style::default().fg(status_color)),
                    Span::styled(&n.name, Style::default().fg(theme.text)),
                ])),
                Cell::from(format_throughput(n.rx_bytes_per_sec)),
                Cell::from(format_throughput(n.tx_bytes_per_sec)),
                Cell::from(format!("{:.0}", n.rx_packets_per_sec)),
                Cell::from(Span::styled(
                    format!("{}", n.rx_errors + n.tx_errors),
                    Style::default().fg(if n.rx_errors + n.tx_errors > 0 {
                        theme.danger
                    } else {
                        theme.text_dim
                    }),
                )),
            ])
            .style(bg)
        })
        .collect();

    let net_table = Table::new(
        net_rows,
        [
            Constraint::Length(14),
            Constraint::Fill(1),
            Constraint::Fill(1),
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
            format!("GPU Processes  SORT: {}", state.process_sort.label())
        }
        crate::ui::input::InputMode::ProcessKill => "GPU Processes  KILL".to_owned(),
        crate::ui::input::InputMode::ProcessFilter => {
            format!("GPU Processes  /{}", state.process_filter)
        }
        _ => "GPU Processes".to_owned(),
    };

    let border_color = match state.input_mode {
        crate::ui::input::InputMode::ProcessSort => theme.warning,
        crate::ui::input::InputMode::ProcessKill => theme.danger,
        crate::ui::input::InputMode::ProcessFilter => theme.accent,
        _ => theme.border,
    };

    let title_color = match state.input_mode {
        crate::ui::input::InputMode::ProcessSort => theme.warning,
        crate::ui::input::InputMode::ProcessKill => theme.danger,
        crate::ui::input::InputMode::ProcessFilter => theme.accent,
        _ => theme.primary,
    };

    let block = styled_block_active(&title, border_color, title_color);
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
    )
    .height(1);

    let filtered = state.filtered_processes();
    let rows: Vec<Row> = filtered
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let is_selected = i == state.process_selected_index;
            let alt_bg = if i % 2 == 1 {
                theme.row_alt_bg
            } else {
                theme.background
            };

            let style = if is_selected {
                Style::default()
                    .fg(theme.text)
                    .bg(theme.highlight_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text).bg(alt_bg)
            };

            let gpu_color = theme.percent_color(p.gpu_utilization);

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
                    Style::default().fg(gpu_color),
                )),
                Cell::from(format_bytes(p.gpu_memory_bytes)),
                Cell::from(format!("{:.0}%", p.cpu_percent)),
                Cell::from(format_bytes(p.host_memory_bytes)),
                Cell::from(truncate_str(&p.command, 50)),
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

// ── Formatting helpers ────────────────────────────────────────────────

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

fn format_bytes_short(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.0}M", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.0}K", bytes as f64 / 1024.0)
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
