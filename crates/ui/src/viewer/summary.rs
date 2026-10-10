//! Summary strip: one line per topic (CPU, each GPU, memory, disk, network,
//! processes) built only from the projection. It is the whole overview in
//! the minimal tier and the context line/sidebar of detail views.

use dgxtop_core::commands::ViewKind;
use dgxtop_core::view::{MetricCell, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::format::{cell_tone, clean, value_text};
use crate::theme::ThemePalette;

const MAX_GPU_LINES: usize = 4;
/// Below this width each topic uses two shorter lines.
const NARROW_WIDTH: u16 = 56;

fn value(cell: &MetricCell, palette: &ThemePalette) -> Span<'static> {
    Span::styled(value_text(cell), palette.tone_style(cell_tone(cell)))
}

fn label(text: impl Into<String>, palette: &ThemePalette) -> Span<'static> {
    Span::styled(text.into(), palette.title_style())
}

fn sep(text: &'static str, palette: &ThemePalette) -> Span<'static> {
    Span::styled(text, palette.muted_style())
}

fn no_data(topic: &'static str, palette: &ThemePalette) -> Line<'static> {
    Line::from(vec![
        label(format!("{topic} "), palette),
        Span::styled("no data", palette.muted_style()),
    ])
}

/// Joins a topic's head and tail spans on one line, or splits them into a
/// head line and an indented continuation line when narrow.
fn topic(
    head: Vec<Span<'static>>,
    tail: Vec<Span<'static>>,
    narrow: bool,
    palette: &ThemePalette,
) -> Vec<Line<'static>> {
    if tail.is_empty() {
        return vec![Line::from(head)];
    }
    if narrow {
        let mut continuation = vec![sep("  ", palette)];
        continuation.extend(tail);
        vec![Line::from(head), Line::from(continuation)]
    } else {
        let mut spans = head;
        spans.push(sep(" · ", palette));
        spans.extend(tail);
        vec![Line::from(spans)]
    }
}

fn cpu_lines(model: &ViewModel, palette: &ThemePalette, narrow: bool) -> Vec<Line<'static>> {
    let Some(cpu) = model.modules.cpu.as_deref() else {
        return vec![no_data("CPU", palette)];
    };
    let mut head = vec![label("CPU ", palette), value(&cpu.total, palette)];
    if let Some(temperature) = cpu.package_temperatures.first() {
        head.push(sep(" · ", palette));
        head.push(value(&temperature.cell, palette));
    }
    if let Some(power) = cpu.power.first() {
        head.push(sep(" · ", palette));
        head.push(value(&power.cell, palette));
    }
    let tail = vec![sep("load ", palette), value(&cpu.load[0], palette)];
    topic(head, tail, narrow, palette)
}

fn gpu_lines(model: &ViewModel, palette: &ThemePalette, narrow: bool) -> Vec<Line<'static>> {
    let Some(gpu) = model.modules.gpu.as_deref() else {
        return vec![no_data("GPU", palette)];
    };
    if gpu.cards.is_empty() {
        let reason = gpu
            .unavailable_reason
            .as_deref()
            .map_or_else(|| "none reported".to_owned(), clean);
        return vec![Line::from(vec![
            label("GPU ", palette),
            Span::styled(reason, palette.muted_style()),
        ])];
    }
    let mut lines: Vec<Line<'static>> = gpu
        .cards
        .iter()
        .take(MAX_GPU_LINES)
        .flat_map(|card| {
            let head = vec![
                label(format!("{} ", clean(&card.label)), palette),
                value(&card.utilization, palette),
                sep(" · ", palette),
                value(&card.temperature, palette),
                sep(" · ", palette),
                value(&card.power_draw, palette),
            ];
            let tail = if card.unified_memory {
                vec![
                    sep("RAM held ", palette),
                    value(&card.process_allocated, palette),
                ]
            } else {
                vec![
                    sep("mem ", palette),
                    value(&card.framebuffer_used, palette),
                    sep(" / ", palette),
                    value(&card.framebuffer_total, palette),
                ]
            };
            topic(head, tail, narrow, palette)
        })
        .collect();
    if gpu.cards.len() > MAX_GPU_LINES {
        lines.push(Line::from(Span::styled(
            format!("+{} more GPUs", gpu.cards.len() - MAX_GPU_LINES),
            palette.muted_style(),
        )));
    }
    lines
}

fn memory_lines(model: &ViewModel, palette: &ThemePalette, narrow: bool) -> Vec<Line<'static>> {
    let Some(memory) = model.modules.memory.as_deref() else {
        return vec![no_data("MEM", palette)];
    };
    let head = vec![
        label("MEM ", palette),
        value(&memory.used, palette),
        sep(" / ", palette),
        value(&memory.total, palette),
    ];
    let tail = vec![
        sep("swap ", palette),
        value(&memory.swap_used, palette),
        sep(" / ", palette),
        value(&memory.swap_total, palette),
    ];
    topic(head, tail, narrow, palette)
}

fn more(count: usize, palette: &ThemePalette) -> Option<Span<'static>> {
    (count > 1).then(|| Span::styled(format!(" (+{})", count - 1), palette.muted_style()))
}

fn disk_lines(model: &ViewModel, palette: &ThemePalette, narrow: bool) -> Vec<Line<'static>> {
    let Some(disk) = model.modules.disk.as_deref() else {
        return vec![no_data("DISK", palette)];
    };
    let Some(device) = disk.devices.first() else {
        return vec![Line::from(vec![
            label("DISK ", palette),
            Span::styled("no devices", palette.muted_style()),
        ])];
    };
    let mut head = vec![label(format!("DISK {}", clean(&device.name)), palette)];
    head.extend(more(disk.devices.len(), palette));
    let tail = vec![
        sep("r ", palette),
        value(&device.read_bps, palette),
        sep(" w ", palette),
        value(&device.write_bps, palette),
    ];
    topic(head, tail, narrow, palette)
}

