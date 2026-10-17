//! Views the interface offers, as tabs in order (D-36, D-75, D-91). Process
//! work (select, sort, filter, terminate) happens in the overview's process
//! table; a pending confirmation is a banner in every view and each
//! signal's outcome a notice. The Processes, Sources and Actions view kinds
//! stay in the command contract but are removed: no tab shows them and the
//! Manager refuses them (D-91).
//!
//! The view kinds are fixed by the command contract, so the two history
//! tabs are one view, `History`, on two pages of its module: the overview's
//! resources and the GPU processes (D-75).

use dgxtop_core::commands::ViewKind;

/// A tab of the interface: a view, and for the history view its page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    Overview,
    Gpu,
    OverviewHistory,
    ProcessHistory,
    Settings,
}

/// Offered tabs in order; the digit keys 1–5 follow this order.
pub const OFFERED_TABS: [Tab; 5] = [
    Tab::Overview,
    Tab::Gpu,
    Tab::OverviewHistory,
    Tab::ProcessHistory,
    Tab::Settings,
];

/// Page of the history module the process history opens on (the list).
pub const PROCESS_LIST_PAGE: u8 = 1;
/// Page of the history module showing the opened run.
pub const RUN_PAGE: u8 = 2;

impl Tab {
    pub const fn view(self) -> ViewKind {
        match self {
            Self::Overview => ViewKind::Overview,
            Self::Gpu => ViewKind::GpuDetail,
            Self::OverviewHistory | Self::ProcessHistory => ViewKind::History,
            Self::Settings => ViewKind::Settings,
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Gpu => "GPU",
            Self::OverviewHistory => "Overview History",
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
            ViewKind::History if page == 0 => Some(Self::OverviewHistory),
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
        assert_eq!(Tab::Settings.number(), 5);
    }

    /// The two history tabs are one view on two pages (D-75).
    #[test]
    fn the_history_tabs_are_pages_of_one_view() {
        assert_eq!(Tab::of(ViewKind::History, 0), Some(Tab::OverviewHistory));
        assert_eq!(Tab::of(ViewKind::History, 1), Some(Tab::ProcessHistory));
        assert_eq!(Tab::of(ViewKind::History, 2), Some(Tab::ProcessHistory));
        assert_eq!(Tab::ProcessHistory.view(), ViewKind::History);
        assert_eq!(next_tab(Some(Tab::OverviewHistory)), Tab::ProcessHistory);
        assert_eq!(previous_tab(Some(Tab::Overview)), Tab::Settings);
        assert_eq!(next_tab(None), Tab::Overview);
        for tab in OFFERED_TABS {
            assert_eq!(Tab::for_control(tab.control_id()), Some(tab));
        }
    }
}
