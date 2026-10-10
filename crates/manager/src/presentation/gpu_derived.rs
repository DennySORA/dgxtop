//! Values the GPU view derives from what NVML reports (D-48): the peak
//! device-memory bandwidth and the per-direction capacity of the PCIe link.
//! Both are theoretical, marked derived, and missing whenever an input is,
//! never zero.

use dgxtop_core::gpu_bandwidth::{memory_peak_bytes_per_second, pcie_bytes_per_second};
use dgxtop_core::metric::{CanonicalUnit, Scalar};
use dgxtop_core::quality::{Quality, SampleKind};
use dgxtop_core::view::{CellState, MetricCell};

/// Peak memory bandwidth from the maximum memory clock (MHz) and the bus
/// width (bits).
pub(super) fn memory_bandwidth(
    max_memory_clock: &MetricCell,
    bus_width: &MetricCell,
) -> MetricCell {
    derived(&[max_memory_clock, bus_width], || {
        memory_peak_bytes_per_second(max_memory_clock.value_f64()?, bus_width.value_f64()?)
    })
}

/// Capacity of a PCIe link of `generation` and `width` lanes, per
/// direction.
pub(super) fn pcie_bandwidth(generation: &MetricCell, width: &MetricCell) -> MetricCell {
    derived(&[generation, width], || {
        let exact = |cell: &MetricCell| cell.value.and_then(Scalar::as_u64);
        pcie_bytes_per_second(exact(generation)?, exact(width)?)
    })
}

/// The value `compute` derives when every input has one; otherwise the
/// first missing input's state.
fn derived(inputs: &[&MetricCell], compute: impl FnOnce() -> Option<f64>) -> MetricCell {
    let unit = CanonicalUnit::BytesPerSecond;
    if let Some(missing) = inputs.iter().find(|cell| cell.value_f64().is_none()) {
        let state = if missing.state.has_value() {
            CellState::Unavailable
        } else {
            missing.state
        };
        return MetricCell::missing(unit, state);
    }
    let Some(value) = compute().and_then(|value| Scalar::from_f64(value).ok()) else {
        return MetricCell::missing(unit, CellState::Unavailable);
    };
    let degraded = inputs.iter().any(|cell| cell.state != CellState::Ok);
    MetricCell {
        value: Some(value),
        unit,
        state: if degraded {
            CellState::Degraded
        } else {
            CellState::Ok
        },
        kind: SampleKind::Derived,
        // The least trusted input bounds the derived value.
        quality: inputs
            .iter()
            .map(|cell| cell.quality)
            .max()
            .unwrap_or(Quality::Unknown),
        age_ms: inputs.iter().filter_map(|cell| cell.age_ms).max(),
        source_label: None,
        conflict: None,
        last_good: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reported(value: Scalar) -> MetricCell {
        MetricCell {
            value: Some(value),
            state: CellState::Ok,
            kind: SampleKind::Measured,
            quality: Quality::Verified,
            age_ms: Some(300),
            ..MetricCell::missing(CanonicalUnit::Count, CellState::Ok)
        }
    }

    #[test]
    fn a_missing_input_keeps_its_state_and_an_unknown_link_has_no_value() {
        let clock = reported(Scalar::from_f64(15_001.0).unwrap());
        let unsupported = MetricCell::missing(CanonicalUnit::Count, CellState::Unsupported);
        let missing = memory_bandwidth(&clock, &unsupported);
        assert_eq!(
            (missing.value, missing.state),
            (None, CellState::Unsupported)
        );
        let width = reported(Scalar::from_u64(16));
        let gen9 = pcie_bandwidth(&reported(Scalar::from_u64(9)), &width);
        assert_eq!((gen9.value, gen9.state), (None, CellState::Unavailable));
        let gen4 = pcie_bandwidth(&reported(Scalar::from_u64(4)), &width);
        assert_eq!(gen4.kind, SampleKind::Derived);
        assert_eq!(gen4.age_ms, Some(300));
    }
}
