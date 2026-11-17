//! What the history's chart pages draw (D-50, D-148): panels, each a title
//! over one strip per series, all on the window's time axis.
//!
//! The Overview History is the whole machine ([`Page::Overview`]): the CPU
//! and the memory (one strip each, against 100 % and the RAM total), the
//! memory's also with its stall pressure, `some` and `full`, where the
//! kernel reports it (D-135), unless a key of their own folded those two
//! strips away (D-144); every temperature the machine reports, each CPU
//! package's, the SoC's (D-137), each GPU's and the charted drive's; the
//! power, the host's total and each CPU package's once the CPU's is read
//! (D-106), else each GPU's; the disk's read and write and the interface's
//! receive and transmit rates (two strips on one scale, the tallest column
//! of the pair, so their heights compare).
//!
//! The GPU History is one GPU ([`Page::Gpu`]), the one picked on a host of
//! several (D-107): its utilization, with the share of time its memory was
//! read or written under it, its memory in use, its temperature, with its
//! fan's speed under it, and its power, each against its own scale. A
//! companion strip that stayed at zero over the whole window (a fan that
//! never turned, GB10's memory activity) is left out: no chart stands
//! empty.

use std::collections::BTreeSet;

use dgxtop_core::identity::EntityId;
use dgxtop_core::metric::CanonicalUnit;
use dgxtop_core::view::{
    HistoryChartKind, HistoryChartView, HistoryPart, HistoryPointView, HistoryView,
};

use super::nanos_to_seconds;
use crate::format::{clean, format_value, gpu_with_model};
use crate::modules::folds::MEMORY_PRESSURE_STRIPS;

/// How a strip colors its columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Paint {
    /// The gauge color of each column's share of the scale: bounded series.
    Levels,
    /// The level color of a stall pressure (D-135): green up to 1 %, red
    /// from 20 %, whatever height the strip's own scale gives the column.
    Pressure,
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
    /// The strips a key of their own folds away (D-144); none for a panel
    /// whose strips all stay.
    pub(super) strips_fold: Option<StripsFold<'a>>,
}

/// Strips of a panel a key of their own folds away (D-144), and what the
/// panel is without them.
#[derive(Debug, Clone)]
pub(super) struct StripsFold<'a> {
    /// The id that folds them (`view::FOLDABLE_PANELS`).
    pub(super) id: &'static str,
    /// The panel's title without them.
    title: String,
    /// The strips that stay.
    strips: Vec<Strip<'a>>,
}

impl Panel<'_> {
    /// The panel as `folded` leaves it: without the strips a key of their
    /// own folded away (D-144), under the title it has without them.
    pub(super) fn with_strips_folded(mut self, folded: &BTreeSet<String>) -> Self {
        let fold = self
            .strips_fold
            .as_mut()
            .filter(|fold| folded.contains(fold.id));
        if let Some(fold) = fold {
            std::mem::swap(&mut self.title, &mut fold.title);
            std::mem::swap(&mut self.strips, &mut fold.strips);
        }
        self
    }

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
            .any(|strip| !matches!(strip.paint, Paint::Levels | Paint::Pressure | Paint::Count))
    }
}

/// The whole machine's temperature and power panels (D-148): their
/// titles and fold ids.
const TEMPERATURE_PANEL: (&str, &str) = ("Temperature", "history.host_temperature");
const POWER_PANEL: (&str, &str) = ("Power", "history.host_power");
/// The power panel of a machine that reads its GPUs' draw alone.
const GPU_POWER_TITLES: (&str, &str) = ("Power · GPU", "Power · GPUs");
/// The top of a pressure strip's scale at least (D-135): its peak, but a
/// stall of a hundredth of a percent must not fill the strip.
const MIN_PRESSURE_SCALE: f64 = 1.0;

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
            | HistoryChartKind::GpuMemoryActivity
            | HistoryChartKind::GpuMemory
            | HistoryChartKind::GpuTemperature
            | HistoryChartKind::GpuFanSpeed
            | HistoryChartKind::GpuPower
    )
}

/// The page of panels a history tab shows (D-148).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Page<'a> {
    /// The whole machine: the Overview History.
    Overview,
    /// One GPU, the GPU History: the one given on a host of several, the
    /// only one otherwise.
    Gpu(Option<&'a EntityId>),
}

