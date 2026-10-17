//! What the history view draws (D-50): panels in the overview's order, each
//! a title over one strip per series, all on the window's time axis. The
//! CPU and the memory (one strip each, against 100 % and the RAM total);
//! the GPUs' utilization, memory in use, temperature and power (each
//! against its own scale), for one GPU at a time on a host of several
//! (D-107), the power panel also leading with the host's total and each CPU
//! package's power once the CPU's is read (D-106); the disk's read and
//! write and the interface's receive and transmit rates (two strips on one
//! scale, the tallest column of the pair, so their heights compare).

use dgxtop_core::identity::EntityId;
use dgxtop_core::metric::CanonicalUnit;
use dgxtop_core::view::{
    HistoryChartKind, HistoryChartView, HistoryPart, HistoryPointView, HistoryView,
};

use super::nanos_to_seconds;
use crate::format::{clean, format_value, gpu_with_model};

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
            energy: chart
                .kind
                .is_power()
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
    /// The GPU the strip is of (`GPU 0`), whose color its label takes
    /// (D-123); none for a strip of no single GPU.
    pub(super) gpu: Option<String>,
}

/// A titled group of strips.
#[derive(Debug, Clone)]
pub(super) struct Panel<'a> {
    pub(super) title: String,
    /// The title in parts when it names GPUs (`GPU 0 · RTX 5080`, `, `,
    /// `GPU 1 · RTX 3060`), each GPU's with the label whose color it takes
    /// (D-123); empty for a title naming none.
    pub(super) title_parts: Vec<(String, Option<String>)>,
    pub(super) strips: Vec<Strip<'a>>,
    /// The id that folds the panel away (`view::FOLDABLE_PANELS`, D-114);
    /// none for a panel that stays.
    pub(super) fold: Option<&'static str>,
}

impl Panel<'_> {
    /// The panel titled by `parts`, which name GPUs (D-123).
    pub(super) fn titled_by(self, parts: Vec<(String, Option<String>)>) -> Self {
        Self {
            title: parts.iter().map(|(text, _)| text.as_str()).collect(),
            title_parts: parts,
            ..self
        }
    }

    /// The panel, folded away by `fold`.
    fn folding(self, fold: &'static str) -> Self {
        Self {
            fold: Some(fold),
            ..self
        }
    }

    /// Whether the panel draws a device's rates, scaled to what they draw.
    pub(super) fn is_rate(&self) -> bool {
        self.strips
            .iter()
            .any(|strip| !matches!(strip.paint, Paint::Levels | Paint::Count))
    }
}

/// The GPU panels: their series and titles; the power panel's title
/// while only the GPUs' power is read.
const GPU_PANELS: [(HistoryChartKind, &str, &str); 4] = [
    (
        HistoryChartKind::GpuUtilization,
        "GPU utilization",
        "history.gpu_utilization",
    ),
    (
        HistoryChartKind::GpuMemory,
        "GPU memory",
        "history.gpu_memory",
    ),
    (
        HistoryChartKind::GpuTemperature,
        "GPU temperature",
        "history.gpu_temperature",
    ),
    (HistoryChartKind::GpuPower, "GPU power", "history.power"),
];
/// The power panel's title once the host's power leads it (D-106).
const POWER_TITLE: &str = "Power";

/// A rate of a pair: its series, label and paint.
type Rate = (HistoryChartKind, &'static str, Paint);

/// The rate panels: the pair each draws, the title and the fold id.
const RATE_PANELS: [([Rate; 2], &str, &str); 2] = [
    (
        [
            (HistoryChartKind::DiskRead, "read", Paint::Read),
            (HistoryChartKind::DiskWrite, "write", Paint::Write),
        ],
        "Disk",
        "history.disk",
    ),
    (
        [
            (HistoryChartKind::NetworkReceive, "rx", Paint::Receive),
            (HistoryChartKind::NetworkTransmit, "tx", Paint::Transmit),
        ],
        "Network",
        "history.network",
    ),
];

/// A GPU the history charts (D-107).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryGpu {
    pub(crate) entity_id: EntityId,
    /// `GPU 0`, whose color its chip takes (D-123).
    pub(crate) label: String,
    /// `GPU 0 · RTX 5080`: the picker's chip, its label and short model
    /// (D-122).
    pub(crate) chip: String,
}

