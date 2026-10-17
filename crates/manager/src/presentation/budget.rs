//! Keeps a projection within `runtime.max_projection_bytes`. When the first
//! measurement is over budget, detail is shed in order of least information
//! loss, and a banner always says so (never silent).

use std::sync::Arc;

use dgxtop_core::view::{BannerLevel, ModuleViews, Trend, ViewModel};

use super::banners::{order_and_bound, push};
use super::history::downsample;

/// Facts from the builders that trimming must preserve.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BudgetHints {
    /// Rows the viewer shows, the table standing alone (`z`, D-121); the
    /// selected process is always among them.
    pub(crate) visible_process_rows: usize,
    /// Rows the table shows beside the Overview's other panels.
    pub(crate) beside_process_rows: usize,
}

/// Room kept for the reduction banner itself.
const NOTICE_RESERVE_BYTES: usize = 1024;
const MIN_TREND_POINTS: usize = 8;
const MIN_HISTORY_POINTS: usize = 16;
const REDUCED_LIST_ROWS: usize = 4;
const REDUCED_LABELED_ROWS: usize = 8;
const REDUCED_CORES: usize = 64;
const MINIMAL_CORES: usize = 16;

type TrimStep = fn(&mut ModuleViews, &BudgetHints) -> bool;

/// Ordered from least to most information loss: the process rows first,
/// which only the table standing alone shows (D-121).
const TRIM_STEPS: [TrimStep; 12] = [
    drop_process_overscan,
    narrow_process_window,
    halve_trends,
    halve_trends,
    halve_history,
    halve_run_charts,
    shorten_lists,
    |modules, _| limit_cores(modules, REDUCED_CORES),
    clear_trends,
    drop_secondary_modules,
    |modules, _| limit_cores(modules, MINIMAL_CORES),
    strip_gpu_details,
];

/// Returns the size the projection is accounted at ([`ViewModel::accounted_bytes`]):
/// it fits `max_bytes` exactly when the JSON does, and the JSON is written
/// only when the bound does not fit.
pub(crate) fn fit_projection(
    model: &mut ViewModel,
    max_bytes: usize,
    hints: &BudgetHints,
) -> usize {
    let size = model.accounted_bytes(max_bytes);
    if size <= max_bytes {
        return size;
    }
    let target = max_bytes.saturating_sub(NOTICE_RESERVE_BYTES);
    for step in TRIM_STEPS {
        if step(&mut model.modules, hints) && model.accounted_bytes(target) <= target {
            break;
        }
    }
    let kib = max_bytes / 1024;
    let (level, text) = if model.accounted_bytes(target) <= target {
        (
            BannerLevel::Info,
            format!("view reduced to fit runtime.max_projection_bytes ({kib} KiB)"),
        )
    } else {
        (
            BannerLevel::Warning,
            format!("view exceeds runtime.max_projection_bytes ({kib} KiB) after reduction"),
        )
    };
    let mut banners = std::mem::take(&mut model.banners);
    push(&mut banners, level, text);
    model.banners = order_and_bound(banners);
    model.accounted_bytes(max_bytes)
}

/// Keeps the most recent half of a trend (same step, shorter window).
fn halve(trend: &mut Trend) -> bool {
    let length = trend.points.len();
    if length <= MIN_TREND_POINTS {
        return false;
    }
    trend.points.drain(..length - length / 2);
    true
}

fn clear(trend: &mut Trend) -> bool {
    let had_points = !trend.points.is_empty();
    trend.points.clear();
    had_points
}

fn for_each_trend(modules: &mut ModuleViews, mut apply: impl FnMut(&mut Trend) -> bool) -> bool {
    let mut changed = false;
    if let Some(cpu) = modules.cpu.as_mut() {
        changed |= apply(&mut Arc::make_mut(cpu).utilization_trend);
    }
    if let Some(memory) = modules.memory.as_mut() {
        let memory = Arc::make_mut(memory);
        changed |= apply(&mut memory.used_trend);
        changed |= apply(&mut memory.gpu_resident_trend);
        for part in &mut memory.gpu_process_trends {
            changed |= apply(&mut part.trend);
        }
    }
    if let Some(gpu) = modules.gpu.as_mut() {
        for card in &mut Arc::make_mut(gpu).cards {
            changed |= apply(&mut card.utilization_trend);
            changed |= apply(&mut card.temperature_trend);
            changed |= apply(&mut card.power_trend);
            changed |= apply(&mut card.memory_trend);
        }
    }
    if let Some(disk) = modules.disk.as_mut() {
        let disk = Arc::make_mut(disk);
        changed |= apply(&mut disk.read_trend);
        changed |= apply(&mut disk.write_trend);
    }
    if let Some(network) = modules.network.as_mut() {
        let network = Arc::make_mut(network);
        changed |= apply(&mut network.rx_trend);
        changed |= apply(&mut network.tx_trend);
    }
    changed
}

