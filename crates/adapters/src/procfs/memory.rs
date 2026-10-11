//! `memory` read group: `/proc/meminfo` fields in KiB on entity `memory`,
//! scope `system`. A field the kernel does not provide (e.g. MemAvailable
//! before Linux 3.14) is reported unsupported; it is never derived from
//! other fields. `MemFree` is not read: on unified memory it says nothing
//! about what is usable, and used memory is total minus available.

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{EntityId, Scope};
use dgxtop_core::metric::native;
use dgxtop_core::naming::{entity, groups};
use dgxtop_core::observation::{Capability, EntityKind, Observation, ReadPlan};
use dgxtop_core::quality::{RawKind, Support};

use super::catalog::{
    EntityParts, KeyContext, LiveSource, Selected, SnapshotBuilder, SourceSpec, source_draft,
};
use super::host::host_entity_id;
use super::locator::{DRIVER, FAMILY_MEMORY, ProcfsLocator, RANK_HOST_WIDE};
use super::mechanics::{ReadSnapshot, TimedRead, invariant, io_failure, timed};
use super::parse::{Field, MEM_FIELDS, MemField, MemInfo, parse_meminfo};
use super::records::{
    FIELD_ABSENT, PARSE_ERROR, ReadWindow, io_failed_record, removed_record, u64_field_record,
};
use crate::common::SystemFiles;

pub(crate) const MEMINFO_PATH: &str = "/proc/meminfo";

fn memory_entity_id() -> EntityId {
    invariant(EntityId::new(entity::MEMORY), entity::MEMORY)
}

fn memory_scope() -> Scope {
    invariant(Scope::new(entity::SCOPE_SYSTEM), entity::SCOPE_SYSTEM)
}

/// Adds the `memory` entity; an unreadable `/proc/meminfo` carries the
/// previous entity over and is reported in the delta's error code.
pub(crate) fn discover(
    builder: &mut SnapshotBuilder<'_, ProcfsLocator>,
    keys: &KeyContext,
    files: &dyn SystemFiles,
) {
    let text = match files.read_to_string(MEMINFO_PATH) {
        Ok(text) => text,
        Err(error) => {
            builder.carry_over_family(FAMILY_MEMORY);
            builder.error(format!("MEMINFO_UNREADABLE:{}", io_failure(&error).1));
            return;
        }
    };
    let info = parse_meminfo(&text);
    let entity_id = memory_entity_id();
    let Some(incarnation) = builder.incarnation(&entity_id, entity::MEMORY) else {
        return;
    };
    let scope = memory_scope();
    let sources = MEM_FIELDS
        .into_iter()
        .filter_map(|field| {
            let origin = format!("procfs:{MEMINFO_PATH}:{}", field.kernel_name());
            let spec = SourceSpec {
                device_id: "meminfo",
                channel: field.kernel_name(),
                entity: &entity_id,
                scope: &scope,
                metric_id: field.metric_id(),
                group: groups::MEMORY,
                native_unit: native::KIB,
                raw_kind: RawKind::Gauge,
                driver: DRIVER,
                device_path: MEMINFO_PATH,
                origin: &origin,
                update_interval_ms: None,
                label: None,
                rank: RANK_HOST_WIDE,
            };
            source_draft(builder.config(), keys, &spec, ProcfsLocator::Memory(field))
        })
        .collect();
    for capability in field_capabilities(&info, &entity_id) {
        builder.capability(capability);
    }
    let parts = EntityParts {
        entity_id,
        kind: EntityKind::SystemMemory,
        parent_id: Some(host_entity_id()),
        display_name: "System memory".to_owned(),
        attributes: Vec::new(),
    };
    builder.add(FAMILY_MEMORY, parts, incarnation, sources);
}

/// Unsupported capabilities for fields this kernel does not provide.
fn field_capabilities(info: &MemInfo, entity_id: &EntityId) -> Vec<Capability> {
    MEM_FIELDS
        .into_iter()
        .filter_map(|field: MemField| {
            let reason = match info.field(field) {
                Field::Value(_) => return None,
                Field::Absent => FIELD_ABSENT,
                Field::Invalid => PARSE_ERROR,
            };
            Some(Capability {
                capability_id: field.metric_id().to_owned(),
                entity_id: Some(entity_id.clone()),
                support: Support::Unsupported,
                reason: Some(reason.to_owned()),
                min_interval_ms: None,
            })
        })
        .collect()
}

/// Reads `/proc/meminfo` once for every selected field.
pub(crate) fn read(
    plan: &ReadPlan,
    selection: &[Selected<'_, ProcfsLocator>],
    files: &dyn SystemFiles,
    clock: &dyn TimeProvider,
) -> ReadSnapshot {
    let started_boot_ns = clock.now_boot_ns();
    let meminfo = timed(clock, || {
        files
            .read_to_string(MEMINFO_PATH)
            .map(|text| parse_meminfo(&text))
    });
    let mut records = Vec::with_capacity(selection.len());
    for selected in selection {
        let record = match selected {
            Selected::Removed(descriptor) => removed_record(plan, descriptor, clock.now_boot_ns()),
            Selected::Live(source) => field_record(plan, source, &meminfo),
        };
        records.extend(record);
    }
    ReadSnapshot {
        records,
        started_boot_ns,
        finished_boot_ns: clock.now_boot_ns(),
        final_error: None,
    }
}

fn field_record(
    plan: &ReadPlan,
    source: &LiveSource<ProcfsLocator>,
    meminfo: &TimedRead<MemInfo>,
) -> Option<Observation> {
    let ProcfsLocator::Memory(field) = source.locator else {
        return None;
    };
    let window = ReadWindow::from(meminfo);
    match &meminfo.result {
        Ok(info) => u64_field_record(plan, &source.descriptor, info.field(field), window),
        Err(error) => io_failed_record(plan, &source.descriptor, error, window),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_time_identities_are_valid() {
        assert_eq!(memory_entity_id().as_str(), "memory");
        assert_eq!(memory_scope().as_str(), "system");
    }
}