/// The GPUs the history charts, in the order of their charts.
pub(crate) fn history_gpus(history: &HistoryView) -> Vec<HistoryGpu> {
    let mut gpus: Vec<HistoryGpu> = Vec::new();
    for chart in history.charts.iter().filter(|chart| is_gpu(chart.kind)) {
        let entity_id = &chart.series.entity_id;
        if !gpus.iter().any(|known| known.entity_id == *entity_id) {
            gpus.push(HistoryGpu {
                entity_id: entity_id.clone(),
                label: gpu_label(&chart.subject),
                chip: gpu_with_name(&chart.subject),
            });
        }
    }
    gpus
}

/// `GPU 0 · RTX 5080` of a chart's subject `GPU 0 · NVIDIA GeForce RTX 5080`.
fn gpu_with_name(subject: &str) -> String {
    let subject = clean(subject);
    match subject.split_once(" · ") {
        Some((label, name)) => gpu_with_model(label, name),
        None => subject,
    }
}

/// The GPU the GPU panels show on a host of several (D-107): `picked`
/// while the history charts it, else the first; `None` on a host of one,
/// whose panels show it alone anyway.
pub(crate) fn shown_gpu(history: &HistoryView, picked: Option<&EntityId>) -> Option<EntityId> {
    let gpus = history_gpus(history);
    if gpus.len() < 2 {
        return None;
    }
    picked
        .filter(|picked| gpus.iter().any(|gpu| gpu.entity_id == **picked))
        .cloned()
        .or_else(|| gpus.into_iter().next().map(|gpu| gpu.entity_id))
}

const fn is_gpu(kind: HistoryChartKind) -> bool {
    matches!(
        kind,
        HistoryChartKind::GpuUtilization
            | HistoryChartKind::GpuMemory
            | HistoryChartKind::GpuTemperature
            | HistoryChartKind::GpuPower
    )
}

/// The panels of `history`, in display order, the GPU panels of `gpu`
/// alone when one is given; a series the view does not carry has no
/// strip, and a panel without strips is left out.
pub(super) fn panels<'a>(history: &'a HistoryView, gpu: Option<&EntityId>) -> Vec<Panel<'a>> {
    let series = Series::of_chart;
    let first = |kind| history.charts.iter().find(|chart| chart.kind == kind);
    let mut panels = Vec::new();
    if let Some(cpu) = first(HistoryChartKind::CpuUtilization) {
        panels.push(
            single("CPU utilization".to_owned(), series(cpu), cpu.scale_max).folding("history.cpu"),
        );
    }
    if let Some(memory) = first(HistoryChartKind::MemoryUsed) {
        let title = memory.scale_max.map_or_else(
            || "Memory used".to_owned(),
            |total| format!("Memory used · {}", format_value(total, memory.unit)),
        );
        panels.push(single(title, series(memory), memory.scale_max).folding("history.memory"));
    }
    for (kind, title, fold) in GPU_PANELS {
        let mut strips = if kind == HistoryChartKind::GpuPower {
            host_power_strips(history)
        } else {
            Vec::new()
        };
        let title = if strips.is_empty() {
            title
        } else {
            POWER_TITLE
        };
        strips.extend(
            history
                .charts
                .iter()
                .filter(|chart| chart.kind == kind)
                .filter(|chart| gpu.is_none_or(|gpu| chart.series.entity_id == *gpu))
                .map(gpu_strip),
        );
        if !strips.is_empty() {
            panels.push(Panel {
                title: title.to_owned(),
                title_parts: Vec::new(),
                strips,
                fold: Some(fold),
            });
        }
    }
    for (pair, title, fold) in RATE_PANELS {
        if let Some(panel) = rate_panel(history, pair, title).map(|panel| panel.folding(fold)) {
            panels.push(panel);
        }
    }
    panels
}