/// The panels of `page`, in display order; a series the view does not
/// carry has no strip, and a panel without strips is left out.
pub(super) fn panels<'a>(history: &'a HistoryView, page: Page<'_>) -> Vec<Panel<'a>> {
    match page {
        Page::Overview => machine_panels(history),
        Page::Gpu(gpu) => gpu_panels(history, gpu),
    }
}

/// The Overview History's panels: what is the same whichever GPU is
/// looked at (D-148).
fn machine_panels(history: &HistoryView) -> Vec<Panel<'_>> {
    let series = Series::of_chart;
    let first = |kind| history.charts.iter().find(|chart| chart.kind == kind);
    let mut panels = Vec::new();
    if let Some(cpu) = first(HistoryChartKind::CpuUtilization) {
        panels.push(
            single("CPU utilization".to_owned(), series(cpu), cpu.scale_max).folding("history.cpu"),
        );
    }
    if let Some(memory) = first(HistoryChartKind::MemoryUsed) {
        panels.push(memory_panel(history, memory).folding("history.memory"));
    }
    panels.extend(group(
        TEMPERATURE_PANEL,
        machine_temperature_strips(history),
    ));
    panels.extend(machine_power_panel(history));
    for (pair, title, fold) in RATE_PANELS {
        if let Some(panel) = rate_panel(history, pair, title).map(|panel| panel.folding(fold)) {
            panels.push(panel);
        }
    }
    panels
}

/// The GPU History's panels of `gpu`, the first GPU charted when none is
/// given: everything on the page is that GPU's (D-148).
fn gpu_panels<'a>(history: &'a HistoryView, gpu: Option<&EntityId>) -> Vec<Panel<'a>> {
    let of_gpu = |kind| {
        history
            .charts
            .iter()
            .filter(|chart| chart.kind == kind)
            .find(|chart| gpu.is_none_or(|gpu| chart.series.entity_id == *gpu))
    };
    let pairs = [
        (
            ("GPU utilization", HistoryChartKind::GpuUtilization, "util"),
            Some(("memory busy", HistoryChartKind::GpuMemoryActivity, "busy")),
            "history.gpu_utilization",
        ),
        (
            ("GPU memory", HistoryChartKind::GpuMemory, ""),
            None,
            "history.gpu_memory",
        ),
        (
            ("GPU temperature", HistoryChartKind::GpuTemperature, "temp"),
            Some(("fan", HistoryChartKind::GpuFanSpeed, "fan")),
            "history.gpu_thermal",
        ),
        (
            ("GPU power", HistoryChartKind::GpuPower, ""),
            None,
            "history.gpu_power",
        ),
    ];
    pairs
        .into_iter()
        .filter_map(|((title, kind, label), companion, fold)| {
            let chart = of_gpu(kind)?;
            let companion = companion
                .and_then(|(name, kind, label)| Some((name, of_gpu(kind)?, label)))
                .filter(|(_, chart, _)| has_moved(chart));
            Some(with_companion((title, chart, label), companion).folding(fold))
        })
        .collect()
}

/// A GPU's panel: `chart` alone under `title`, its strip named by its GPU
/// for whoever reads the page aloud, or over the `companion` charted with
/// it, each under its label in the GPU's color (D-123), the title naming
/// both (`GPU temperature and fan`).
fn with_companion<'a>(
    (title, chart, label): (&str, &'a HistoryChartView, &str),
    companion: Option<(&str, &'a HistoryChartView, &str)>,
) -> Panel<'a> {
    let (title, strips) = match companion {
        None => (title.to_owned(), vec![gpu_strip(chart)]),
        Some((name, other, other_label)) => {
            let labeled = |label: &str, chart: &'a HistoryChartView| Strip {
                label: label.to_owned(),
                ..gpu_strip(chart)
            };
            (
                format!("{title} and {name}"),
                vec![labeled(label, chart), labeled(other_label, other)],
            )
        }
    };
    Panel {
        title,
        title_parts: Vec::new(),
        strips,
        fold: None,
        strips_fold: None,
    }
}

/// Whether `chart` ever left zero in the window: a companion strip that
/// did not is left out (module docs).
fn has_moved(chart: &HistoryChartView) -> bool {
    chart.peak.is_some_and(|peak| peak > 0.0)
}

/// A panel of the whole machine: `strips` under `title`, folded by `fold`;
/// none without strips.
fn group<'a>((title, fold): (&str, &'static str), strips: Vec<Strip<'a>>) -> Option<Panel<'a>> {
    (!strips.is_empty()).then(|| Panel {
        title: title.to_owned(),
        title_parts: Vec::new(),
        strips,
        fold: Some(fold),
        strips_fold: None,
    })
}