fn halve_trends(modules: &mut ModuleViews, _: &BudgetHints) -> bool {
    for_each_trend(modules, halve)
}

fn clear_trends(modules: &mut ModuleViews, _: &BudgetHints) -> bool {
    for_each_trend(modules, clear)
}

fn truncate<T>(items: &mut Vec<T>, keep: usize) -> bool {
    let longer = items.len() > keep;
    items.truncate(keep);
    longer
}

/// The window centers the selection inside the visible rows, so dropping
/// the overscan never drops the selected process.
fn drop_process_overscan(modules: &mut ModuleViews, hints: &BudgetHints) -> bool {
    let Some(processes) = modules.processes.as_mut() else {
        return false;
    };
    truncate(
        &mut Arc::make_mut(processes).rows,
        hints.visible_process_rows.max(1),
    )
}

/// Narrows the window to the rows the table shows beside the Overview's
/// other panels, around the selected process: only the table standing
/// alone (`z`, D-121) shows fewer.
fn narrow_process_window(modules: &mut ModuleViews, hints: &BudgetHints) -> bool {
    let Some(processes) = modules.processes.as_mut() else {
        return false;
    };
    let keep = hints.beside_process_rows.max(1);
    if processes.rows.len() <= keep {
        return false;
    }
    let processes = Arc::make_mut(processes);
    let selected = processes
        .selected_index
        .and_then(|index| index.checked_sub(processes.window_start))
        .map(|offset| usize::try_from(offset).unwrap_or(usize::MAX));
    let start = selected
        .map_or(0, |at| at.saturating_sub(keep / 2))
        .min(processes.rows.len() - keep);
    processes.rows.drain(..start);
    processes.rows.truncate(keep);
    processes.window_start = processes
        .window_start
        .saturating_add(u32::try_from(start).unwrap_or(u32::MAX));
    true
}

/// Halves the points of every history chart above the floor.
fn halve_history(modules: &mut ModuleViews, _: &BudgetHints) -> bool {
    let Some(history) = modules.history.as_mut() else {
        return false;
    };
    if history
        .charts
        .iter()
        .all(|chart| chart.points.len() <= MIN_HISTORY_POINTS)
    {
        return false;
    }
    let view = Arc::make_mut(history);
    let mut changed = false;
    for chart in &mut view.charts {
        let length = chart.points.len();
        if length > MIN_HISTORY_POINTS {
            chart.points = downsample(&chart.points, length / 2);
            chart.display_downsampled = true;
            changed |= chart.points.len() < length;
        }
        for part in &mut chart.parts {
            if part.means.len() > MIN_HISTORY_POINTS {
                part.means = halve_cells(&part.means);
                changed = true;
            }
        }
    }
    changed
}

/// Halves the points of the opened run's charts above the floor: a run on
/// four GPUs draws 26 (D-120).
fn halve_run_charts(modules: &mut ModuleViews, _: &BudgetHints) -> bool {
    let Some(history) = modules.process_history.as_mut() else {
        return false;
    };
    let long = |opened: &dgxtop_core::view::RunDetailView| {
        opened
            .charts
            .iter()
            .any(|chart| chart.points.len() > MIN_HISTORY_POINTS)
    };
    if !history.opened.as_ref().is_some_and(long) {
        return false;
    }
    let Some(opened) = Arc::make_mut(history).opened.as_mut() else {
        return false;
    };
    let mut changed = false;
    for chart in &mut opened.charts {
        let length = chart.points.len();
        if length > MIN_HISTORY_POINTS {
            chart.points = downsample(&chart.points, length / 2);
            changed |= chart.points.len() < length;
        }
    }
    changed
}

/// Merges each pair of a part's cells: the mean of what both recorded.
fn halve_cells(means: &[Option<f64>]) -> Vec<Option<f64>> {
    means
        .chunks(2)
        .map(|pair| {
            let known: Vec<f64> = pair.iter().flatten().copied().collect();
            #[allow(clippy::cast_precision_loss)]
            let count = known.len() as f64;
            (!known.is_empty()).then(|| known.iter().sum::<f64>() / count)
        })
        .collect()
}

