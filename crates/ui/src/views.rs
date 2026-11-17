//! Views the interface offers, as tabs in order (D-36, D-75, D-91). Process
//! work (select, sort, filter, terminate) happens in the overview's process
//! table; a pending confirmation is a banner in every view and each
//! signal's outcome a notice. The Processes, Sources and Actions view kinds
//! stay in the command contract but are removed: no tab shows them and the
//! Manager refuses them (D-91).
//!
//! The view kinds are fixed by the command contract, so the three history
//! tabs are one view, `History`, on pages of its module: the whole
//! machine's resources, one GPU's (D-148) and the GPU processes (D-75).

use dgxtop_core::commands::ViewKind;

/// A tab of the interface: a view, and for the history view its page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    Overview,
    Gpu,
    OverviewHistory,
    GpuHistory,
    ProcessHistory,
    Settings,
}

/// Offered tabs in order; the digit keys 1–6 follow this order.
pub const OFFERED_TABS: [Tab; 6] = [
    Tab::Overview,
    Tab::Gpu,
    Tab::OverviewHistory,
    Tab::GpuHistory,
    Tab::ProcessHistory,
    Tab::Settings,
];

/// Page of the history module charting the whole machine: the Overview
/// History, the module's first page.
pub const OVERVIEW_HISTORY_PAGE: u8 = 0;
/// Page of the history module the process history opens on (the list).
pub const PROCESS_LIST_PAGE: u8 = 1;
/// Page of the history module showing the opened run.
pub const RUN_PAGE: u8 = 2;
/// Page of the history module charting one GPU: the GPU History (D-148).
pub const GPU_HISTORY_PAGE: u8 = 3;

/// Whether `page` of the history module charts the history's series (the
/// Overview History and the GPU History) rather than GPU process runs.
pub const fn is_chart_page(page: u8) -> bool {
    matches!(page, OVERVIEW_HISTORY_PAGE | GPU_HISTORY_PAGE)
}

impl Tab {
    pub const fn view(self) -> ViewKind {
        match self {
            Self::Overview => ViewKind::Overview,
            Self::Gpu => ViewKind::GpuDetail,
            Self::OverviewHistory | Self::GpuHistory | Self::ProcessHistory => ViewKind::History,
            Self::Settings => ViewKind::Settings,
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Gpu => "GPU",
            Self::OverviewHistory => "Overview History",
            Self::GpuHistory => "GPU History",
            Self::ProcessHistory => "Process History",
            Self::Settings => "Settings",
        }
    }

    /// Control id of the tab's hit target.
    pub const fn control_id(self) -> &'static str {
        match self {
            Self::Overview => "tab.overview",
            Self::Gpu => "tab.gpu_detail",
            Self::OverviewHistory => "tab.history",
            Self::GpuHistory => "tab.gpu_history",
            Self::ProcessHistory => "tab.process_history",
            Self::Settings => "tab.settings",
        }
    }

    /// Inverse of [`Tab::control_id`].
    pub fn for_control(control_id: &str) -> Option<Self> {
        OFFERED_TABS
            .into_iter()
            .find(|tab| tab.control_id() == control_id)
    }

    /// The tab showing `view` on history `page`; `None` for a hidden view.
    pub fn of(view: ViewKind, page: u8) -> Option<Self> {
        match view {
            ViewKind::Overview => Some(Self::Overview),
            ViewKind::GpuDetail => Some(Self::Gpu),
            ViewKind::History if page == OVERVIEW_HISTORY_PAGE => Some(Self::OverviewHistory),
            ViewKind::History if page == GPU_HISTORY_PAGE => Some(Self::GpuHistory),
            ViewKind::History => Some(Self::ProcessHistory),
            ViewKind::Settings => Some(Self::Settings),
            ViewKind::Processes | ViewKind::Sources | ViewKind::Actions => None,
        }
    }

    /// 1-based tab number.
    pub fn number(self) -> usize {
        OFFERED_TABS
            .iter()
            .position(|offered| *offered == self)
            .map_or(1, |index| index + 1)
    }
}

/// The tab after `tab`, wrapping; from a hidden view, the first.
pub fn next_tab(tab: Option<Tab>) -> Tab {
    let next = tab.map_or(0, Tab::number) % OFFERED_TABS.len();
    OFFERED_TABS[next]
}

/// The tab before `tab`, wrapping; from a hidden view, the first.
pub fn previous_tab(tab: Option<Tab>) -> Tab {
    match tab {
        Some(tab) => OFFERED_TABS[(tab.number() + OFFERED_TABS.len() - 2) % OFFERED_TABS.len()],
        None => OFFERED_TABS[0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_retired_views_are_not_offered() {
        assert_eq!(Tab::of(ViewKind::Sources, 0), None);
        assert_eq!(Tab::of(ViewKind::Processes, 0), None);
        assert_eq!(Tab::of(ViewKind::Actions, 0), None);
        assert_eq!(Tab::Overview.number(), 1);
        assert_eq!(Tab::Settings.number(), 6);
    }

    /// The three history tabs are one view on pages of its module (D-75,
    /// D-148): the whole machine, one GPU, then the runs and an opened run.
    #[test]
    fn the_history_tabs_are_pages_of_one_view() {
        assert_eq!(Tab::of(ViewKind::History, 0), Some(Tab::OverviewHistory));
        assert_eq!(Tab::of(ViewKind::History, 1), Some(Tab::ProcessHistory));
        assert_eq!(Tab::of(ViewKind::History, 2), Some(Tab::ProcessHistory));
        assert_eq!(
            Tab::of(ViewKind::History, GPU_HISTORY_PAGE),
            Some(Tab::GpuHistory)
        );
        assert!(is_chart_page(OVERVIEW_HISTORY_PAGE) && is_chart_page(GPU_HISTORY_PAGE));
        assert!(!is_chart_page(PROCESS_LIST_PAGE) && !is_chart_page(RUN_PAGE));
        assert_eq!(Tab::ProcessHistory.view(), ViewKind::History);
        assert_eq!(Tab::GpuHistory.view(), ViewKind::History);
        assert_eq!(next_tab(Some(Tab::OverviewHistory)), Tab::GpuHistory);
        assert_eq!(next_tab(Some(Tab::GpuHistory)), Tab::ProcessHistory);
        assert_eq!(previous_tab(Some(Tab::Overview)), Tab::Settings);
        assert_eq!(next_tab(None), Tab::Overview);
        for tab in OFFERED_TABS {
            assert_eq!(Tab::for_control(tab.control_id()), Some(tab));
        }
    }
}