/// The memory panel: the RAM in use against the RAM total, alone until the
/// kernel reports the memory's stall pressure, then with the strips `some`
/// and `full` under it (D-135), each scaled to its peak (1 % at least) and
/// colored by how much time was lost. A key of their own folds those two
/// away (D-144): the panel is then the one a kernel without PSI has.
fn memory_panel<'a>(history: &'a HistoryView, used: &'a HistoryChartView) -> Panel<'a> {
    let total = used
        .scale_max
        .map(|total| format!(" · {}", format_value(total, used.unit)))
        .unwrap_or_default();
    let pressure: Vec<Strip<'a>> = [
        (HistoryChartKind::MemoryPressureSome, "some"),
        (HistoryChartKind::MemoryPressureFull, "full"),
    ]
    .into_iter()
    .filter_map(|(kind, label)| {
        let chart = history.charts.iter().find(|chart| chart.kind == kind)?;
        Some(pressure_strip(label, chart))
    })
    .collect();
    let alone = single(
        format!("Memory used{total}"),
        Series::of_chart(used),
        used.scale_max,
    );
    if pressure.is_empty() {
        return alone;
    }
    let mut strips = vec![level_strip("used".to_owned(), used)];
    strips.extend(pressure);
    Panel {
        title: format!("Memory used and pressure{total}"),
        title_parts: Vec::new(),
        strips,
        fold: None,
        strips_fold: Some(StripsFold {
            id: MEMORY_PRESSURE_STRIPS,
            title: alone.title,
            strips: alone.strips,
        }),
    }
}

/// A stall pressure's strip (D-135).
fn pressure_strip<'a>(label: &str, chart: &'a HistoryChartView) -> Strip<'a> {
    let peak = chart.peak.filter(|peak| peak.is_finite()).unwrap_or(0.0);
    Strip {
        label: label.to_owned(),
        series: Series::of_chart(chart),
        scale: Some(peak.max(MIN_PRESSURE_SCALE)),
        paint: Paint::Pressure,
        gpu: None,
    }
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

/// The strips of every chart of `kind`, each a GPU's.
fn gpu_strips(history: &HistoryView, kind: HistoryChartKind) -> impl Iterator<Item = Strip<'_>> {
    history
        .charts
        .iter()
        .filter(move |chart| chart.kind == kind)
        .map(gpu_strip)
}

/// The strips of the CPU packages charted as `kind`: `CPU`, or `CPU 0`,
/// `CPU 1` on a host of several.
fn package_strips(history: &HistoryView, kind: HistoryChartKind) -> Vec<Strip<'_>> {
    let packages: Vec<&HistoryChartView> = history
        .charts
        .iter()
        .filter(|chart| chart.kind == kind)
        .collect();
    let alone = packages.len() == 1;
    packages
        .into_iter()
        .map(|chart| {
            let label = if alone {
                "CPU".to_owned()
            } else {
                clean(&chart.subject)
            };
            level_strip(label, chart)
        })
        .collect()
}

/// What the machine draws (D-106, D-148): the host's total (`total`) and
/// each CPU package's power once the CPU's is read. A machine that reports
/// neither shows each GPU's draw, the only power it reads, and the title
/// says so (`Power · GPU`): a GPU's draw is not the machine's, and a lone
/// strip has no label to say whose it is. With a total the GPUs' are its
/// parts, charted on the GPU History.
fn machine_power_panel(history: &HistoryView) -> Option<Panel<'_>> {
    let total = history
        .charts
        .iter()
        .filter(|chart| chart.kind == HistoryChartKind::TotalPower)
        .map(|chart| level_strip("total".to_owned(), chart));
    let host: Vec<Strip<'_>> = total
        .chain(package_strips(history, HistoryChartKind::CpuPower))
        .collect();
    if !host.is_empty() {
        return group(POWER_PANEL, host);
    }
    let gpus: Vec<Strip<'_>> = gpu_strips(history, HistoryChartKind::GpuPower).collect();
    let (one, several) = GPU_POWER_TITLES;
    let title = if gpus.len() == 1 { one } else { several };
    group((title, POWER_PANEL.1), gpus)
}