/// A strip of `chart` labeled `label`, in the level colors of its scale.
fn level_strip(label: String, chart: &HistoryChartView) -> Strip<'_> {
    Strip {
        label,
        series: Series::of_chart(chart),
        scale: scale_or_peak(chart),
        paint: Paint::Levels,
        gpu: None,
    }
}

/// A GPU's strip, labeled by its GPU and in that GPU's color (D-123).
fn gpu_strip(chart: &HistoryChartView) -> Strip<'_> {
    let label = gpu_label(&chart.subject);
    Strip {
        gpu: Some(label.clone()),
        ..level_strip(label, chart)
    }
}

/// The host's total power (`total`) and each CPU package's (`CPU`, or
/// `CPU 0`, `CPU 1` on a host of several), which lead the power panel once
/// the CPU's power is read (D-106); none before.
fn host_power_strips(history: &HistoryView) -> Vec<Strip<'_>> {
    let packages: Vec<&HistoryChartView> = history
        .charts
        .iter()
        .filter(|chart| chart.kind == HistoryChartKind::CpuPower)
        .collect();
    let total = history
        .charts
        .iter()
        .filter(|chart| chart.kind == HistoryChartKind::TotalPower)
        .map(|chart| level_strip("total".to_owned(), chart));
    let cpus = packages.iter().map(|chart| {
        let label = if packages.len() == 1 {
            "CPU".to_owned()
        } else {
            clean(&chart.subject)
        };
        level_strip(label, chart)
    });
    total.chain(cpus).collect()
}

/// The chart's fixed scale; without one (a GPU that reports no power
/// limit, the host's and the CPU's power), its peak, like a rate (D-41).
fn scale_or_peak(chart: &HistoryChartView) -> Option<f64> {
    chart
        .scale_max
        .or_else(|| chart.peak.filter(|peak| peak.is_finite() && *peak > 0.0))
}