fn network_lines(model: &ViewModel, palette: &ThemePalette, narrow: bool) -> Vec<Line<'static>> {
    let Some(network) = model.modules.network.as_deref() else {
        return vec![no_data("NET", palette)];
    };
    let Some(interface) = network.interfaces.first() else {
        return vec![Line::from(vec![
            label("NET ", palette),
            Span::styled("no interfaces", palette.muted_style()),
        ])];
    };
    let mut head = vec![label(format!("NET {}", clean(&interface.name)), palette)];
    head.extend(more(network.interfaces.len(), palette));
    let tail = vec![
        sep("rx ", palette),
        value(&interface.rx_bps, palette),
        sep(" tx ", palette),
        value(&interface.tx_bps, palette),
    ];
    topic(head, tail, narrow, palette)
}

fn process_lines(model: &ViewModel, palette: &ThemePalette, narrow: bool) -> Vec<Line<'static>> {
    let Some(processes) = model.modules.processes.as_deref() else {
        return vec![no_data("PROC", palette)];
    };
    let head = vec![
        label("PROC ", palette),
        Span::styled(
            format!("{} rows", processes.total_rows),
            palette.text_style(),
        ),
    ];
    let tail = processes
        .rows
        .first()
        .map(|top| {
            vec![
                sep("top ", palette),
                Span::styled(
                    format!("{} (pid {})", clean(&top.name), top.key.pid),
                    palette.text_style(),
                ),
            ]
        })
        .unwrap_or_default();
    topic(head, tail, narrow, palette)
}

/// Every summary line for a width, most important first.
/// Summary lines. In the overview every topic has a line (a missing one says
/// "no data"); a detail view's context shows only the topics its projection
/// carries, minus the view's own topic, so it can be empty.
pub fn summary_lines(model: &ViewModel, palette: &ThemePalette, width: u16) -> Vec<Line<'static>> {
    let narrow = width < NARROW_WIDTH;
    let view = model.session.view;
    let modules = &model.modules;
    let topics: [(bool, bool, TopicLines); 6] = [
        (modules.cpu.is_some(), false, cpu_lines),
        (
            modules.gpu.is_some(),
            view == ViewKind::GpuDetail,
            gpu_lines,
        ),
        (modules.memory.is_some(), false, memory_lines),
        (modules.disk.is_some(), false, disk_lines),
        (modules.network.is_some(), false, network_lines),
        (
            modules.processes.is_some(),
            view == ViewKind::Processes,
            process_lines,
        ),
    ];
    let overview = view == ViewKind::Overview;
    topics
        .into_iter()
        .filter(|(present, own_topic, _)| overview || (*present && !*own_topic))
        .flat_map(|(_, _, lines)| lines(model, palette, narrow))
        .collect()
}

type TopicLines = fn(&ViewModel, &ThemePalette, bool) -> Vec<Line<'static>>;

/// Compact one-line form: `CPU 45.3% │ GPU 0 90.0% │ MEM 12.3 GiB / 64.0 GiB`.
pub fn summary_one_line(model: &ViewModel, palette: &ThemePalette) -> Line<'static> {
    let mut spans = Vec::new();
    if let Some(cpu) = model.modules.cpu.as_deref() {
        spans.push(label("CPU ", palette));
        spans.push(value(&cpu.total, palette));
    }
    if let Some(gpu) = model.modules.gpu.as_deref() {
        for card in gpu.cards.iter().take(MAX_GPU_LINES) {
            spans.push(sep(" │ ", palette));
            spans.push(label(format!("{} ", clean(&card.label)), palette));
            spans.push(value(&card.utilization, palette));
        }
    }
    if let Some(memory) = model.modules.memory.as_deref() {
        spans.push(sep(" │ ", palette));
        spans.push(label("MEM ", palette));
        spans.push(value(&memory.used, palette));
        spans.push(sep(" / ", palette));
        spans.push(value(&memory.total, palette));
    }
    if spans.is_empty() {
        spans.push(Span::styled(
            "summary: no data in this projection",
            palette.muted_style(),
        ));
    } else if matches!(spans.first(), Some(span) if span.content.starts_with(" │")) {
        spans.remove(0);
    }
    Line::from(spans)
}

/// Draws the summary into `area` (one compact line when only one row fits).
pub fn render(frame: &mut Frame<'_>, area: Rect, model: &ViewModel, palette: &ThemePalette) {
    if area.is_empty() {
        return;
    }
    if area.height == 1 {
        frame.buffer_mut().set_line(
            area.x,
            area.y,
            &summary_one_line(model, palette),
            area.width,
        );
        return;
    }
    let lines = summary_lines(model, palette, area.width);
    let capacity = usize::from(area.height);
    for (offset, line) in lines.iter().take(capacity).enumerate() {
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        frame
            .buffer_mut()
            .set_line(area.x, area.y + offset, line, area.width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures;
    use dgxtop_core::commands::ViewKind;

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn missing_topics_say_no_data() {
        let mut model = test_fixtures::view_model(ViewKind::Overview, 1);
        model.modules = dgxtop_core::view::ModuleViews::default();
        let lines: Vec<String> = summary_lines(&model, &ThemePalette::default(), 120)
            .iter()
            .map(text)
            .collect();
        assert!(lines.iter().all(|line| line.ends_with("no data")));
        assert_eq!(
            text(&summary_one_line(&model, &ThemePalette::default())),
            "summary: no data in this projection"
        );
    }
}
