//! `memory` read group, on entity `memory`, scope `system`:
//! `/proc/meminfo` fields in KiB, the swap and OOM-kill counters of
//! `/proc/vmstat` and the memory's pressure averages (D-135). A field the
//! kernel does not provide (e.g. MemAvailable before Linux 3.14, `oom_kill`
//! before 4.13) is reported unsupported; it is never derived from other
//! fields. `MemFree` is not read: on unified memory it says nothing about
//! what is usable, and used memory is total minus available. The swap
//! counters are pages, emitted as bytes of the host's page size.

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{EntityId, Scope};
use dgxtop_core::metric::native;
use dgxtop_core::naming::{entity, groups};
use dgxtop_core::observation::{Capability, EntityKind, Observation, ReadPlan};
use dgxtop_core::pressure::PressureResource;
use dgxtop_core::process_stat::PAGE_SIZE_UNKNOWN;
use dgxtop_core::quality::{RawKind, ReadStatus, Support};

use super::catalog::{
    EntityParts, KeyContext, LiveSource, Selected, SnapshotBuilder, SourceDraft, SourceSpec,
    source_draft,
};
use super::host::host_entity_id;
use super::locator::{DRIVER, FAMILY_MEMORY, ProcfsLocator, RANK_HOST_WIDE};
use super::mechanics::{ReadSnapshot, TimedRead, invariant, io_failure, timed};
use super::parse::{
    Field, MEM_FIELDS, MemField, MemInfo, VMSTAT_FIELDS, VmStat, VmStatField, parse_meminfo,
    parse_vmstat,
};
use super::pressure::{self, PressurePlace, PressureRead};
use super::records::{
    DerivationFailure, FIELD_ABSENT, PARSE_ERROR, ReadWindow, io_failed_record, removed_record,
    u64_field_record, u64_outcome_record,
};
use crate::common::SystemFiles;

pub(crate) const MEMINFO_PATH: &str = "/proc/meminfo";
pub(crate) const VMSTAT_PATH: &str = "/proc/vmstat";
const SWAP_BYTES_OVERFLOW: &str = "SWAP_BYTES_OVERFLOW";

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
    let mut sources = meminfo_sources(builder, keys, &entity_id, &scope);
    for capability in field_capabilities(&info, &entity_id) {
        builder.capability(capability);
    }
    sources.extend(vmstat_sources(builder, keys, files, &entity_id, &scope));
    let place = PressurePlace {
        resource: PressureResource::Memory,
        entity: &entity_id,
        scope: &scope,
        group: groups::MEMORY,
    };
    sources.extend(pressure::sources(builder, keys, files, &place));
    let parts = EntityParts {
        entity_id,
        kind: EntityKind::SystemMemory,
        parent_id: Some(host_entity_id()),
        display_name: "System memory".to_owned(),
        attributes: Vec::new(),
    };
    builder.add(FAMILY_MEMORY, parts, incarnation, sources);
}

fn meminfo_sources(
    builder: &SnapshotBuilder<'_, ProcfsLocator>,
    keys: &KeyContext,
    entity_id: &EntityId,
    scope: &Scope,
) -> Vec<SourceDraft<ProcfsLocator>> {
    MEM_FIELDS
        .into_iter()
        .filter_map(|field| {
            let origin = format!("procfs:{MEMINFO_PATH}:{}", field.kernel_name());
            let spec = SourceSpec {
                device_id: "meminfo",
                channel: field.kernel_name(),
                entity: entity_id,
                scope,
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
        .collect()
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
            Some(unsupported(field.metric_id(), entity_id, reason))
        })
        .collect()
}

fn unsupported(metric_id: &str, entity_id: &EntityId, reason: &str) -> Capability {
    Capability {
        capability_id: metric_id.to_owned(),
        entity_id: Some(entity_id.clone()),
        support: Support::Unsupported,
        reason: Some(reason.to_owned()),
        min_interval_ms: None,
    }
}

/// The `/proc/vmstat` counters this kernel writes; a counter it lacks, or
/// an unreadable file, is an unsupported capability and no source.
fn vmstat_sources(
    builder: &mut SnapshotBuilder<'_, ProcfsLocator>,
    keys: &KeyContext,
    files: &dyn SystemFiles,
    entity_id: &EntityId,
    scope: &Scope,
) -> Vec<SourceDraft<ProcfsLocator>> {
    let stat = files
        .read_to_string(VMSTAT_PATH)
        .map(|text| parse_vmstat(&text))
        .map_err(|error| io_failure(&error).1);
    let mut sources = Vec::new();
    for field in VMSTAT_FIELDS {
        let missing = match &stat {
            Ok(stat) => match stat.field(field) {
                Field::Value(_) => None,
                Field::Absent => Some(FIELD_ABSENT),
                Field::Invalid => Some(PARSE_ERROR),
            },
            Err(reason) => Some(reason.as_str()),
        };
        match missing {
            Some(reason) => {
                builder.capability(unsupported(field.metric_id(), entity_id, reason));
            }
            None => sources.extend(vmstat_source(builder, keys, entity_id, scope, field)),
        }
    }
    sources
}

fn vmstat_source(
    builder: &SnapshotBuilder<'_, ProcfsLocator>,
    keys: &KeyContext,
    entity_id: &EntityId,
    scope: &Scope,
    field: VmStatField,
) -> Option<SourceDraft<ProcfsLocator>> {
    let origin = format!("procfs:{VMSTAT_PATH}:{}", field.kernel_name());
    let spec = SourceSpec {
        device_id: "vmstat",
        channel: field.kernel_name(),
        entity: entity_id,
        scope,
        metric_id: field.metric_id(),
        group: groups::MEMORY,
        native_unit: field.native_unit(),
        raw_kind: RawKind::Counter,
        driver: DRIVER,
        device_path: VMSTAT_PATH,
        origin: &origin,
        update_interval_ms: None,
        label: None,
        rank: RANK_HOST_WIDE,
    };
    source_draft(builder.config(), keys, &spec, ProcfsLocator::VmStat(field))
}

/// Everything the `memory` group read needs.
pub(crate) struct MemoryRead<'a> {
    pub plan: &'a ReadPlan,
    pub files: &'a dyn SystemFiles,
    pub clock: &'a dyn TimeProvider,
    /// Bytes of a memory page, which turn the swap counters into bytes.
    pub page_size: Option<u64>,
}

