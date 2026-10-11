//! What the history view draws (D-50): panels in the overview's order, each
//! a title over one strip per series, all on the window's time axis. The
//! CPU and the memory (one strip each, against 100 % and the RAM total);
//! the GPUs' utilization, memory in use, temperature and power (a strip per
//! GPU, each against its own scale); the disk's read and write and the
//! interface's receive and transmit rates (two strips on one scale, the
//! tallest column of the pair, so their heights compare).

use dgxtop_core::metric::CanonicalUnit;
use dgxtop_core::view::{
    HistoryChartKind, HistoryChartView, HistoryPart, HistoryPointView, HistoryView,
};

use super::stats::nanos_to_seconds;
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
    /// A count without a bound (threads, processes): one color.
    Count,
}

/// Whether a strip's series has answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SeriesState<'a> {
    Ready,
    Pending,
    /// Its query failed; why.
    Failed(&'a str),
}

/// What a strip draws and its legend says: an Overview History chart or a
/// run's (D-77).
#[derive(Debug, Clone, Copy)]
pub(super) struct Series<'a> {
    pub(super) points: &'a [HistoryPointView],
    /// The tracked processes' parts, stacked at the foot (D-64).
    pub(super) parts: &'a [HistoryPart],
    pub(super) unit: CanonicalUnit,
    /// The legend's first entry: `now` and the newest point's mean, or for
    /// a run that ended `last`.
    pub(super) latest: (&'static str, Option<f64>),
    pub(super) mean: Option<f64>,
    pub(super) peak: Option<f64>,
    /// Joules over the window, for a power strip.
    pub(super) energy: Option<f64>,
    pub(super) state: SeriesState<'a>,
}

impl<'a> Series<'a> {
    /// An Overview History chart.
    fn of_chart(chart: &'a HistoryChartView) -> Self {
        let state = match (&chart.error, chart.pending) {
            (Some(error), _) => SeriesState::Failed(error),
            (None, true) => SeriesState::Pending,
            (None, false) => SeriesState::Ready,
        };
        Self {
            points: &chart.points,
            parts: &chart.parts,
            unit: chart.unit,
            latest: ("now", newest(&chart.points)),
            mean: chart.mean,
            peak: chart.peak,
            // The mean power times the time covered.
            energy: (chart.kind == HistoryChartKind::GpuPower)
                .then(|| Some(chart.mean? * nanos_to_seconds(chart.covered_ns?)))
                .flatten(),
            state,
        }
    }

    pub(super) fn error(&self) -> Option<&'a str> {
        match self.state {
            SeriesState::Failed(error) => Some(error),
            SeriesState::Ready | SeriesState::Pending => None,
        }
    }
}

/// The newest point's mean.
pub(super) fn newest(points: &[HistoryPointView]) -> Option<f64> {
    points.iter().rev().find_map(|point| point.mean)
}

/// One series of a panel.
#[derive(Debug, Clone)]
pub(super) struct Strip<'a> {
    /// What the strip is in a panel of several (`GPU 0`, `read`); empty
    /// in a panel of one.
    pub(super) label: String,
    pub(super) series: Series<'a>,
    /// Top of the scale; `None` for a rate, whose pair is scaled when it
    /// is drawn ([`Panel::is_rate`]).
    pub(super) scale: Option<f64>,
    pub(super) paint: Paint,
}

/// A titled group of strips.
#[derive(Debug, Clone)]
pub(super) struct Panel<'a> {
    pub(super) title: String,
    pub(super) strips: Vec<Strip<'a>>,
}

impl Panel<'_> {
    /// Whether the panel draws a device's rates, scaled to what they draw.
    pub(super) fn is_rate(&self) -> bool {
        self.strips
            .iter()
            .any(|strip| !matches!(strip.paint, Paint::Levels | Paint::Count))
    }
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
    let series = Series::of_chart;
    let first = |kind| history.charts.iter().find(|chart| chart.kind == kind);
    let mut panels = Vec::new();
    if let Some(cpu) = first(HistoryChartKind::CpuUtilization) {
        panels.push(single(
            "CPU utilization".to_owned(),
            series(cpu),
            cpu.scale_max,
        ));
    }
    if let Some(memory) = first(HistoryChartKind::MemoryUsed) {
        let title = memory.scale_max.map_or_else(
            || "Memory used".to_owned(),
            |total| format!("Memory used · {}", format_value(total, memory.unit)),
        );
        panels.push(single(title, series(memory), memory.scale_max));
    }
    for (kind, title) in GPU_PANELS {
        let strips: Vec<Strip<'_>> = history
            .charts
            .iter()
            .filter(|chart| chart.kind == kind)
            .map(|chart| Strip {
                label: gpu_label(&chart.subject),
                series: series(chart),
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
pub(super) fn single(title: String, series: Series<'_>, scale: Option<f64>) -> Panel<'_> {
    Panel {
        title,
        strips: vec![Strip {
            label: String::new(),
            series,
            scale,
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

/// A device's two rates on one scale, set when they are drawn: the tallest
/// column of the two (D-57), so their heights compare.
fn rate_panel<'a>(history: &'a HistoryView, pair: [Rate; 2], title: &str) -> Option<Panel<'a>> {
    let charts: Vec<(&HistoryChartView, &str, Paint)> = pair
        .into_iter()
        .filter_map(|(kind, label, paint)| {
            let chart = history.charts.iter().find(|chart| chart.kind == kind)?;
            Some((chart, label, paint))
        })
        .collect();
    let strips: Vec<Strip<'a>> = charts
        .iter()
        .map(|(chart, label, paint)| Strip {
            label: (*label).to_owned(),
            series: Series::of_chart(chart),
            scale: None,
            paint: *paint,
        })
        .collect();
    let subject = charts.first().map(|(chart, _, _)| clean(&chart.subject))?;
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
        // A rate pair is scaled when drawn, to its tallest column.
        let disk = &panels[6];
        assert!(disk.is_rate() && !panels[5].is_rate());
        assert!(disk.strips.iter().all(|strip| strip.scale.is_none()));
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

    /// A power chart's energy is its mean over the time it holds (D-99):
    /// half an hour recorded at 200 W is 360 kJ whatever the window spans;
    /// nothing recorded, or another series, has none.
    #[test]
    fn a_power_strip_draws_its_mean_over_the_time_it_holds() {
        let mut history = history_view();
        let power = history
            .charts
            .iter_mut()
            .find(|chart| chart.kind == HistoryChartKind::GpuPower)
            .unwrap();
        power.mean = Some(200.0);
        power.covered_ns = Some(1_800 * 1_000_000_000);
        let energy = Series::of_chart(power).energy.unwrap();
        assert!((energy - 360_000.0).abs() < 1e-6, "{energy}");
        power.covered_ns = None;
        assert_eq!(Series::of_chart(power).energy, None);
        let cpu = &history.charts[0];
        assert_eq!(Series::of_chart(cpu).energy, None, "{:?}", cpu.kind);
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
