//! What the GPU view asks of a card's values (D-47 to D-50): whether the
//! device reports one, its exact count, the throttle reasons in words and
//! colors, and the driver's attributes. A value the device does not report
//! is left out, so no row lists one; the card's note names it once.

use dgxtop_core::gpu_state::{ThrottleSeverity, throttle_reasons, throttle_severity};
use dgxtop_core::metric::ScalarView;
use dgxtop_core::view::{CellState, GpuCard, GpuDetails, MetricCell};
use ratatui::style::Style;

use crate::format::clean;
use crate::registry::DisplayContext;

/// Whether the GPU view lists `cell`: not when the device does not support
/// it or reports nothing for it, which the card's notes explain.
pub(super) fn reported(cell: &MetricCell) -> bool {
    cell.value.is_some() || !matches!(cell.state, CellState::Unsupported | CellState::Unavailable)
}

/// Whether `cell` holds a value to show.
pub(super) fn has_value(cell: &MetricCell) -> bool {
    cell.state.has_value() && cell.value.is_some()
}

/// An exact count's value (the throttle bits).
pub(super) fn exact_count(cell: &MetricCell) -> Option<u64> {
    if !cell.state.has_value() {
        return None;
    }
    match cell.value?.view() {
        ScalarView::U64(count) => Some(count),
        ScalarView::F64(_) => None,
    }
}

/// The reasons set in `bits`, `none` without one.
pub(super) fn throttle_text(bits: u64) -> String {
    let reasons: Vec<&str> = throttle_reasons(bits).map(|reason| reason.label).collect();
    if reasons.is_empty() {
        "none".to_owned()
    } else {
        reasons.join(", ")
    }
}

/// The color of the worst reason set in `bits`: a slowdown from heat or a
/// hardware signal is critical, a power cap a warning, idle and set clocks
/// normal.
pub(super) fn throttle_style(bits: u64, context: &DisplayContext) -> Style {
    let theme = &context.theme;
    match throttle_severity(bits) {
        Some(ThrottleSeverity::Alert) => Style::new().fg(theme.danger),
        Some(ThrottleSeverity::Notice) => Style::new().fg(theme.warning),
        Some(ThrottleSeverity::Benign) | None => theme.text_style(),
    }
}

/// An attribute of the card's driver metadata or its details, by key
/// (`dgxtop_core::naming::attributes`).
pub(super) fn attribute(card: &GpuCard, key: &str) -> Option<String> {
    let details = card
        .details
        .as_deref()
        .map_or(&[][..], |details: &GpuDetails| {
            details.attributes.as_slice()
        });
    card.metadata
        .iter()
        .chain(details)
        .find(|(name, _)| name == key)
        .map(|(_, value)| clean(value))
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{missing, ok};
    use dgxtop_core::metric::CanonicalUnit;

    #[test]
    fn unsupported_and_unreported_values_are_left_out() {
        assert!(!reported(&missing(
            CanonicalUnit::Count,
            CellState::Unsupported
        )));
        assert!(!reported(&missing(
            CanonicalUnit::Count,
            CellState::Unavailable
        )));
        // A value that is late or unreadable says so.
        assert!(reported(&missing(CanonicalUnit::Count, CellState::Warmup)));
        assert!(reported(&missing(
            CanonicalUnit::Count,
            CellState::PermissionDenied
        )));
        assert!(reported(&ok(1.0, CanonicalUnit::Count)));
        assert!(!has_value(&missing(
            CanonicalUnit::Count,
            CellState::Warmup
        )));
        assert_eq!(exact_count(&ok(1.0, CanonicalUnit::Count)), None);
    }
}
