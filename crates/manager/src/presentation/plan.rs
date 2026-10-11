//! Which module views a projection fills (only visible, enabled topics) and
//! the viewport-derived sizes used to bound them.

use dgxtop_core::commands::ViewKind;
use dgxtop_core::view::history_plot_columns;

use crate::history::MIN_DISPLAY_POINTS;

pub(crate) const CPU: &str = "cpu";
pub(crate) const GPU: &str = "gpu";
pub(crate) const MEMORY: &str = "memory";
pub(crate) const DISK: &str = "disk";
pub(crate) const NETWORK: &str = "network";
pub(crate) const PROCESSES: &str = "processes";
pub(crate) const HISTORY: &str = "history";
pub(crate) const SOURCES: &str = "sources";
pub(crate) const SETTINGS: &str = "settings";
pub(crate) const ACTIONS: &str = "actions";

/// Viewport assumed until the session reports its terminal size.
const DEFAULT_VIEWPORT: (u16, u16) = (80, 24);
/// Upper bound of copied trend points (TrendBook capacity bounds it too).
const MAX_TREND_POINTS: usize = 512;
const MIN_TREND_POINTS: usize = 16;
/// Trend points the overview GPUs share: eight GPUs still get 256 each.
const GPU_OVERVIEW_POINT_BUDGET: usize = 2048;

/// Module ids (from `config::BUILTIN_MODULES`) shown by each view.
pub fn visible_modules(view: ViewKind) -> &'static [&'static str] {
    match view {
        ViewKind::Overview => &[CPU, GPU, MEMORY, DISK, NETWORK, PROCESSES],
        ViewKind::GpuDetail => &[GPU],
        ViewKind::Processes => &[PROCESSES],
        ViewKind::History => &[HISTORY],
        ViewKind::Sources => &[SOURCES],
        ViewKind::Settings => &[SETTINGS],
        ViewKind::Actions => &[ACTIONS],
    }
}

/// Visible modules split into those to fill and those disabled by config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ViewPlan {
    shown: Vec<&'static str>,
    disabled: Vec<&'static str>,
}

impl ViewPlan {
    /// A pending confirmation always carries the actions view: its UID,
    /// start identity and warning are safety information (UI.md §2, §6).
    pub(crate) fn new(view: ViewKind, enabled: &[String], confirmation_pending: bool) -> Self {
        let (mut shown, mut disabled): (Vec<&'static str>, Vec<&'static str>) =
            visible_modules(view)
                .iter()
                .partition(|module| enabled.iter().any(|entry| entry == *module));
        if confirmation_pending {
            disabled.retain(|module| *module != ACTIONS);
            if !shown.contains(&ACTIONS) {
                shown.push(ACTIONS);
            }
        }
        Self { shown, disabled }
    }

    pub(crate) fn shows(&self, module: &str) -> bool {
        self.shown.contains(&module)
    }

    pub(crate) fn disabled(&self) -> &[&'static str] {
        &self.disabled
    }
}

/// Sizes derived from the session's terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ViewportLayout {
    pub(crate) columns: u16,
    pub(crate) rows: u16,
}

impl ViewportLayout {
    pub(crate) fn new(viewport: (u16, u16)) -> Self {
        let (columns, rows) = if viewport.0 == 0 || viewport.1 == 0 {
            DEFAULT_VIEWPORT
        } else {
            viewport
        };
        Self { columns, rows }
    }

    /// Full-width charts (CPU trend, selected GPU, history).
    pub(crate) fn chart_points(self) -> usize {
        usize::from(self.columns).clamp(MIN_TREND_POINTS, MAX_TREND_POINTS)
    }

    /// History charts: the points the HistoryService queries for a chart's
    /// plot (D-50), so the projection shares its result instead of
    /// downsampling it every frame.
    pub(crate) fn history_chart_points(self) -> usize {
        let queried = u32::from(history_plot_columns(self.columns)).max(MIN_DISPLAY_POINTS);
        usize::try_from(queried)
            .unwrap_or(MAX_TREND_POINTS)
            .clamp(MIN_TREND_POINTS, MAX_TREND_POINTS)
    }

