use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, BorderType, Borders, Cell, Chart, Dataset, GraphType, Paragraph, Row, Sparkline,
    Table,
};

use crate::app::AppState;
use crate::domain::history::TimeWindowAggregator;
use crate::ui::theme::Theme;
use crate::ui::widgets::gradient_gauge::GradientGauge;

/// Render the main overview dashboard.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let h = area.height;
    let gpu_count = state.gpus.len();

    // GPU panel: fixed based on GPU count
    let gpu_want = if gpu_count == 0 {
        0u16
    } else {
        (gpu_count as u16 * 3 + 2).min(14)
    };

    // CPU+Mem panel: desired height
    let cpu_mem_want = compute_cpu_mem_want(state);

    // Minimum heights before we start cutting
    let cpu_mem_min: u16 = 7; // border(2) + info(1) + gauge(2) + stats(2)
    let io_min: u16 = 6; // border(2) + header+1row(2) + chart(2)
    let process_min: u16 = 4; // border(2) + header(1) + 1 row(1)

    // Distribute available height
    let fixed = gpu_want + process_min;
    let flexible_budget = h.saturating_sub(fixed);

    // Give CPU+Mem its share first, then IO gets the rest (both capped at want)
    let io_want: u16 = 12;
    let cpu_mem_h = if flexible_budget >= cpu_mem_want + io_want {
        cpu_mem_want
    } else if flexible_budget >= cpu_mem_min + io_min {
        // Distribute proportionally between cpu_mem and io
        let extra = flexible_budget.saturating_sub(cpu_mem_min + io_min);
        let cpu_extra_max = cpu_mem_want.saturating_sub(cpu_mem_min);
        let io_extra_max = io_want.saturating_sub(io_min);
        let total_extra_want = cpu_extra_max + io_extra_max;
        let cpu_extra = if total_extra_want > 0 {
            (extra as u32 * cpu_extra_max as u32 / total_extra_want as u32) as u16
        } else {
            0
        };
        cpu_mem_min + cpu_extra.min(cpu_extra_max)
    } else {
        // Very tight — give cpu_mem at least its min
        flexible_budget.saturating_sub(io_min).max(cpu_mem_min)
    };

    let io_h = flexible_budget
        .saturating_sub(cpu_mem_h)
        .min(io_want)
        .max(io_min.min(flexible_budget.saturating_sub(cpu_mem_h)));

    let [top_area, mid_area, bottom_area] = Layout::vertical([
        Constraint::Length(cpu_mem_h),
        Constraint::Length(gpu_want),
        Constraint::Fill(1),
    ])
    .areas(area);

    render_cpu_memory_row(frame, top_area, state, theme);

    if gpu_count > 0 {
        render_gpu_panel(frame, mid_area, state, theme);
    }

    render_bottom_section(frame, bottom_area, state, theme, io_h);
}

