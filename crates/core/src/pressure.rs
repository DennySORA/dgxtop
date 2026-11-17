//! Pressure stall information (PSI, D-135).
//!
//! For a resource, the share of time tasks stalled waiting for it, as the
//! kernel averages it over its last 10, 60 and 300 seconds
//! (`/proc/pressure/<resource>`). `some` counts the time at least one task
//! stalled, `full` the time every task that was not idle stalled at once.
//! It measures time lost waiting, never how much of the resource is in use:
//! memory can be nearly full with no pressure.

use crate::metric::ids;

/// A resource the kernel reports stall time for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PressureResource {
    Cpu,
    Memory,
    Io,
}

/// Which tasks a pressure line counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PressureKind {
    /// At least one task stalled.
    Some,
    /// Every task that was not idle stalled at once.
    Full,
}

/// The time a kernel average spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PressureWindow {
    Avg10,
    Avg60,
    Avg300,
}

/// The averages of a pressure line, shortest first.
pub const PRESSURE_WINDOWS: [PressureWindow; 3] = [
    PressureWindow::Avg10,
    PressureWindow::Avg60,
    PressureWindow::Avg300,
];

impl PressureResource {
    pub const ALL: [Self; 3] = [Self::Cpu, Self::Memory, Self::Io];

    /// The file under `/proc/pressure`.
    #[must_use]
    pub const fn kernel_name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Io => "io",
        }
    }

    /// The kinds read for the whole system. The CPU has no `full`: the
    /// kernel defines it only inside a control group and writes zeros for
    /// the system.
    #[must_use]
    pub const fn kinds(self) -> &'static [PressureKind] {
        match self {
            Self::Cpu => &[PressureKind::Some],
            Self::Memory | Self::Io => &[PressureKind::Some, PressureKind::Full],
        }
    }

    /// The metric of one average; `None` for a kind the resource does not
    /// report ([`Self::kinds`]).
    #[must_use]
    pub const fn metric_id(
        self,
        kind: PressureKind,
        window: PressureWindow,
    ) -> Option<&'static str> {
        use PressureKind::{Full, Some as SomeTasks};
        use PressureWindow::{Avg10, Avg60, Avg300};
        let id = match (self, kind, window) {
            (Self::Cpu, SomeTasks, Avg10) => ids::CPU_PRESSURE_SOME_AVG10,
            (Self::Cpu, SomeTasks, Avg60) => ids::CPU_PRESSURE_SOME_AVG60,
            (Self::Cpu, SomeTasks, Avg300) => ids::CPU_PRESSURE_SOME_AVG300,
            (Self::Cpu, Full, _) => return None,
            (Self::Memory, SomeTasks, Avg10) => ids::MEMORY_PRESSURE_SOME_AVG10,
            (Self::Memory, SomeTasks, Avg60) => ids::MEMORY_PRESSURE_SOME_AVG60,
            (Self::Memory, SomeTasks, Avg300) => ids::MEMORY_PRESSURE_SOME_AVG300,
            (Self::Memory, Full, Avg10) => ids::MEMORY_PRESSURE_FULL_AVG10,
            (Self::Memory, Full, Avg60) => ids::MEMORY_PRESSURE_FULL_AVG60,
            (Self::Memory, Full, Avg300) => ids::MEMORY_PRESSURE_FULL_AVG300,
            (Self::Io, SomeTasks, Avg10) => ids::IO_PRESSURE_SOME_AVG10,
            (Self::Io, SomeTasks, Avg60) => ids::IO_PRESSURE_SOME_AVG60,
            (Self::Io, SomeTasks, Avg300) => ids::IO_PRESSURE_SOME_AVG300,
            (Self::Io, Full, Avg10) => ids::IO_PRESSURE_FULL_AVG10,
            (Self::Io, Full, Avg60) => ids::IO_PRESSURE_FULL_AVG60,
            (Self::Io, Full, Avg300) => ids::IO_PRESSURE_FULL_AVG300,
        };
        Some(id)
    }

    /// Every metric the resource reports: each kind's averages, shortest
    /// first.
    pub fn metric_ids(self) -> impl Iterator<Item = &'static str> {
        self.kinds().iter().flat_map(move |kind| {
            PRESSURE_WINDOWS
                .into_iter()
                .filter_map(move |window| self.metric_id(*kind, window))
        })
    }

    /// Whether `metric_id` is one of the resource's averages.
    #[must_use]
    pub fn reports(self, metric_id: &str) -> bool {
        self.metric_ids().any(|id| id == metric_id)
    }
}

impl PressureKind {
    /// The word a pressure line starts with.
    #[must_use]
    pub const fn kernel_name(self) -> &'static str {
        match self {
            Self::Some => "some",
            Self::Full => "full",
        }
    }
}

impl PressureWindow {
    /// The key of the average on a pressure line.
    #[must_use]
    pub const fn kernel_name(self) -> &'static str {
        match self {
            Self::Avg10 => "avg10",
            Self::Avg60 => "avg60",
            Self::Avg300 => "avg300",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::metric::{CanonicalUnit, descriptor};

    /// Every average has a catalogued percentage of its own, named after
    /// its resource, kind and window as the kernel spells them.
    #[test]
    fn every_average_is_a_catalogued_percentage() {
        let mut seen = BTreeSet::new();
        for resource in PressureResource::ALL {
            for kind in resource.kinds() {
                for window in PRESSURE_WINDOWS {
                    let id = resource.metric_id(*kind, window).unwrap();
                    assert_eq!(
                        id,
                        format!(
                            "{}.pressure.{}.{}",
                            resource.kernel_name(),
                            kind.kernel_name(),
                            window.kernel_name()
                        )
                    );
                    assert!(seen.insert(id), "{id} is listed twice");
                    assert_eq!(descriptor(id).unwrap().unit, CanonicalUnit::Percent, "{id}");
                    assert!(resource.reports(id));
                }
            }
        }
        assert_eq!(seen.len(), 15);
    }

    /// The CPU's `full` is no system-wide figure, so it has no metric.
    #[test]
    fn the_cpu_has_no_full_pressure() {
        for window in PRESSURE_WINDOWS {
            assert_eq!(
                PressureResource::Cpu.metric_id(PressureKind::Full, window),
                None
            );
        }
        assert_eq!(PressureResource::Cpu.metric_ids().count(), 3);
        assert_eq!(PressureResource::Memory.metric_ids().count(), 6);
        assert!(!PressureResource::Memory.reports(ids::IO_PRESSURE_SOME_AVG10));
    }
}
