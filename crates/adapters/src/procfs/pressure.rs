//! Pressure stall information (D-135): the averages of
//! `/proc/pressure/<resource>` as percent gauges on the entity each
//! resource belongs to, in the read group that feeds its domain's
//! collector: the memory's with `memory`, the CPU's with `cpu`, the I/O's
//! with `disk`. A kernel without PSI, or with it turned off at boot
//! (`psi=0`), has no file to read: the resource is reported unsupported and
//! gets no source. A group reads its file once a cycle, and only while one
//! of its selected sources needs it.

use std::io;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{EntityId, Scope};
use dgxtop_core::metric::native;
use dgxtop_core::observation::{Capability, Observation, ReadPlan};
use dgxtop_core::pressure::{PRESSURE_WINDOWS, PressureKind, PressureResource};
use dgxtop_core::quality::{RawKind, Support};

use super::catalog::{
    KeyContext, LiveSource, Selected, SnapshotBuilder, SourceDraft, SourceSpec, source_draft,
};
use super::locator::{DRIVER, ProcfsLocator, RANK_HOST_WIDE};
use super::mechanics::{TimedRead, io_failure, timed};
use super::parse::{Pressure, parse_pressure};
use super::records::{FIELD_ABSENT, ReadWindow, f64_field_record, io_failed_record};
use crate::common::SystemFiles;

/// The kernel refreshes the averages every 2 s (`PSI_FREQ`); reading more
/// often does not produce a new measurement.
const PRESSURE_UPDATE_MS: u32 = 2_000;
/// Reason of a kernel built without PSI.
const PSI_NOT_PRESENT: &str = "PSI_NOT_PRESENT";

pub(crate) fn pressure_path(resource: PressureResource) -> String {
    format!("/proc/pressure/{}", resource.kernel_name())
}

/// Where a resource's pressure sources go.
pub(crate) struct PressurePlace<'a> {
    pub resource: PressureResource,
    pub entity: &'a EntityId,
    pub scope: &'a Scope,
    pub group: &'static str,
}

/// The sources of the averages the kernel writes for `place.resource`;
/// none, with the reason as a capability, where it offers no pressure
/// file or not every line.
pub(crate) fn sources(
    builder: &mut SnapshotBuilder<'_, ProcfsLocator>,
    keys: &KeyContext,
    files: &dyn SystemFiles,
    place: &PressurePlace<'_>,
) -> Vec<SourceDraft<ProcfsLocator>> {
    let path = pressure_path(place.resource);
    let pressure = match files.read_to_string(&path) {
        Ok(text) => parse_pressure(&text),
        Err(error) => {
            for kind in place.resource.kinds() {
                builder.capability(unsupported(place, *kind, &unreadable_reason(&error)));
            }
            return Vec::new();
        }
    };
    let mut sources = Vec::new();
    for kind in place.resource.kinds() {
        if pressure.has(*kind) {
            sources.extend(kind_sources(builder, keys, place, *kind, &path));
        } else {
            builder.capability(unsupported(place, *kind, FIELD_ABSENT));
        }
    }
    sources
}

fn unreadable_reason(error: &io::Error) -> String {
    if error.kind() == io::ErrorKind::NotFound {
        PSI_NOT_PRESENT.to_owned()
    } else {
        io_failure(error).1
    }
}

fn kind_sources(
    builder: &SnapshotBuilder<'_, ProcfsLocator>,
    keys: &KeyContext,
    place: &PressurePlace<'_>,
    kind: PressureKind,
    path: &str,
) -> Vec<SourceDraft<ProcfsLocator>> {
    let device = format!("pressure.{}", place.resource.kernel_name());
    PRESSURE_WINDOWS
        .into_iter()
        .filter_map(|window| {
            let metric_id = place.resource.metric_id(kind, window)?;
            let channel = format!("{}.{}", kind.kernel_name(), window.kernel_name());
            let origin = format!("procfs:{path}:{channel}");
            let spec = SourceSpec {
                device_id: &device,
                channel: &channel,
                entity: place.entity,
                scope: place.scope,
                metric_id,
                group: place.group,
                native_unit: native::PERCENT,
                raw_kind: RawKind::Gauge,
                driver: DRIVER,
                device_path: path,
                origin: &origin,
                update_interval_ms: Some(PRESSURE_UPDATE_MS),
                label: None,
                rank: RANK_HOST_WIDE,
            };
            let locator = ProcfsLocator::Pressure {
                resource: place.resource,
                kind,
                window,
            };
            source_draft(builder.config(), keys, &spec, locator)
        })
        .collect()
}

/// The capability of a kind's shortest average, which stands for the line.
fn unsupported(place: &PressurePlace<'_>, kind: PressureKind, reason: &str) -> Capability {
    let capability_id = place
        .resource
        .metric_id(kind, PRESSURE_WINDOWS[0])
        .map_or_else(
            || format!("{}.pressure", place.resource.kernel_name()),
            str::to_owned,
        );
    Capability {
        capability_id,
        entity_id: Some(place.entity.clone()),
        support: Support::Unsupported,
        reason: Some(reason.to_owned()),
        min_interval_ms: None,
    }
}

/// One read of a resource's pressure file, shared by the sources of a
/// cycle; not read at all when no selected source is one of its averages.
pub(crate) struct PressureRead {
    resource: PressureResource,
    read: Option<TimedRead<Pressure>>,
}

impl PressureRead {
    pub(crate) fn of(
        resource: PressureResource,
        selection: &[Selected<'_, ProcfsLocator>],
        files: &dyn SystemFiles,
        clock: &dyn TimeProvider,
    ) -> Self {
        let needed = selection.iter().any(|selected| {
            matches!(
                selected,
                Selected::Live(source)
                    if matches!(source.locator, ProcfsLocator::Pressure { resource: wanted, .. } if wanted == resource)
            )
        });
        let read = needed.then(|| {
            timed(clock, || {
                files
                    .read_to_string(&pressure_path(resource))
                    .map(|text| parse_pressure(&text))
            })
        });
        Self { resource, read }
    }

    /// The record of `source` when it is one of this resource's averages.
    pub(crate) fn record(
        &self,
        plan: &ReadPlan,
        source: &LiveSource<ProcfsLocator>,
    ) -> Option<Observation> {
        let ProcfsLocator::Pressure {
            resource,
            kind,
            window,
        } = source.locator
        else {
            return None;
        };
        if resource != self.resource {
            return None;
        }
        let read = self.read.as_ref()?;
        let window_read = ReadWindow::from(read);
        match &read.result {
            Ok(pressure) => f64_field_record(
                plan,
                &source.descriptor,
                pressure.average(kind, window),
                window_read,
            ),
            Err(error) => io_failed_record(plan, &source.descriptor, error, window_read),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_resource_has_its_own_file() {
        assert_eq!(
            pressure_path(PressureResource::Memory),
            "/proc/pressure/memory"
        );
        assert_eq!(pressure_path(PressureResource::Io), "/proc/pressure/io");
        assert_eq!(pressure_path(PressureResource::Cpu), "/proc/pressure/cpu");
    }
}