fn shorten_lists(modules: &mut ModuleViews, _: &BudgetHints) -> bool {
    let mut changed = false;
    if let Some(disk) = modules.disk.as_mut() {
        let view = Arc::make_mut(disk);
        changed |= truncate(&mut view.devices, REDUCED_LIST_ROWS);
        changed |= truncate(&mut view.filesystems, REDUCED_LIST_ROWS);
    }
    if let Some(network) = modules.network.as_mut() {
        changed |= truncate(&mut Arc::make_mut(network).interfaces, REDUCED_LIST_ROWS);
    }
    if let Some(cpu) = modules.cpu.as_mut() {
        let view = Arc::make_mut(cpu);
        changed |= truncate(&mut view.package_temperatures, REDUCED_LABELED_ROWS);
        changed |= truncate(&mut view.soc_temperatures, REDUCED_LABELED_ROWS);
        changed |= truncate(&mut view.power, REDUCED_LABELED_ROWS);
    }
    changed
}

fn limit_cores(modules: &mut ModuleViews, keep: usize) -> bool {
    modules
        .cpu
        .as_mut()
        .is_some_and(|cpu| truncate(&mut Arc::make_mut(cpu).cores, keep))
}

fn drop_secondary_modules(modules: &mut ModuleViews, _: &BudgetHints) -> bool {
    let dropped = modules.disk.is_some() || modules.network.is_some();
    modules.disk = None;
    modules.network = None;
    dropped
}

