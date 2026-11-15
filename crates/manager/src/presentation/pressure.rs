//! A resource's stall pressure (D-135) for the module that shows it: the
//! kernel's averages, shortest first, where it reports any. A host whose
//! kernel has no PSI never committed one of them, and its modules get no
//! pressure at all instead of a row of missing values.

use dgxtop_core::pressure::{PRESSURE_WINDOWS, PressureKind, PressureResource};
use dgxtop_core::view::{MetricCell, PressureView};

use super::reader::MetricReader;

/// The pressure of `resource`, whose averages are series of `entity` in
/// `scope`; `None` while the snapshot holds none of them.
pub(crate) fn pressure_view(
    reader: &MetricReader<'_>,
    resource: PressureResource,
    entity: &str,
    scope: &str,
) -> Option<PressureView> {
    let reported = resource
        .metric_ids()
        .any(|metric| reader.index().get(metric, entity, scope).is_some());
    if !reported {
        return None;
    }
    let averages = |kind: PressureKind| -> Option<[MetricCell; 3]> {
        let mut cells = Vec::with_capacity(PRESSURE_WINDOWS.len());
        for window in PRESSURE_WINDOWS {
            let metric = resource.metric_id(kind, window)?;
            cells.push(reader.cell(metric, entity, scope));
        }
        cells.try_into().ok()
    };
    Some(PressureView {
        some: averages(PressureKind::Some)?,
        full: averages(PressureKind::Full),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::testkit::{Fixture, NOW, sample};
    use dgxtop_core::metric::ids;
    use dgxtop_core::view::CellState;

    fn build(
        fixture: &Fixture,
        resource: PressureResource,
        entity: &str,
        scope: &str,
    ) -> Option<PressureView> {
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW);
        pressure_view(&reader, resource, entity, scope)
    }

    fn values(cells: &[MetricCell; 3]) -> [Option<f64>; 3] {
        [
            cells[0].value_f64(),
            cells[1].value_f64(),
            cells[2].value_f64(),
        ]
    }

    /// The memory's pressure carries both lines, shortest average first; a
    /// true zero stays a value and an average not committed yet is missing,
    /// never zero.
    #[test]
    fn the_averages_come_shortest_first_with_zero_and_missing_apart() {
        let mut fixture = Fixture::new();
        for (metric, percent) in [
            (ids::MEMORY_PRESSURE_SOME_AVG300, 0.2),
            (ids::MEMORY_PRESSURE_SOME_AVG10, 2.4),
            (ids::MEMORY_PRESSURE_SOME_AVG60, 0.9),
            (ids::MEMORY_PRESSURE_FULL_AVG10, 0.0),
        ] {
            fixture.add("memory", sample(metric, "memory", "system").f64(percent));
        }
        let view = build(&fixture, PressureResource::Memory, "memory", "system").unwrap();
        assert_eq!(values(&view.some), [Some(2.4), Some(0.9), Some(0.2)]);
        let full = view.full.unwrap();
        assert_eq!(values(&full), [Some(0.0), None, None]);
        assert_eq!(full[0].state, CellState::Ok);
        assert_eq!(full[1].state, CellState::Unavailable);
    }

    /// The CPU has no `full`; a host whose kernel reports no pressure has
    /// no pressure view, and another resource's averages are not its own.
    #[test]
    fn the_cpu_has_no_full_and_a_kernel_without_psi_has_no_view() {
        let mut fixture = Fixture::new();
        assert_eq!(build(&fixture, PressureResource::Cpu, "host", "host"), None);
        fixture.add(
            "cpu",
            sample(ids::CPU_PRESSURE_SOME_AVG10, "host", "host").f64(1.5),
        );
        let view = build(&fixture, PressureResource::Cpu, "host", "host").unwrap();
        assert_eq!(values(&view.some), [Some(1.5), None, None]);
        assert!(view.full.is_none());
        assert_eq!(build(&fixture, PressureResource::Io, "host", "host"), None);
    }
}