/// Reads each file once for the selected sources that need it.
pub(crate) fn read(
    context: &MemoryRead<'_>,
    selection: &[Selected<'_, ProcfsLocator>],
) -> ReadSnapshot {
    let started_boot_ns = context.clock.now_boot_ns();
    let files = MemoryFiles::read(context, selection);
    let mut records = Vec::with_capacity(selection.len());
    for selected in selection {
        let record = match selected {
            Selected::Removed(descriptor) => {
                removed_record(context.plan, descriptor, context.clock.now_boot_ns())
            }
            Selected::Live(source) => files.record(context, source),
        };
        records.extend(record);
    }
    ReadSnapshot {
        records,
        started_boot_ns,
        finished_boot_ns: context.clock.now_boot_ns(),
        final_error: None,
    }
}

/// `/proc/meminfo`, `/proc/vmstat` and the memory's pressure file, each
/// read at most once per cycle.
struct MemoryFiles {
    meminfo: Option<TimedRead<MemInfo>>,
    vmstat: Option<TimedRead<VmStat>>,
    pressure: PressureRead,
}

impl MemoryFiles {
    fn read(context: &MemoryRead<'_>, selection: &[Selected<'_, ProcfsLocator>]) -> Self {
        let needs = |predicate: fn(&ProcfsLocator) -> bool| {
            selection.iter().any(
                |selected| matches!(selected, Selected::Live(source) if predicate(&source.locator)),
            )
        };
        let (files, clock) = (context.files, context.clock);
        let meminfo = needs(|locator| matches!(locator, ProcfsLocator::Memory(_))).then(|| {
            timed(clock, || {
                files
                    .read_to_string(MEMINFO_PATH)
                    .map(|text| parse_meminfo(&text))
            })
        });
        let vmstat = needs(|locator| matches!(locator, ProcfsLocator::VmStat(_))).then(|| {
            timed(clock, || {
                files
                    .read_to_string(VMSTAT_PATH)
                    .map(|text| parse_vmstat(&text))
            })
        });
        Self {
            meminfo,
            vmstat,
            pressure: PressureRead::of(PressureResource::Memory, selection, files, clock),
        }
    }

    fn record(
        &self,
        context: &MemoryRead<'_>,
        source: &LiveSource<ProcfsLocator>,
    ) -> Option<Observation> {
        let descriptor = &source.descriptor;
        match source.locator {
            ProcfsLocator::Memory(field) => {
                let read = self.meminfo.as_ref()?;
                let window = ReadWindow::from(read);
                match &read.result {
                    Ok(info) => {
                        u64_field_record(context.plan, descriptor, info.field(field), window)
                    }
                    Err(error) => io_failed_record(context.plan, descriptor, error, window),
                }
            }
            ProcfsLocator::VmStat(field) => {
                vmstat_record(context, source, field, self.vmstat.as_ref()?)
            }
            ProcfsLocator::Pressure { .. } => self.pressure.record(context.plan, source),
            _ => None,
        }
    }
}

fn vmstat_record(
    context: &MemoryRead<'_>,
    source: &LiveSource<ProcfsLocator>,
    field: VmStatField,
    read: &TimedRead<VmStat>,
) -> Option<Observation> {
    let descriptor = &source.descriptor;
    let window = ReadWindow::from(read);
    let stat = match &read.result {
        Ok(stat) => stat,
        Err(error) => return io_failed_record(context.plan, descriptor, error, window),
    };
    let value = stat.field(field);
    if !field.counts_pages() {
        return u64_field_record(context.plan, descriptor, value, window);
    }
    let bytes = match value {
        Field::Value(pages) => page_bytes(pages, context.page_size),
        Field::Absent => Err((ReadStatus::Unsupported, FIELD_ABSENT)),
        Field::Invalid => Err((ReadStatus::Error, PARSE_ERROR)),
    };
    u64_outcome_record(context.plan, descriptor, bytes, window)
}

/// `pages` in bytes; without the page size there is no value, never a
/// guessed 4 KiB.
fn page_bytes(pages: u64, page_size: Option<u64>) -> Result<u64, DerivationFailure> {
    let page_size = page_size.ok_or((ReadStatus::Unsupported, PAGE_SIZE_UNKNOWN))?;
    pages
        .checked_mul(page_size)
        .ok_or((ReadStatus::Error, SWAP_BYTES_OVERFLOW))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_time_identities_are_valid() {
        assert_eq!(memory_entity_id().as_str(), "memory");
        assert_eq!(memory_scope().as_str(), "system");
    }

    #[test]
    fn swap_pages_become_bytes_of_the_hosts_page_size() {
        assert_eq!(page_bytes(3, Some(4096)), Ok(12_288));
        assert_eq!(page_bytes(3, Some(65_536)), Ok(196_608));
        assert_eq!(
            page_bytes(3, None),
            Err((ReadStatus::Unsupported, PAGE_SIZE_UNKNOWN))
        );
        assert_eq!(
            page_bytes(u64::MAX, Some(4096)),
            Err((ReadStatus::Error, SWAP_BYTES_OVERFLOW))
        );
    }
}