/// Clock lists and metadata go last; notes stay because they explain
/// missing values.
fn strip_gpu_details(modules: &mut ModuleViews, _: &BudgetHints) -> bool {
    let Some(gpu) = modules.gpu.as_mut() else {
        return false;
    };
    let mut changed = false;
    for card in &mut Arc::make_mut(gpu).cards {
        changed |= truncate(&mut card.clocks, 0);
        changed |= truncate(&mut card.metadata, 0);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::metric::CanonicalUnit;
    use dgxtop_core::view::{CellState, MemoryView, MetricCell, ProcessTrend};

    fn trend(points: usize) -> Trend {
        Trend {
            points: (0..points)
                .map(|index| Some(f64::from(u32::try_from(index).unwrap())))
                .collect(),
        }
    }

    #[test]
    fn halving_keeps_the_most_recent_points() {
        let mut long = trend(100);
        assert!(halve(&mut long));
        assert_eq!(long.points.len(), 50);
        assert_eq!(long.points.last().copied().flatten(), Some(99.0));
        let mut short = trend(MIN_TREND_POINTS);
        assert!(!halve(&mut short));
        assert!(clear(&mut short));
        assert!(!clear(&mut short));
    }

    /// A part's cells merge in pairs, as the charts' points do; a pair
    /// without a recorded cell stays unknown.
    #[test]
    fn halving_a_part_merges_its_cells_in_pairs() {
        let means = [Some(2.0), Some(4.0), None, None, Some(6.0)];
        assert_eq!(halve_cells(&means), vec![Some(3.0), None, Some(6.0)]);
        assert_eq!(halve_cells(&[Some(1.0), None]), vec![Some(1.0)]);
    }

    /// The opened run's charts halve above the floor (D-120), once they are
    /// long.
    #[test]
    fn an_opened_runs_charts_halve() {
        use dgxtop_core::metric::CanonicalUnit;
        use dgxtop_core::process_history::{RunDetails, RunInfo, RunMetric};
        use dgxtop_core::tiers::BarSize;
        use dgxtop_core::view::{
            HistoryPointView, ProcessHistoryView, RunChartView, RunDetailView,
        };
        let point = |minute: u64| HistoryPointView {
            start_utc_ns: minute * 60_000_000_000,
            end_utc_ns: (minute + 1) * 60_000_000_000,
            mean: Some(1.0),
            min: None,
            max: Some(1.0),
        };
        let charts = RunMetric::ALL
            .iter()
            .map(|metric| RunChartView {
                metric: *metric,
                unit: CanonicalUnit::Percent,
                scale_max: None,
                points: (0..100).map(point).collect(),
                mean: None,
                peak: None,
                latest: None,
            })
            .collect();
        let mut modules = ModuleViews {
            process_history: Some(Arc::new(ProcessHistoryView {
                runs: Vec::new(),
                more_runs: 0,
                opened: Some(RunDetailView {
                    info: RunInfo {
                        key: dgxtop_core::testing::test_process_key(1),
                        name: "train".to_owned(),
                        command: String::new(),
                        cwd: String::new(),
                        uid: None,
                        gpus: Vec::new(),
                        details: RunDetails::default(),
                        started_utc_ns: 0,
                        last_seen_utc_ns: 0,
                        end: None,
                    },
                    live: true,
                    slot: None,
                    bar: BarSize::OneMinute,
                    window_start_utc_ns: 0,
                    window_end_utc_ns: 100 * 60_000_000_000,
                    charts,
                    loading: false,
                }),
                kept_on_disk: true,
                utc_offset_s: 0,
                bar: BarSize::Auto,
                cpu_scale: None,
            })),
            ..ModuleViews::default()
        };
        let hints = BudgetHints {
            visible_process_rows: 10,
            beside_process_rows: 4,
        };
        let lengths = |modules: &ModuleViews| -> Vec<usize> {
            let opened = modules
                .process_history
                .as_ref()
                .unwrap()
                .opened
                .as_ref()
                .unwrap();
            opened
                .charts
                .iter()
                .map(|chart| chart.points.len())
                .collect()
        };
        assert!(halve_run_charts(&mut modules, &hints));
        assert!(lengths(&modules).iter().all(|length| *length == 50));
        assert!(halve_run_charts(&mut modules, &hints));
        assert!(halve_run_charts(&mut modules, &hints));
        assert!(
            lengths(&modules)
                .iter()
                .all(|length| *length <= MIN_HISTORY_POINTS)
        );
        assert!(!halve_run_charts(&mut modules, &hints), "at the floor");
    }

    /// Narrowing keeps the rows the table shows beside the Overview's other
    /// panels around the selected process, and where they start (D-121).
    #[test]
    fn a_narrowed_process_window_keeps_the_selection() {
        use dgxtop_core::commands::SortKey;
        use dgxtop_core::testing::test_process_key;
        use dgxtop_core::view::{ProcessRowView, ProcessesView};
        let row = |pid: i64| ProcessRowView {
            key: test_process_key(pid),
            name: format!("p{pid}"),
            user: "u".to_owned(),
            state: None,
            cpu_percent: None,
            rss_bytes: None,
            missing_reason: None,
            gpus: Vec::new(),
            color_slot: None,
        };
        let mut modules = ModuleViews {
            processes: Some(Arc::new(ProcessesView {
                rows: (100..140).map(row).collect(),
                window_start: 100,
                total_rows: 300,
                truncated: false,
                sort: SortKey::Pid,
                ascending: true,
                filter: String::new(),
                gpu_filter: None,
                gpu_choices: Vec::new(),
                selected: Some(test_process_key(130)),
                selected_index: Some(130),
                selected_missing: None,
            })),
            ..ModuleViews::default()
        };
        let hints = BudgetHints {
            visible_process_rows: 40,
            beside_process_rows: 6,
        };
        assert!(narrow_process_window(&mut modules, &hints));
        let processes = modules.processes.as_ref().unwrap();
        assert_eq!(processes.window_start, 127);
        let pids: Vec<i64> = processes
            .rows
            .iter()
            .map(|row| i64::from(row.key.pid.get()))
            .collect();
        assert_eq!(pids, vec![127, 128, 129, 130, 131, 132]);
        assert!(!narrow_process_window(&mut modules, &hints), "once");
    }

    #[test]
    fn steps_report_whether_they_changed_anything() {
        let mut modules = ModuleViews::default();
        let hints = BudgetHints {
            visible_process_rows: 10,
            beside_process_rows: 4,
        };
        for step in TRIM_STEPS {
            assert!(!step(&mut modules, &hints));
        }
        let missing = MetricCell::missing(CanonicalUnit::Bytes, CellState::Unavailable);
        modules.memory = Some(Arc::new(MemoryView {
            total: missing.clone(),
            available: missing.clone(),
            used: missing.clone(),
            buffers: missing.clone(),
            cached: missing.clone(),
            swap_total: missing.clone(),
            swap_used: missing,
            gpu_resident: None,
            used_trend: trend(64),
            gpu_resident_trend: trend(64),
            gpu_processes: Vec::new(),
            gpu_process_trends: vec![ProcessTrend {
                slot: 0,
                trend: trend(64),
            }],
        }));
        assert!(halve_trends(&mut modules, &hints));
        let memory = modules.memory.as_ref().unwrap();
        assert_eq!(memory.used_trend.points.len(), 32);
        assert_eq!(memory.gpu_resident_trend.points.len(), 32);
        assert_eq!(memory.gpu_process_trends[0].trend.points.len(), 32);
        assert!(clear_trends(&mut modules, &hints));
    }
}
