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
    /// Rows the viewer shows; the selected process is always among them.
    pub(crate) visible_process_rows: usize,
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

/// Ordered from least to most information loss.
const TRIM_STEPS: [TrimStep; 11] = [
    halve_trends,
    halve_trends,
    drop_process_overscan,
    halve_history,
    halve_sources,
    shorten_lists,
    |modules, _| limit_cores(modules, REDUCED_CORES),
    clear_trends,
    drop_secondary_modules,
    |modules, _| limit_cores(modules, MINIMAL_CORES),
    strip_gpu_details,
];

/// Returns the final estimated size of the projection.
pub(crate) fn fit_projection(
    model: &mut ViewModel,
    max_bytes: usize,
    hints: &BudgetHints,
) -> usize {
    let size = model.estimated_bytes();
    if size <= max_bytes {
        return size;
    }
    let target = max_bytes.saturating_sub(NOTICE_RESERVE_BYTES);
    for step in TRIM_STEPS {
        if step(&mut model.modules, hints) && model.estimated_bytes() <= target {
            break;
        }
    }
    let kib = max_bytes / 1024;
    let (level, text) = if model.estimated_bytes() <= target {
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
    model.estimated_bytes()
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

/// Halves the metric window around the selected series and the
/// unclassified list.
fn halve_sources(modules: &mut ModuleViews, _: &BudgetHints) -> bool {
    let Some(sources) = modules.sources.as_mut() else {
        return false;
    };
    let view = Arc::make_mut(sources);
    let before = view.metrics.len() + view.unclassified.len();
    let length = view.metrics.len();
    if length > 1 {
        let center = view
            .selected
            .as_ref()
            .and_then(|selected| {
                view.metrics
                    .iter()
                    .position(|metric| &metric.key.series_selector() == selected)
            })
            .unwrap_or(0);
        let keep = (length / 2).max(1);
        let start = center.saturating_sub(keep / 2).min(length - keep);
        view.metrics = view.metrics.drain(start..start + keep).collect();
    }
    let unclassified = view.unclassified.len() / 2;
    view.unclassified.truncate(unclassified);
    view.metrics.len() + view.unclassified.len() < before
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
            step_ms: 1000,
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

    #[test]
    fn steps_report_whether_they_changed_anything() {
        let mut modules = ModuleViews::default();
        let hints = BudgetHints {
            visible_process_rows: 10,
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