fn compute_cpu_mem_want(state: &AppState) -> u16 {
    if !state.show_per_core {
        // border(2) + info(1) + gauge(2) + stats(1) + sparkline(2) = 10
        // Memory: border(2) + ram(2) + swap(2) + detail(1) + stats(1) + sparkline(2) = 10
        return 10;
    }
    let core_count = state.cpu.as_ref().map(|c| c.core_count).unwrap_or(0);
    let cols = per_core_columns(core_count);
    let core_rows = if cols == 0 {
        0
    } else {
        (core_count as u16).div_ceil(cols)
    };
    // border(2) + info(1) + core_rows + stats(1)
    (2 + 1 + core_rows + 1).max(10)
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

/// Determine how many columns to use for per-core display.
fn per_core_columns(core_count: usize) -> u16 {
    if core_count <= 16 {
        2
    } else if core_count <= 64 {
        4
    } else {
        8
    }
}

fn render_cpu_panel(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let cpu = match &state.cpu {
        Some(c) => c,
        None => {
            let block = styled_block("CPU", theme);
            frame.render_widget(block, area);
            return;
        }
    };

    let title = if state.show_per_core {
        format!("CPU ({} cores)", cpu.core_count)
    } else {
        "CPU".to_owned()
    };
    let block = styled_block(&title, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if state.show_per_core {
        render_cpu_per_core(frame, inner, cpu, state, theme);
    } else {
        render_cpu_aggregate(frame, inner, cpu, state, theme);
    }
}

fn render_cpu_aggregate(
    frame: &mut Frame,
    inner: Rect,
    cpu: &crate::domain::cpu::CpuStats,
    state: &AppState,
    theme: &Theme,
) {
    let [info_area, bar_area, stats_area, sparkline_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(inner);

    render_cpu_info_line(frame, info_area, cpu, theme);
    render_cpu_main_gauge(frame, bar_area, cpu, theme);

    // ── Stats (1/6/12/24h on 1 line) ──
    if stats_area.height > 0 {
        let line = build_pct_stats_line(&state.cpu_history.usage_agg, theme);
        frame.render_widget(Paragraph::new(line), stats_area);
    }

    // ── Sparkline ──
    if sparkline_area.height > 0 {
        let data = state
            .cpu_history
            .usage
            .to_sparkline_data(sparkline_area.width as usize);
        let sparkline = Sparkline::default()
            .data(&data)
            .max(100)
            .style(Style::default().fg(theme.sparkline_color));
        frame.render_widget(sparkline, sparkline_area);
    }
}

fn render_cpu_per_core(
    frame: &mut Frame,
    inner: Rect,
    cpu: &crate::domain::cpu::CpuStats,
    state: &AppState,
    theme: &Theme,
) {
    let cols = per_core_columns(cpu.core_count);
    let core_rows = if cols == 0 {
        0
    } else {
        (cpu.core_count as u16).div_ceil(cols)
    };

    let [info_area, core_area, stats_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(core_rows),
        Constraint::Fill(1),
    ])
    .areas(inner);

    render_cpu_info_line(frame, info_area, cpu, theme);

    // ── Per-core bars (htop-style) ──
    if core_area.height > 0 && core_area.width > 0 {
        let col_width = core_area.width / cols;
        for (i, core) in cpu.cores.iter().enumerate() {
            let col = i as u16 % cols;
            let row = i as u16 / cols;
            if row >= core_area.height {
                break;
            }
            let x = core_area.x + col * col_width;
            let w = col_width.saturating_sub(1);

            // Core label (right-aligned index)
            let label_width: u16 = if cpu.core_count >= 100 { 4 } else { 3 };
            let label = format!("{:>width$}", i, width = label_width as usize);
            frame.buffer_mut().set_string(
                x,
                core_area.y + row,
                &label,
                Style::default().fg(theme.text_muted),
            );

            // Gauge
            let gauge_w = w.saturating_sub(label_width + 5);
            if gauge_w > 0 {
                let pct_label = format!("{:>3.0}%", core.usage_percent);
                let gauge = GradientGauge::new(core.usage_percent / 100.0)
                    .label(&pct_label)
                    .colors(theme.gauge_low, theme.gauge_mid, theme.gauge_high)
                    .bg_color(theme.gauge_bg);
                frame.render_widget(
                    gauge,
                    Rect::new(x + label_width, core_area.y + row, gauge_w, 1),
                );
            }
        }
    }

    // ── Stats ──
    if stats_area.height > 0 {
        let line = build_pct_stats_line(&state.cpu_history.usage_agg, theme);
        frame.render_widget(Paragraph::new(line), stats_area);
    }
}

fn render_cpu_info_line(
    frame: &mut Frame,
    area: Rect,
    cpu: &crate::domain::cpu::CpuStats,
    theme: &Theme,
) {
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

    let load_color = |val: f64| -> ratatui::style::Color {
        let ratio = val / cpu.core_count.max(1) as f64;
        if ratio >= 1.0 {
            theme.danger
        } else if ratio >= 0.7 {
            theme.warning
        } else {
            theme.success
        }
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
        Span::styled("  load ", Style::default().fg(theme.text_muted)),
        Span::styled(
            format!("{:.2}", cpu.load_avg_1m),
            Style::default().fg(load_color(cpu.load_avg_1m)),
        ),
        Span::styled(" ", Style::default()),
        Span::styled(
            format!("{:.2}", cpu.load_avg_5m),
            Style::default().fg(load_color(cpu.load_avg_5m)),
        ),
        Span::styled(" ", Style::default()),
        Span::styled(
            format!("{:.2}", cpu.load_avg_15m),
            Style::default().fg(load_color(cpu.load_avg_15m)),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("tasks {}/{}", cpu.tasks_running, cpu.tasks_total),
            Style::default().fg(theme.text_muted),
        ),
    ]);
    frame.render_widget(Paragraph::new(info), area);
}

fn render_cpu_main_gauge(
    frame: &mut Frame,
    bar_area: Rect,
    cpu: &crate::domain::cpu::CpuStats,
    theme: &Theme,
) {
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
}

/// Return only the time windows for which enough data has been collected.
fn active_windows(agg: &TimeWindowAggregator) -> Vec<usize> {
    let elapsed = agg.elapsed_hours();
    [1, 6, 12, 24]
        .iter()
        .copied()
        .filter(|&h| elapsed >= h as f64)
        .collect()
}

/// Build the `Xh` label like `1h` or `1/6/12/24h`.
fn windows_label(windows: &[usize]) -> String {
    let parts: Vec<String> = windows.iter().map(|h| h.to_string()).collect();
    format!("{}h", parts.join("/"))
}

/// Format percentage stats as `avg X/X/X/X% max X/X/X/X%` on 1 line.
/// Only shows windows with enough elapsed time.
fn build_pct_stats_line<'a>(agg: &TimeWindowAggregator, theme: &Theme) -> Line<'a> {
    let windows = active_windows(agg);
    if windows.is_empty() {
        return Line::default();
    }
    let avgs: Vec<String> = windows
        .iter()
        .map(|&h| format!("{:.0}", agg.average_over_hours(h)))
        .collect();
    let maxs: Vec<String> = windows
        .iter()
        .map(|&h| format!("{:.0}", agg.max_over_hours(h)))
        .collect();
    Line::from(vec![
        Span::styled(
            format!(" {} ", windows_label(&windows)),
            Style::default().fg(theme.text_muted),
        ),
        Span::styled("avg:", Style::default().fg(theme.text_dim)),
        Span::styled(
            format!("{}%", avgs.join("/")),
            Style::default().fg(theme.text),
        ),
        Span::styled("  max:", Style::default().fg(theme.text_dim)),
        Span::styled(
            format!("{}%", maxs.join("/")),
            Style::default().fg(theme.warning),
        ),
    ])
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
        stats_area,
        sparkline_area,
    ] = Layout::vertical([
        Constraint::Length(1),
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

    // ── Memory stats (1/6/12/24h) ──
    if stats_area.height > 0 {
        let line = build_pct_stats_line(&state.memory_history.usage_agg, theme);
        frame.render_widget(Paragraph::new(line), stats_area);
    }

    // ── Sparkline ──
    if sparkline_area.height > 0 {
        let data = state
            .memory_history
            .usage
            .to_sparkline_data(sparkline_area.width as usize);
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

    let mem_metric_label = if gpu.memory_is_shared { "MGP" } else { "MEM" };
    frame.buffer_mut().set_string(
        bar_start,
        mem_line.y,
        mem_metric_label,
        Style::default().fg(theme.text_muted),
    );

    let mem_label = if gpu.memory_total_bytes > 0 {
        format!("{mem_gib_used:.1}/{mem_gib_total:.0}G")
    } else if gpu.memory_is_shared {
        format!("{mem_gib_used:.1}G used")
    } else {
        "N/A".to_owned()
    };
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

fn render_bottom_section(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    io_h: u16,
) {
    let [io_area, process_area] =
        Layout::vertical([Constraint::Length(io_h), Constraint::Fill(1)]).areas(area);

    render_io_row(frame, io_area, state, theme);
    render_process_table(frame, process_area, state, theme);
}

fn render_io_row(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let [disk_area, net_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);

    render_disk_panel(frame, disk_area, state, theme);
    render_network_panel(frame, net_area, state, theme);
}

fn render_disk_panel(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let selected_name = state
        .disks
        .get(state.selected_disk_index)
        .map(|d| d.device_name.as_str())
        .unwrap_or("—");
    let title = format!("Disk I/O [{selected_name}] (d/D)");
    let disk_block = styled_block(&title, theme);
    let disk_inner = disk_block.inner(area);
    frame.render_widget(disk_block, area);

    if disk_inner.height == 0 || disk_inner.width == 0 {
        return;
    }

    let max_visible = state.config.network_max_visible;
    let table_rows = state.disks.len().min(max_visible) as u16;
    let inner_h = disk_inner.height;
    let show_chart = inner_h > (1 + table_rows + 1 + 2);
    let show_stats = inner_h > (1 + table_rows + 1);

    let mut constraints = vec![Constraint::Length(1 + table_rows)];
    if show_chart {
        constraints.push(Constraint::Fill(1)); // chart gets remaining
    }
    if show_stats {
        constraints.push(Constraint::Length(1)); // stats 1 line (x/x/x/x format)
    }
    let areas = Layout::vertical(constraints).split(disk_inner);

    let table_area = areas[0];
    let (chart_area, stats_area) = if show_chart && show_stats {
        (Some(areas[1]), Some(areas[2]))
    } else if show_chart {
        (Some(areas[1]), None)
    } else if show_stats {
        (None, Some(areas[1]))
    } else {
        (None, None)
    };

    // ── Disk table ──
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
    ]);

    let disk_rows: Vec<Row> = state
        .disks
        .iter()
        .take(max_visible)
        .enumerate()
        .map(|(i, d)| {
            let is_selected = i == state.selected_disk_index;
            let bg = if is_selected {
                Style::default().bg(theme.highlight_bg)
            } else if i % 2 == 1 {
                Style::default().bg(theme.row_alt_bg)
            } else {
                Style::default()
            };
            let name_color = if is_selected {
                theme.primary
            } else {
                theme.secondary
            };
            Row::new(vec![
                Cell::from(Span::styled(
                    &d.device_name,
                    Style::default().fg(name_color),
                )),
                Cell::from(format_throughput(d.read_bytes_per_sec)),
                Cell::from(format_throughput(d.write_bytes_per_sec)),
                Cell::from(format!("{:.0}/{:.0}", d.read_iops, d.write_iops)),
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
        ],
    )
    .header(disk_header);
    frame.render_widget(disk_table, table_area);

    // ── Disk chart + stats ──
    if let Some(history) = selected_disk_history(state) {
        if let Some(ca) = chart_area
            && ca.height > 0
            && ca.width > 4
        {
            let read_data = history.read_throughput.to_chart_data();
            let write_data = history.write_throughput.to_chart_data();
            render_dual_line_chart(
                frame,
                ca,
                &DualChartConfig {
                    data_a: &read_data,
                    data_b: &write_data,
                    max_y: history
                        .read_throughput
                        .max_value()
                        .max(history.write_throughput.max_value())
                        .max(1.0),
                    color_a: theme.success,
                    color_b: theme.warning,
                    name_a: "R",
                    name_b: "W",
                },
                theme,
            );
        }

        if let Some(sa) = stats_area
            && sa.height > 0
        {
            let line = build_disk_stats_line(
                &history.read_agg,
                &history.write_agg,
                state.config.update_interval_secs,
                theme,
            );
            frame.render_widget(Paragraph::new(line), sa);
        }
    }
}

fn selected_disk_history(state: &AppState) -> Option<&crate::domain::disk::DiskHistory> {
    let selected = state.disks.get(state.selected_disk_index)?;
    state.disk_histories.get(&selected.device_name)
}

fn selected_network_history(state: &AppState) -> Option<&crate::domain::network::NetworkHistory> {
    let selected = state.networks.get(state.selected_network_index)?;
    state.network_histories.get(&selected.name)
}

struct DualChartConfig<'a> {
    data_a: &'a [(f64, f64)],
    data_b: &'a [(f64, f64)],
    max_y: f64,
    color_a: ratatui::style::Color,
    color_b: ratatui::style::Color,
    name_a: &'a str,
    name_b: &'a str,
}

fn render_dual_line_chart(frame: &mut Frame, area: Rect, cfg: &DualChartConfig, theme: &Theme) {
    let x_bound = cfg.data_a.len().max(cfg.data_b.len()).max(1) as f64;

    let dataset_a = Dataset::default()
        .name(cfg.name_a)
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(cfg.color_a))
        .data(cfg.data_a);
    let dataset_b = Dataset::default()
        .name(cfg.name_b)
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(cfg.color_b))
        .data(cfg.data_b);

    let chart = Chart::new(vec![dataset_a, dataset_b])
        .x_axis(
            Axis::default()
                .style(Style::default().fg(theme.text_muted))
                .bounds([0.0, x_bound]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(theme.text_muted))
                .labels(vec![
                    Span::raw("0"),
                    Span::raw(format_throughput_short(cfg.max_y)),
                ])
                .bounds([0.0, cfg.max_y]),
        );
    frame.render_widget(chart, area);
}

/// Format disk stats as 1 line. Only shows windows with enough elapsed time.
fn build_disk_stats_line<'a>(
    read_agg: &TimeWindowAggregator,
    write_agg: &TimeWindowAggregator,
    _interval: f64,
    theme: &Theme,
) -> Line<'a> {
    let windows = active_windows(read_agg);
    if windows.is_empty() {
        return Line::default();
    }
    let r_avgs: Vec<String> = windows
        .iter()
        .map(|&h| format_throughput_short(read_agg.average_over_hours(h)))
        .collect();
    let w_avgs: Vec<String> = windows
        .iter()
        .map(|&h| format_throughput_short(write_agg.average_over_hours(h)))
        .collect();
    Line::from(vec![
        Span::styled(
            format!(" {} ", windows_label(&windows)),
            Style::default().fg(theme.text_muted),
        ),
        Span::styled("R:", Style::default().fg(theme.text_dim)),
        Span::styled(r_avgs.join("/"), Style::default().fg(theme.success)),
        Span::styled(" W:", Style::default().fg(theme.text_dim)),
        Span::styled(w_avgs.join("/"), Style::default().fg(theme.warning)),
    ])
}

