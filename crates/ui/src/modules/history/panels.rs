//! What the history view draws (D-50): panels in the overview's order, each
//! a title over one strip per series, all on the window's time axis. The
//! CPU and the memory (one strip each, against 100 % and the RAM total);
//! the GPUs' utilization, memory in use, temperature and power (a strip per
//! GPU, each against its own scale); the disk's read and write and the
//! interface's receive and transmit rates (two strips on one scale, the
//! larger peak of the pair, so their heights compare).

use dgxtop_core::view::{HistoryChartKind, HistoryChartView, HistoryView};

use crate::format::{clean, format_value};

/// How a strip colors its columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Paint {
    /// The gauge color of each column's share of the scale: bounded series.
    Levels,
    /// The overview's rate colors.
    Read,
    Write,
    Receive,
    Transmit,
}

/// One series of a panel.
#[derive(Debug, Clone)]
pub(super) struct Strip<'a> {
    /// What the strip is in a panel of several (`GPU 0`, `read`); empty
    /// in a panel of one.
    pub(super) label: String,
    pub(super) chart: &'a HistoryChartView,
    /// Top of the scale; `None` while a rate pair has no data.
    pub(super) scale: Option<f64>,
    pub(super) paint: Paint,
}

/// A titled group of strips.
#[derive(Debug, Clone)]
pub(super) struct Panel<'a> {
    pub(super) title: String,
    pub(super) strips: Vec<Strip<'a>>,
}

/// The GPU panels: their series and titles.
const GPU_PANELS: [(HistoryChartKind, &str); 4] = [
    (HistoryChartKind::GpuUtilization, "GPU utilization"),
    (HistoryChartKind::GpuMemory, "GPU memory"),
    (HistoryChartKind::GpuTemperature, "GPU temperature"),
    (HistoryChartKind::GpuPower, "GPU power"),
];

/// A rate of a pair: its series, label and paint.
type Rate = (HistoryChartKind, &'static str, Paint);

/// The rate panels: the pair each draws and the title.
const RATE_PANELS: [([Rate; 2], &str); 2] = [
    (
        [
            (HistoryChartKind::DiskRead, "read", Paint::Read),
            (HistoryChartKind::DiskWrite, "write", Paint::Write),
        ],
        "Disk",
    ),
    (
        [
            (HistoryChartKind::NetworkReceive, "rx", Paint::Receive),
            (HistoryChartKind::NetworkTransmit, "tx", Paint::Transmit),
        ],
        "Network",
    ),
];

/// The panels of `history`, in display order; a series the view does not
/// carry has no strip, and a panel without strips is left out.
pub(super) fn panels(history: &HistoryView) -> Vec<Panel<'_>> {
    let first = |kind| history.charts.iter().find(|chart| chart.kind == kind);
    let mut panels = Vec::new();
    if let Some(cpu) = first(HistoryChartKind::CpuUtilization) {
        panels.push(single("CPU utilization".to_owned(), cpu));
    }
    if let Some(memory) = first(HistoryChartKind::MemoryUsed) {
        let title = memory.scale_max.map_or_else(
            || "Memory used".to_owned(),
            |total| format!("Memory used · {}", format_value(total, memory.unit)),
        );
        panels.push(single(title, memory));
    }
    for (kind, title) in GPU_PANELS {
        let strips: Vec<Strip<'_>> = history
            .charts
            .iter()
            .filter(|chart| chart.kind == kind)
            .map(|chart| Strip {
                label: gpu_label(&chart.subject),
                chart,
                scale: scale_or_peak(chart),
                paint: Paint::Levels,
            })
            .collect();
        if !strips.is_empty() {
            panels.push(Panel {
                title: title.to_owned(),
                strips,
            });
        }
    }
    for (pair, title) in RATE_PANELS {
        if let Some(panel) = rate_panel(history, pair, title) {
            panels.push(panel);
        }
    }
    panels
}

/// The chart's fixed scale; without one (a GPU that reports no power
/// limit), its peak, like a rate (D-41).
fn scale_or_peak(chart: &HistoryChartView) -> Option<f64> {
    chart
        .scale_max
        .or_else(|| chart.peak.filter(|peak| peak.is_finite() && *peak > 0.0))
}

/// A panel of one bounded series.
fn single(title: String, chart: &HistoryChartView) -> Panel<'_> {
    Panel {
        title,
        strips: vec![Strip {
            label: String::new(),
            chart,
            scale: chart.scale_max,
            paint: Paint::Levels,
        }],
    }
}