    /// Bars of a Process History chart (D-77): one a column of its plot,
    /// which is laid out like a history chart's.
    pub(crate) fn run_chart_bars(self) -> usize {
        usize::from(history_plot_columns(self.columns)).max(1)
    }

    /// Half-width charts (disk and network rates).
    pub(crate) fn half_chart_points(self) -> usize {
        (usize::from(self.columns) / 2).clamp(MIN_TREND_POINTS, MAX_TREND_POINTS)
    }

    /// Utilization chart of one of `gpu_count` overview GPUs: the width of
    /// the full-width panel, within the share of a point budget the GPUs
    /// split (D-43).
    pub(crate) fn gpu_summary_points(self, gpu_count: usize) -> usize {
        let share = GPU_OVERVIEW_POINT_BUDGET / gpu_count.max(1);
        usize::from(self.columns)
            .min(share)
            .clamp(MIN_TREND_POINTS, MAX_TREND_POINTS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::config::BUILTIN_MODULES;

    fn all_enabled() -> Vec<String> {
        BUILTIN_MODULES
            .iter()
            .map(|module| (*module).to_owned())
            .collect()
    }

    #[test]
    fn every_view_shows_registered_modules_only() {
        for view in ViewKind::ALL {
            for module in visible_modules(view) {
                assert!(BUILTIN_MODULES.contains(module), "{module}");
            }
        }
        assert_eq!(visible_modules(ViewKind::Overview).len(), 6);
    }

    #[test]
    fn pending_confirmation_always_carries_actions() {
        let plan = ViewPlan::new(ViewKind::Processes, &all_enabled(), true);
        assert!(plan.shows("processes"));
        assert!(plan.shows("actions"));
        let disabled = ViewPlan::new(ViewKind::Actions, &[], true);
        assert!(disabled.shows("actions"));
        assert!(disabled.disabled().is_empty());
    }

    #[test]
    fn layout_defaults_and_chart_widths() {
        let unknown = ViewportLayout::new((0, 0));
        assert_eq!((unknown.columns, unknown.rows), (80, 24));
        let wide = ViewportLayout::new((160, 50));
        assert_eq!(wide.chart_points(), 160);
        assert_eq!(wide.half_chart_points(), 80);
        // Two history panels of 77 side by side, each a 53-column plot: the
        // charts keep the 60 points the HistoryService queries at least, so
        // the Viewer merges them and no projection downsamples (D-50).
        assert_eq!(wide.history_chart_points(), 60);
        assert_eq!(ViewportLayout::new((213, 41)).history_chart_points(), 80);
        // A GPU chart spans the full-width panel (D-43); the GPUs share a
        // point budget, so many GPUs do not multiply the projection.
        assert_eq!(wide.gpu_summary_points(2), 160);
        assert_eq!(ViewportLayout::new((213, 41)).gpu_summary_points(2), 213);
        assert_eq!(ViewportLayout::new((300, 50)).gpu_summary_points(8), 256);
        assert_eq!(ViewportLayout::new((300, 50)).gpu_summary_points(64), 32);
        assert_eq!(
            ViewportLayout::new((u16::MAX, 10)).gpu_summary_points(1),
            512
        );
        assert_eq!(ViewportLayout::new((20, 5)).gpu_summary_points(0), 20);
        assert_eq!(ViewportLayout::new((10, 5)).gpu_summary_points(1), 16);
        assert_eq!(ViewportLayout::new((20, 5)).chart_points(), 20);
        assert_eq!(ViewportLayout::new((10, 5)).chart_points(), 16);
        assert_eq!(ViewportLayout::new((u16::MAX, 10)).chart_points(), 512);
    }
}