fn render_network_panel(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let selected_name = state
        .networks
        .get(state.selected_network_index)
        .map(|n| n.name.as_str())
        .unwrap_or("—");
    let title = format!("Network [{selected_name}] (n/N)");
    let net_block = styled_block(&title, theme);
    let net_inner = net_block.inner(area);
    frame.render_widget(net_block, area);

    if net_inner.height == 0 || net_inner.width == 0 {
        return;
    }

    let max_visible = state.config.network_max_visible;
    let table_rows = state.networks.len().min(max_visible) as u16;
    let inner_h = net_inner.height;
    let show_chart = inner_h > (1 + table_rows + 1 + 2);
    let show_stats = inner_h > (1 + table_rows + 1);

    let mut constraints = vec![Constraint::Length(1 + table_rows)];
    if show_chart {
        constraints.push(Constraint::Fill(1));
    }
    if show_stats {
        constraints.push(Constraint::Length(1)); // stats 1 line (x/x/x/x format)
    }
    let areas = Layout::vertical(constraints).split(net_inner);

    let table_area = areas[0];
    let (chart_area, stats_area) = if show_chart && show_stats {
        (Some(areas[1]), Some(areas[2]))
    } else if show_chart {
        (Some(areas[1]), None)
    } else if show_stats {
        (None, Some(areas[1]))
    } else {
        (None, None)
    };

    // ── Network table ──
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
        Cell::from("Errors").style(
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    let net_rows: Vec<Row> = state
        .networks
        .iter()
        .take(max_visible)
        .enumerate()
        .map(|(i, n)| {
            let is_selected = i == state.selected_network_index;
            let status = if n.is_up { "●" } else { "○" };
            let status_color = if n.is_up {
                theme.success
            } else {
                theme.text_muted
            };
            let bg = if is_selected {
                Style::default().bg(theme.highlight_bg)
            } else if i % 2 == 1 {
                Style::default().bg(theme.row_alt_bg)
            } else {
                Style::default()
            };
            let name_color = if is_selected {
                theme.primary
            } else {
                theme.text
            };
            Row::new(vec![
                Cell::from(Line::from(vec![
                    Span::styled(format!("{status} "), Style::default().fg(status_color)),
                    Span::styled(&n.name, Style::default().fg(name_color)),
                ])),
                Cell::from(format_throughput(n.rx_bytes_per_sec)),
                Cell::from(format_throughput(n.tx_bytes_per_sec)),
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
            Constraint::Length(7),
        ],
    )
    .header(net_header);
    frame.render_widget(net_table, table_area);

    // ── Network chart + stats ──
    if let Some(history) = selected_network_history(state) {
        if let Some(ca) = chart_area
            && ca.height > 0
            && ca.width > 4
        {
            let rx_data = history.rx_throughput.to_chart_data();
            let tx_data = history.tx_throughput.to_chart_data();
            render_dual_line_chart(
                frame,
                ca,
                &DualChartConfig {
                    data_a: &rx_data,
                    data_b: &tx_data,
                    max_y: history
                        .rx_throughput
                        .max_value()
                        .max(history.tx_throughput.max_value())
                        .max(1.0),
                    color_a: theme.success,
                    color_b: theme.accent,
                    name_a: "RX",
                    name_b: "TX",
                },
                theme,
            );
        }

        if let Some(sa) = stats_area
            && sa.height > 0
        {
            let line = build_network_stats_line(
                &history.rx_agg,
                &history.tx_agg,
                state.config.update_interval_secs,
                theme,
            );
            frame.render_widget(Paragraph::new(line), sa);
        }
    }
}

/// Format network stats as 1 line. Only shows windows with enough elapsed time.
fn build_network_stats_line<'a>(
    rx_agg: &TimeWindowAggregator,
    tx_agg: &TimeWindowAggregator,
    interval: f64,
    theme: &Theme,
) -> Line<'a> {
    let windows = active_windows(rx_agg);
    if windows.is_empty() {
        return Line::default();
    }
    let rx_tots: Vec<String> = windows
        .iter()
        .map(|&h| format_bytes_compact(rx_agg.sum_over_hours(h) * interval))
        .collect();
    let tx_tots: Vec<String> = windows
        .iter()
        .map(|&h| format_bytes_compact(tx_agg.sum_over_hours(h) * interval))
        .collect();
    Line::from(vec![
        Span::styled(
            format!(" {} ", windows_label(&windows)),
            Style::default().fg(theme.text_muted),
        ),
        Span::styled("↓", Style::default().fg(theme.success)),
        Span::styled(rx_tots.join("/"), Style::default().fg(theme.success)),
        Span::styled(" ↑", Style::default().fg(theme.accent)),
        Span::styled(tx_tots.join("/"), Style::default().fg(theme.accent)),
    ])
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

fn format_throughput_short(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1}G/s", bytes_per_sec / (1024.0 * 1024.0 * 1024.0))
    } else if bytes_per_sec >= 1024.0 * 1024.0 {
        format!("{:.0}M/s", bytes_per_sec / (1024.0 * 1024.0))
    } else if bytes_per_sec >= 1024.0 {
        format!("{:.0}K/s", bytes_per_sec / 1024.0)
    } else {
        format!("{:.0}B/s", bytes_per_sec)
    }
}

fn format_bytes_compact(bytes: f64) -> String {
    let abs = bytes.abs();
    if abs >= 1024.0 * 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1}TB", bytes / (1024.0 * 1024.0 * 1024.0 * 1024.0))
    } else if abs >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1}GB", bytes / (1024.0 * 1024.0 * 1024.0))
    } else if abs >= 1024.0 * 1024.0 {
        format!("{:.0}MB", bytes / (1024.0 * 1024.0))
    } else if abs >= 1024.0 {
        format!("{:.0}KB", bytes / 1024.0)
    } else {
        format!("{:.0}B", bytes)
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