/// `GPU 0` of `GPU 0 · NVIDIA GB10`.
fn gpu_label(subject: &str) -> String {
    let subject = clean(subject);
    subject
        .split(" · ")
        .next()
        .map_or_else(|| subject.clone(), str::to_owned)
}

/// A device's two rates on one scale: the larger peak of the two.
fn rate_panel<'a>(history: &'a HistoryView, pair: [Rate; 2], title: &str) -> Option<Panel<'a>> {
    let charts = pair.map(|(kind, label, paint)| {
        let chart = history.charts.iter().find(|chart| chart.kind == kind);
        (chart, label, paint)
    });
    let scale = charts
        .iter()
        .filter_map(|(chart, _, _)| chart.and_then(|chart| chart.peak))
        .filter(|peak| peak.is_finite() && *peak > 0.0)
        .reduce(f64::max);
    let strips: Vec<Strip<'a>> = charts
        .into_iter()
        .filter_map(|(chart, label, paint)| {
            Some(Strip {
                label: label.to_owned(),
                chart: chart?,
                scale,
                paint,
            })
        })
        .collect();
    let subject = strips.first().map(|strip| clean(&strip.chart.subject))?;
    Some(Panel {
        title: if subject.is_empty() {
            title.to_owned()
        } else {
            format!("{title} · {subject}")
        },
        strips,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::history_view;

    fn layout(panels: &[Panel<'_>]) -> Vec<(String, Vec<String>)> {
        panels
            .iter()
            .map(|panel| {
                (
                    panel.title.clone(),
                    panel
                        .strips
                        .iter()
                        .map(|strip| strip.label.clone())
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn panels_follow_the_overview_with_a_strip_per_gpu() {
        let history = history_view();
        let panels = panels(&history);
        let strings = |labels: &[&str]| -> Vec<String> {
            labels.iter().map(|label| (*label).to_owned()).collect()
        };
        let gpus = strings(&["GPU 0", "GPU 1"]);
        assert_eq!(
            layout(&panels),
            vec![
                ("CPU utilization".to_owned(), strings(&[""])),
                ("Memory used · 64.0 GiB".to_owned(), strings(&[""])),
                ("GPU utilization".to_owned(), gpus.clone()),
                ("GPU memory".to_owned(), gpus.clone()),
                ("GPU temperature".to_owned(), gpus.clone()),
                ("GPU power".to_owned(), gpus),
                ("Disk · nvme0n1".to_owned(), strings(&["read", "write"])),
                ("Network · enp1s0".to_owned(), strings(&["rx", "tx"])),
            ]
        );
        // Each GPU strip keeps its own scale (its framebuffer, its limit).
        let memory = &panels[3];
        assert_eq!(
            memory
                .strips
                .iter()
                .map(|strip| strip.scale)
                .collect::<Vec<_>>(),
            vec![Some(80.0 * 1_073_741_824.0), Some(64.0 * 1_073_741_824.0)]
        );
        // A rate pair shares the larger peak, so its heights compare.
        let disk = &panels[6];
        let peaks: Vec<Option<f64>> = disk.strips.iter().map(|strip| strip.chart.peak).collect();
        let larger = peaks.iter().flatten().copied().reduce(f64::max);
        assert!(disk.strips.iter().all(|strip| strip.scale == larger));
        assert_eq!(
            disk.strips
                .iter()
                .map(|strip| strip.paint)
                .collect::<Vec<_>>(),
            vec![Paint::Read, Paint::Write]
        );
        assert_eq!(gpu_label("GPU 1 · NVIDIA GB10"), "GPU 1");
        assert_eq!(gpu_label("nameless"), "nameless");
    }

    /// GB10 reports no power limit: its power strip is scaled to its peak,
    /// which the gutter prints, instead of a scale left blank.
    #[test]
    fn a_gpu_strip_without_a_scale_takes_its_peak() {
        let mut history = history_view();
        let power = history
            .charts
            .iter_mut()
            .find(|chart| chart.kind == HistoryChartKind::GpuPower)
            .unwrap();
        power.scale_max = None;
        power.peak = Some(68.2);
        let panels = panels(&history);
        let strip = panels
            .iter()
            .find(|panel| panel.title == "GPU power")
            .and_then(|panel| panel.strips.first())
            .unwrap();
        assert_eq!(strip.scale, Some(68.2));
    }
}
