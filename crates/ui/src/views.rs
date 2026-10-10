//! Views the interface offers, in tab order (D-36, D-44). Process work
//! (select, sort, filter, terminate) happens in the overview's process
//! table; source diagnostics and the action record are not offered (a
//! pending confirmation is a banner in every view and each signal's outcome
//! a notice). The Manager still accepts every view kind; the interface
//! simply never navigates to a hidden one.

use dgxtop_core::commands::ViewKind;

/// Offered views in tab order; the digit keys 1–4 follow this order.
pub const OFFERED_VIEWS: [ViewKind; 4] = [
    ViewKind::Overview,
    ViewKind::GpuDetail,
    ViewKind::History,
    ViewKind::Settings,
];

/// 1-based tab number of `view`, `None` for a hidden view.
pub fn tab_number(view: ViewKind) -> Option<usize> {
    OFFERED_VIEWS
        .iter()
        .position(|offered| *offered == view)
        .map(|index| index + 1)
}

/// The offered view after `view`, wrapping; from a hidden view, the first.
pub fn next_view(view: ViewKind) -> ViewKind {
    let next = tab_number(view).unwrap_or(0) % OFFERED_VIEWS.len();
    OFFERED_VIEWS[next]
}

/// The offered view before `view`, wrapping; from a hidden view, the first.
pub fn previous_view(view: ViewKind) -> ViewKind {
    match tab_number(view) {
        Some(number) => OFFERED_VIEWS[(number + OFFERED_VIEWS.len() - 2) % OFFERED_VIEWS.len()],
        None => OFFERED_VIEWS[0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_processes_and_actions_are_not_offered() {
        assert_eq!(tab_number(ViewKind::Sources), None);
        assert_eq!(tab_number(ViewKind::Processes), None);
        assert_eq!(tab_number(ViewKind::Actions), None);
        assert_eq!(tab_number(ViewKind::Overview), Some(1));
        assert_eq!(tab_number(ViewKind::Settings), Some(4));
    }
}