/// Every temperature the machine reports (D-137, D-148): each CPU
/// package's, the SoC's (`SoC`), each GPU's in its color and the charted
/// drive's (`disk`), all against the same 100 °C.
fn machine_temperature_strips(history: &HistoryView) -> Vec<Strip<'_>> {
    let of = |kind, label: &'static str| {
        history
            .charts
            .iter()
            .filter(move |chart| chart.kind == kind)
            .map(move |chart| level_strip(label.to_owned(), chart))
    };
    package_strips(history, HistoryChartKind::CpuTemperature)
        .into_iter()
        .chain(of(HistoryChartKind::SocTemperature, "SoC"))
        .chain(gpu_strips(history, HistoryChartKind::GpuTemperature))
        .chain(of(HistoryChartKind::DiskTemperature, "disk"))
        .collect()
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
        strips_fold: None,
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
        strips_fold: None,
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

    fn strings(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|label| (*label).to_owned()).collect()
    }

    fn folds(panels: &[Panel<'_>]) -> Vec<Option<&'static str>> {
        panels.iter().map(|panel| panel.fold).collect()
    }

    /// A chart like `template` of another series.
    fn chart_of(
        template: &HistoryChartView,
        kind: HistoryChartKind,
        subject: &str,
        unit: CanonicalUnit,
        (scale_max, peak): (Option<f64>, f64),
    ) -> HistoryChartView {
        HistoryChartView {
            kind,
            subject: subject.to_owned(),
            unit,
            scale_max,
            peak: Some(peak),
            mean: Some(peak / 4.0),
            ..template.clone()
        }
    }

    /// A chart of `gpu` (0 or 1) like the fixture's of `like`.
    fn gpu_chart(
        history: &HistoryView,
        (like, kind): (HistoryChartKind, HistoryChartKind),
        gpu: usize,
        unit: CanonicalUnit,
        peak: f64,
    ) -> HistoryChartView {
        let template = history
            .charts
            .iter()
            .filter(|chart| chart.kind == like)
            .nth(gpu)
            .unwrap();
        chart_of(template, kind, &template.subject, unit, (Some(100.0), peak))
    }

    /// The Overview History holds what is the same whichever GPU is looked
    /// at (D-148): the CPU, the memory, the machine's temperatures and
    /// power, the disk and the network, and no panel of one GPU.
    #[test]
    fn the_overview_history_is_the_whole_machine() {
        let history = history_view();
        let panels = panels(&history, Page::Overview);
        let gpus = strings(&["GPU 0", "GPU 1"]);
        assert_eq!(
            layout(&panels),
            vec![
                ("CPU utilization".to_owned(), strings(&[""])),
                ("Memory used · 64.0 GiB".to_owned(), strings(&[""])),
                ("Temperature".to_owned(), gpus.clone()),
                ("Power · GPUs".to_owned(), gpus),
                ("Disk · nvme0n1".to_owned(), strings(&["read", "write"])),
                ("Network · enp1s0".to_owned(), strings(&["rx", "tx"])),
            ]
        );
        assert_eq!(
            folds(&panels),
            [
                "history.cpu",
                "history.memory",
                "history.host_temperature",
                "history.host_power",
                "history.disk",
                "history.network"
            ]
            .map(Some)
        );
        // A rate pair is scaled when drawn, to its tallest column.
        let disk = &panels[4];
        assert!(disk.is_rate() && !panels[3].is_rate());
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

    /// The GPU History is one GPU (D-107, D-148): the one picked while the
    /// history charts it, else the first, and every strip on the page is
    /// that GPU's, so switching the GPU changes all of them. A host of one
    /// GPU has nothing to pick.
    #[test]
    fn the_gpu_history_is_one_gpu() {
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
        assert_eq!(shown_gpu(&history, Some(&gone)), Some(first.clone()));
        let titles = strings(&[
            "GPU utilization",
            "GPU memory",
            "GPU temperature",
            "GPU power",
        ]);
        let page = |gpu: &EntityId| panels(&history, Page::Gpu(Some(gpu)));
        for gpu in [&first, &second] {
            let panels = page(gpu);
            let shown: Vec<String> = panels.iter().map(|panel| panel.title.clone()).collect();
            assert_eq!(shown, titles);
            assert_eq!(
                folds(&panels),
                [
                    "history.gpu_utilization",
                    "history.gpu_memory",
                    "history.gpu_thermal",
                    "history.gpu_power"
                ]
                .map(Some)
            );
            let label = gpu_label(
                &history_gpus(&history)
                    .iter()
                    .find(|shown| shown.entity_id == *gpu)
                    .unwrap()
                    .chip,
            );
            for panel in &panels {
                let strip = &panel.strips[0];
                assert_eq!((panel.strips.len(), &strip.label), (1, &label));
                assert_eq!(strip.gpu.as_ref(), Some(&label));
                let of_this_gpu = history
                    .charts
                    .iter()
                    .filter(|chart| chart.series.entity_id == *gpu)
                    .any(|chart| std::ptr::eq(chart.points.as_slice(), strip.series.points));
                assert!(of_this_gpu, "{}: another GPU's series", panel.title);
            }
        }
        // Each GPU keeps its own scale (its framebuffer, its limit).
        let memory_scale = |gpu: &EntityId| page(gpu)[1].strips[0].scale;
        assert_eq!(memory_scale(&first), Some(80.0 * 1_073_741_824.0));
        assert_eq!(memory_scale(&second), Some(64.0 * 1_073_741_824.0));
        // No pick: the first GPU's page.
        assert_eq!(
            panels(&history, Page::Gpu(None))[1].strips[0].scale,
            memory_scale(&first)
        );
        let mut single = history.clone();
        single
            .charts
            .retain(|chart| chart.series.entity_id != second);
        assert_eq!(history_gpus(&single).len(), 1);
        assert_eq!(shown_gpu(&single, Some(&second)), None);
        assert_eq!(panels(&single, Page::Gpu(None)).len(), 4);
        // A host without a GPU has no GPU page to draw.
        single.charts.retain(|chart| !is_gpu(chart.kind));
        assert!(panels(&single, Page::Gpu(None)).is_empty());
    }

    /// A GPU's memory activity goes under its utilization and its fan under
    /// its temperature (D-148), each named in the title and against 100 %;
    /// one that stayed at zero over the window is left out, as is one the
    /// GPU does not report, so no strip stands empty.
    #[test]
    fn a_gpus_companion_strips_show_once_they_move() {
        let mut history = history_view();
        let first = history_gpus(&history)[0].entity_id.clone();
        let second = history_gpus(&history)[1].entity_id.clone();
        let activity = gpu_chart(
            &history,
            (
                HistoryChartKind::GpuUtilization,
                HistoryChartKind::GpuMemoryActivity,
            ),
            0,
            CanonicalUnit::Percent,
            40.0,
        );
        let fan = gpu_chart(
            &history,
            (
                HistoryChartKind::GpuTemperature,
                HistoryChartKind::GpuFanSpeed,
            ),
            0,
            CanonicalUnit::Percent,
            0.0,
        );
        history.charts.extend([activity, fan]);
        let shown =
            |history: &HistoryView, gpu: &EntityId| layout(&panels(history, Page::Gpu(Some(gpu))));
        let page = shown(&history, &first);
        assert_eq!(
            page[0],
            (
                "GPU utilization and memory busy".to_owned(),
                strings(&["util", "busy"])
            )
        );
        assert_eq!(
            page[2],
            ("GPU temperature".to_owned(), strings(&["GPU 0"])),
            "a fan that never turned has no strip"
        );
        history
            .charts
            .iter_mut()
            .find(|chart| chart.kind == HistoryChartKind::GpuFanSpeed)
            .unwrap()
            .peak = Some(55.0);
        let page = shown(&history, &first);
        assert_eq!(
            page[2],
            (
                "GPU temperature and fan".to_owned(),
                strings(&["temp", "fan"])
            )
        );
        let panels = panels(&history, Page::Gpu(Some(&first)));
        assert_eq!(panels.len(), 4, "the companions add no panel");
        assert_eq!(panels[2].fold, Some("history.gpu_thermal"));
        let scales: Vec<Option<f64>> = panels[2].strips.iter().map(|strip| strip.scale).collect();
        assert_eq!(scales, [Some(100.0), Some(100.0)]);
        // Their labels take the GPU's color (D-123).
        assert!(
            panels[2]
                .strips
                .iter()
                .all(|strip| strip.gpu.as_deref() == Some("GPU 0"))
        );
        // The other GPU reports neither: its panels are the plain ones.
        assert_eq!(
            shown(&history, &second)[0],
            ("GPU utilization".to_owned(), strings(&["GPU 1"]))
        );
        // Both count as that GPU's series.
        assert_eq!(history_gpus(&history).len(), 2);
    }

    /// The machine's power panel (D-106, D-148): each GPU's draw until the
    /// CPU's power is read, and its title says so (a lone GPU's strip has
    /// no label, and its draw is not the machine's); then the host's total
    /// and the CPU, each scaled to its peak and with its energy, the GPUs'
    /// draws being the total's parts and staying on the GPU History.
    #[test]
    fn the_machines_power_is_its_total_once_read() {
        let titled = |history: &HistoryView| -> String {
            let panels = panels(history, Page::Overview);
            let panel = panels
                .iter()
                .find(|panel| panel.fold == Some("history.host_power"));
            panel.unwrap().title.clone()
        };
        let power_panel = |history: &HistoryView| -> Vec<(String, Option<String>)> {
            let panels = panels(history, Page::Overview);
            let panel = panels
                .iter()
                .find(|panel| panel.fold == Some("history.host_power"))
                .unwrap();
            panel
                .strips
                .iter()
                .map(|strip| (strip.label.clone(), strip.gpu.clone()))
                .collect()
        };
        let mut history = history_view();
        // Only a GPU's strip takes a GPU's color (D-123).
        let gpu = |label: &str| (label.to_owned(), Some(label.to_owned()));
        assert_eq!(power_panel(&history), [gpu("GPU 0"), gpu("GPU 1")]);
        assert_eq!(titled(&history), "Power · GPUs");
        // GB10: one GPU, whose draw is the only power read.
        let mut one_gpu = history.clone();
        let second = history_gpus(&history)[1].entity_id.clone();
        one_gpu
            .charts
            .retain(|chart| chart.series.entity_id != second);
        assert_eq!(power_panel(&one_gpu), [gpu("GPU 0")]);
        assert_eq!(titled(&one_gpu), "Power · GPU");
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
        let host = |label: &str| (label.to_owned(), None);
        assert_eq!(power_panel(&history), [host("total"), host("CPU")]);
        assert_eq!(titled(&history), "Power");
        let panels = panels(&history, Page::Overview);
        let panel = panels.iter().find(|panel| panel.title == "Power").unwrap();
        assert_eq!(panel.fold, Some("history.host_power"));
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
            power_panel(&history),
            [host("total"), host("CPU 0"), host("CPU 1")]
        );
        // Each GPU's own draw is on its page either way.
        let page = super::panels(&history, Page::Gpu(None));
        assert_eq!(page[3].title, "GPU power");
        assert!(page[3].strips[0].series.energy.is_some());
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
        for page in [Page::Gpu(None), Page::Overview] {
            let panels = panels(&history, page);
            let strip = panels
                .iter()
                .find(|panel| panel.title.contains("ower"))
                .and_then(|panel| panel.strips.first())
                .unwrap();
            assert_eq!(strip.scale, Some(68.2), "{page:?}");
        }
    }

    /// Where the kernel reports the memory's stall pressure the memory
    /// panel carries it under the RAM in use (D-135): `some` and `full`,
    /// each scaled to its peak, 1 % at least, and colored by the time lost,
    /// not by its share of that scale. It stays one panel under one fold.
    #[test]
    fn the_memory_panel_adds_the_stall_pressure_strips() {
        let mut history = history_view();
        let template = history.charts[1].clone();
        let pressure =
            |kind, peak: f64| chart_of(&template, kind, "", CanonicalUnit::Percent, (None, peak));
        history
            .charts
            .push(pressure(HistoryChartKind::MemoryPressureSome, 2.4));
        history
            .charts
            .push(pressure(HistoryChartKind::MemoryPressureFull, 0.02));
        fn machine(history: &HistoryView) -> Vec<Panel<'_>> {
            panels(history, Page::Overview)
        }
        let panels = machine(&history);
        let memory = &panels[1];
        assert_eq!(memory.title, "Memory used and pressure · 64.0 GiB");
        assert_eq!(memory.fold, Some("history.memory"));
        let strips: Vec<(&str, Paint, Option<f64>)> = memory
            .strips
            .iter()
            .map(|strip| (strip.label.as_str(), strip.paint, strip.scale))
            .collect();
        assert_eq!(
            strips,
            vec![
                ("used", Paint::Levels, Some(64.0 * 1_073_741_824.0)),
                ("some", Paint::Pressure, Some(2.4)),
                ("full", Paint::Pressure, Some(1.0)),
            ]
        );
        assert!(!memory.is_rate());
        // A key of their own folds the two away (D-144): what is left is
        // the panel of a kernel without PSI, which still names that key.
        let folded =
            |ids: &[&str]| -> BTreeSet<String> { ids.iter().map(|id| (*id).to_owned()).collect() };
        let alone = machine(&history)
            .swap_remove(1)
            .with_strips_folded(&folded(&[MEMORY_PRESSURE_STRIPS]));
        assert_eq!(alone.title, "Memory used · 64.0 GiB");
        let labels: Vec<&str> = alone
            .strips
            .iter()
            .map(|strip| strip.label.as_str())
            .collect();
        assert_eq!(labels, vec![""], "one strip needs no label");
        assert_eq!(alone.fold, Some("history.memory"));
        assert_eq!(
            alone.strips_fold.as_ref().map(|fold| fold.id),
            Some(MEMORY_PRESSURE_STRIPS)
        );
        // Another panel's fold leaves it whole, and no other panel of
        // either page has strips to fold.
        let whole = machine(&history)
            .swap_remove(1)
            .with_strips_folded(&folded(&["history.cpu", "history.memory"]));
        assert_eq!(whole.strips.len(), 3);
        let with_strips_fold = |page| {
            super::panels(&history, page)
                .iter()
                .filter(|panel| panel.strips_fold.is_some())
                .count()
        };
        assert_eq!(with_strips_fold(Page::Overview), 1);
        assert_eq!(with_strips_fold(Page::Gpu(None)), 0);
        // Only `some` reported: its strip alone follows the RAM in use.
        history
            .charts
            .retain(|chart| chart.kind != HistoryChartKind::MemoryPressureFull);
        assert_eq!(machine(&history)[1].strips.len(), 2);
        // A kernel without PSI has nothing to fold.
        history
            .charts
            .retain(|chart| chart.kind != HistoryChartKind::MemoryPressureSome);
        assert!(machine(&history)[1].strips_fold.is_none());
    }

    /// The whole machine's temperatures are one panel of the Overview
    /// History (D-137, D-148): `CPU` (or each package by its number), the
    /// `SoC`, each GPU in its color and the charted drive, all against
    /// 100 °C. A host without a GPU keeps the panel.
    #[test]
    fn the_machines_temperatures_are_one_panel() {
        let mut history = history_view();
        let template = history.charts[1].clone();
        let celsius = |kind, subject: &str, peak: f64| {
            chart_of(
                &template,
                kind,
                subject,
                CanonicalUnit::DegC,
                (Some(100.0), peak),
            )
        };
        history.charts.extend([
            celsius(HistoryChartKind::DiskTemperature, "nvme0n1", 52.0),
            celsius(HistoryChartKind::CpuTemperature, "CPU 0", 88.0),
            celsius(HistoryChartKind::SocTemperature, "", 93.0),
        ]);
        let temperature = |history: &HistoryView| -> Vec<String> {
            let panels = panels(history, Page::Overview);
            let panel = panels
                .iter()
                .find(|panel| panel.title == "Temperature")
                .unwrap();
            panel
                .strips
                .iter()
                .map(|strip| strip.label.clone())
                .collect()
        };
        assert_eq!(
            temperature(&history),
            ["CPU", "SoC", "GPU 0", "GPU 1", "disk"]
        );
        let panels = panels(&history, Page::Overview);
        let panel = panels
            .iter()
            .find(|panel| panel.title == "Temperature")
            .unwrap();
        assert_eq!(panel.fold, Some("history.host_temperature"));
        assert!(panel.strips.iter().all(|strip| strip.scale == Some(100.0)));
        let colored: Vec<Option<&str>> = panel
            .strips
            .iter()
            .map(|strip| strip.gpu.as_deref())
            .collect();
        assert_eq!(colored, [None, None, Some("GPU 0"), Some("GPU 1"), None]);
        // Several packages are named apart.
        history
            .charts
            .push(celsius(HistoryChartKind::CpuTemperature, "CPU 1", 80.0));
        assert_eq!(temperature(&history)[..3], ["CPU 0", "CPU 1", "SoC"]);
        // Without a GPU the host's own temperatures keep the panel.
        history.charts.retain(|chart| !is_gpu(chart.kind));
        assert_eq!(temperature(&history), ["CPU 0", "CPU 1", "SoC", "disk"]);
    }
}