/// A panel of one bounded series.
pub(super) fn single(title: String, series: Series<'_>, scale: Option<f64>) -> Panel<'_> {
    Panel {
        title,
        title_parts: Vec::new(),
        strips: vec![Strip {
            label: String::new(),
            series,
            scale,
            paint: Paint::Levels,
            gpu: None,
        }],
        fold: None,
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
            gpu: None,
        })
        .collect();
    let subject = charts.first().map(|(chart, _, _)| clean(&chart.subject))?;
    Some(Panel {
        title: if subject.is_empty() {
            title.to_owned()
        } else {
            format!("{title} · {subject}")
        },
        title_parts: Vec::new(),
        strips,
        fold: None,
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
        let panels = panels(&history, None);
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

    /// On a host of several GPUs the GPU panels show one GPU (D-107): the
    /// one picked while the history charts it, else the first; the other
    /// panels stay as they are. A host of one GPU has nothing to pick.
    #[test]
    fn the_gpu_panels_show_the_gpu_picked() {
        let history = history_view();
        let gpus = history_gpus(&history);
        assert_eq!(
            gpus.iter().map(|gpu| gpu.chip.as_str()).collect::<Vec<_>>(),
            ["GPU 0 · GB10", "GPU 1 · GB10"]
        );
        assert_eq!(gpus[1].label, "GPU 1");
        let (first, second) = (gpus[0].entity_id.clone(), gpus[1].entity_id.clone());
        assert_eq!(shown_gpu(&history, None), Some(first.clone()));
        assert_eq!(shown_gpu(&history, Some(&second)), Some(second.clone()));
        let gone = EntityId::new("gpu:gone").unwrap();
        assert_eq!(shown_gpu(&history, Some(&gone)), Some(first));
        let picked = layout(&panels(&history, Some(&second)));
        let all = layout(&panels(&history, None));
        assert_eq!(picked.len(), all.len());
        for ((title, labels), (_, every)) in picked.iter().zip(&all) {
            if title.starts_with("GPU") {
                assert_eq!(labels, &["GPU 1".to_owned()], "{title}");
            } else {
                assert_eq!(labels, every, "{title}");
            }
        }
        let mut single = history.clone();
        single
            .charts
            .retain(|chart| chart.series.entity_id != second);
        assert_eq!(history_gpus(&single).len(), 1);
        assert_eq!(shown_gpu(&single, Some(&second)), None);
    }

    /// Once the CPU's power is read the power panel leads with the host's
    /// total and the CPU (D-106), each scaled to its peak, every strip with
    /// its energy; until then it is the GPUs' power panel. The GPUs' strips
    /// are in their colors.
    #[test]
    fn the_power_panel_leads_with_the_total_and_the_cpu_once_read() {
        let power_panel = |history: &HistoryView| -> (String, Vec<String>) {
            let panels = panels(history, None);
            let panel = panels
                .iter()
                .find(|panel| panel.title.ends_with("power") || panel.title == "Power")
                .unwrap();
            (
                panel.title.clone(),
                panel
                    .strips
                    .iter()
                    .map(|strip| strip.label.clone())
                    .collect(),
            )
        };
        let mut history = history_view();
        let gpus = vec!["GPU 0".to_owned(), "GPU 1".to_owned()];
        assert_eq!(
            power_panel(&history),
            ("GPU power".to_owned(), gpus.clone())
        );
        let template = history.charts[0].clone();
        let power = |kind, subject: &str, peak: f64| HistoryChartView {
            kind,
            subject: subject.to_owned(),
            scale_max: None,
            peak: Some(peak),
            mean: Some(peak / 2.0),
            covered_ns: Some(3_600 * 1_000_000_000),
            ..template.clone()
        };
        history
            .charts
            .push(power(HistoryChartKind::TotalPower, "", 180.0));
        history
            .charts
            .push(power(HistoryChartKind::CpuPower, "CPU 0", 45.0));
        let (title, labels) = power_panel(&history);
        assert_eq!(title, "Power");
        assert_eq!(labels[..2], ["total".to_owned(), "CPU".to_owned()]);
        assert_eq!(labels[2..], gpus[..]);
        let panels = panels(&history, None);
        let panel = panels.iter().find(|panel| panel.title == "Power").unwrap();
        // Only a GPU's strip takes a GPU's color (D-123).
        let colored: Vec<Option<&str>> = panel
            .strips
            .iter()
            .map(|strip| strip.gpu.as_deref())
            .collect();
        assert_eq!(colored, [None, None, Some("GPU 0"), Some("GPU 1")]);
        assert_eq!(panel.strips[0].scale, Some(180.0), "scaled to its peak");
        // 90 W for an hour.
        let energy = panel.strips[0].series.energy.unwrap();
        assert!((energy - 324_000.0).abs() < 1e-6, "{energy}");
        assert!(
            panel
                .strips
                .iter()
                .all(|strip| strip.series.energy.is_some())
        );
        // Several packages are named apart.
        history
            .charts
            .push(power(HistoryChartKind::CpuPower, "CPU 1", 40.0));
        assert_eq!(
            power_panel(&history).1[..3],
            ["total".to_owned(), "CPU 0".to_owned(), "CPU 1".to_owned()]
        );
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
        let panels = panels(&history, None);
        let strip = panels
            .iter()
            .find(|panel| panel.title == "GPU power")
            .and_then(|panel| panel.strips.first())
            .unwrap();
        assert_eq!(strip.scale, Some(68.2));
    }
}
