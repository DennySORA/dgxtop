//! `disk` read group: `/proc/diskstats` counters per block device, entity
//! `blockdev:MAJ:MIN`. `ram*` and `loop*` devices are skipped. The scope is
//! `partition` when `/sys/class/block/<name>/partition` exists (static
//! metadata cached per generation); a renamed device or a new major:minor is
//! a new incarnation, so counter bases are never continued across devices.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{EntityId, Scope};
use dgxtop_core::naming::{attributes, entity, groups};
use dgxtop_core::observation::{Attribute, EntityKind, Observation, ReadPlan};
use dgxtop_core::quality::{RawKind, ReadStatus};

use super::catalog::{
    EntityParts, KeyContext, LiveSource, Selected, SnapshotBuilder, SourceDraft, SourceSpec,
    source_draft,
};
use super::host::host_entity_id;
use super::locator::{DRIVER, FAMILY_DISK, ProcfsLocator, RANK_DEVICE};
use super::mechanics::{ReadSnapshot, TimedRead, invariant, io_failure, timed};
use super::parse::{DISK_FIELDS, DiskLine, parse_diskstats};
use super::records::{
    DEVICE_NOT_PRESENT, ReadWindow, failed_record, io_failed_record, removed_record,
    u64_field_record,
};
use crate::common::SystemFiles;

pub(crate) const DISKSTATS_PATH: &str = "/proc/diskstats";
const DEVICE_RECREATED: &str = "DEVICE_RECREATED";
/// Kernel block device names are short (DISK_NAME_LEN is 32).
const MAX_DEVICE_NAME_BYTES: usize = 64;

/// Whether a diskstats device is excluded (RAM disks and loop devices carry
/// no physical I/O).
fn is_skipped_device(name: &str) -> bool {
    name.starts_with("ram") || name.starts_with("loop")
}

/// sysfs directory name of a block device (`/` becomes `!`, the kernel
/// convention); names that could escape `/sys/class/block` are rejected.
fn sysfs_block_name(name: &str) -> Option<String> {
    let valid = !name.is_empty()
        && name.len() <= MAX_DEVICE_NAME_BYTES
        && name != "."
        && name != ".."
        && !name.chars().any(char::is_control);
    valid.then(|| name.replace('/', "!"))
}

/// Partition flag per block device incarnation.
#[derive(Debug, Clone, Default)]
pub(crate) struct DiskScopeCache {
    entries: BTreeMap<EntityId, (u32, bool)>,
}

impl DiskScopeCache {
    fn is_partition(
        &mut self,
        files: &dyn SystemFiles,
        entity_id: &EntityId,
        generation: u32,
        sysfs_name: &str,
    ) -> bool {
        if let Some((cached_generation, partition)) = self.entries.get(entity_id)
            && *cached_generation == generation
        {
            return *partition;
        }
        let partition = files.exists(&format!("/sys/class/block/{sysfs_name}/partition"));
        self.entries
            .insert(entity_id.clone(), (generation, partition));
        partition
    }

    fn retain(&mut self, present: &BTreeSet<EntityId>) {
        self.entries
            .retain(|entity_id, _| present.contains(entity_id));
    }
}

/// Adds every block device of `/proc/diskstats`; an unreadable file carries
/// the previous devices over and is reported in the delta's error code.
pub(crate) fn discover(
    builder: &mut SnapshotBuilder<'_, ProcfsLocator>,
    keys: &KeyContext,
    files: &dyn SystemFiles,
    cache: &mut DiskScopeCache,
) {
    let text = match files.read_to_string(DISKSTATS_PATH) {
        Ok(text) => text,
        Err(error) => {
            builder.carry_over_family(FAMILY_DISK);
            builder.error(format!("DISKSTATS_UNREADABLE:{}", io_failure(&error).1));
            return;
        }
    };
    let mut present = BTreeSet::new();
    for line in parse_diskstats(&text) {
        if is_skipped_device(&line.name) {
            continue;
        }
        if let Some(entity_id) = add_device(builder, keys, files, cache, &line) {
            present.insert(entity_id);
        }
    }
    cache.retain(&present);
}

fn add_device(
    builder: &mut SnapshotBuilder<'_, ProcfsLocator>,
    keys: &KeyContext,
    files: &dyn SystemFiles,
    cache: &mut DiskScopeCache,
    line: &DiskLine,
) -> Option<EntityId> {
    let (Some(sysfs_name), Ok(entity_id)) = (
        sysfs_block_name(&line.name),
        entity::block_device(line.major, line.minor),
    ) else {
        builder.error("BLOCK_DEVICE_IDENTITY_INVALID");
        return None;
    };
    let incarnation = builder.incarnation(&entity_id, &line.name)?;
    let partition = cache.is_partition(files, &entity_id, incarnation.generation, &sysfs_name);
    let scope_name = if partition {
        entity::SCOPE_PARTITION
    } else {
        entity::SCOPE_DEVICE
    };
    let scope = invariant(Scope::new(scope_name), scope_name);
    let sources = device_sources(builder, keys, &entity_id, &scope, line);
    let major_minor = format!("{}:{}", line.major, line.minor);
    let parts = EntityParts {
        entity_id: entity_id.clone(),
        kind: EntityKind::BlockDevice,
        parent_id: Some(host_entity_id()),
        display_name: line.name.clone(),
        attributes: vec![
            Attribute {
                key: attributes::MAJOR_MINOR.to_owned(),
                value: major_minor,
            },
            Attribute {
                key: attributes::DEVICE.to_owned(),
                value: line.name.clone(),
            },
        ],
    };
    builder.add(FAMILY_DISK, parts, incarnation, sources);
    Some(entity_id)
}

fn device_sources(
    builder: &SnapshotBuilder<'_, ProcfsLocator>,
    keys: &KeyContext,
    entity_id: &EntityId,
    scope: &Scope,
    line: &DiskLine,
) -> Vec<SourceDraft<ProcfsLocator>> {
    let device = format!("blockdev.{}.{}", line.major, line.minor);
    DISK_FIELDS
        .into_iter()
        .filter_map(|field| {
            let origin = format!(
                "procfs:{DISKSTATS_PATH}:{}:{}.{}",
                line.major,
                line.minor,
                field.channel()
            );
            let spec = SourceSpec {
                device_id: &device,
                channel: field.channel(),
                entity: entity_id,
                scope,
                metric_id: field.metric_id(),
                group: groups::DISK,
                native_unit: field.native_unit(),
                raw_kind: RawKind::Counter,
                driver: DRIVER,
                device_path: DISKSTATS_PATH,
                origin: &origin,
                update_interval_ms: None,
                label: Some(&line.name),
                rank: RANK_DEVICE,
            };
            let locator = ProcfsLocator::Disk {
                major: line.major,
                minor: line.minor,
                name: line.name.clone(),
                field,
            };
            source_draft(builder.config(), keys, &spec, locator)
        })
        .collect()
}

/// Reads `/proc/diskstats` once for every selected device field.
pub(crate) fn read(
    plan: &ReadPlan,
    selection: &[Selected<'_, ProcfsLocator>],
    files: &dyn SystemFiles,
    clock: &dyn TimeProvider,
) -> ReadSnapshot {
    let started_boot_ns = clock.now_boot_ns();
    let diskstats = DiskstatsRead::read(files, clock);
    let mut records = Vec::with_capacity(selection.len());
    for selected in selection {
        let record = match selected {
            Selected::Removed(descriptor) => removed_record(plan, descriptor, clock.now_boot_ns()),
            Selected::Live(source) => diskstats.record(plan, source),
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

/// One `/proc/diskstats` read indexed by `major:minor`.
struct DiskstatsRead {
    read: TimedRead<String>,
    lines: BTreeMap<(u32, u32), DiskLine>,
}

impl DiskstatsRead {
    fn read(files: &dyn SystemFiles, clock: &dyn TimeProvider) -> Self {
        let read = timed(clock, || files.read_to_string(DISKSTATS_PATH));
        let lines = read
            .result
            .as_ref()
            .map(|text| {
                parse_diskstats(text)
                    .into_iter()
                    .map(|line| ((line.major, line.minor), line))
                    .collect()
            })
            .unwrap_or_default();
        Self { read, lines }
    }

    /// The catalogued device must still be present under the same name;
    /// another name on the same major:minor is a new device.
    fn record(&self, plan: &ReadPlan, source: &LiveSource<ProcfsLocator>) -> Option<Observation> {
        let ProcfsLocator::Disk {
            major,
            minor,
            name,
            field,
        } = &source.locator
        else {
            return None;
        };
        let descriptor = &source.descriptor;
        let window = ReadWindow::from(&self.read);
        if let Err(error) = &self.read.result {
            return io_failed_record(plan, descriptor, error, window);
        }
        match self.lines.get(&(*major, *minor)) {
            None => failed_record(
                plan,
                descriptor,
                ReadStatus::Removed,
                DEVICE_NOT_PRESENT,
                window,
            ),
            Some(line) if &line.name != name => failed_record(
                plan,
                descriptor,
                ReadStatus::Removed,
                DEVICE_RECREATED,
                window,
            ),
            Some(line) => u64_field_record(plan, descriptor, line.field(*field), window),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysfs_names_cannot_escape_the_block_class() {
        assert_eq!(
            sysfs_block_name("cciss/c0d0").as_deref(),
            Some("cciss!c0d0")
        );
        assert_eq!(sysfs_block_name("sda").as_deref(), Some("sda"));
        assert!(sysfs_block_name("..").is_none());
        assert!(sysfs_block_name("").is_none());
        assert!(sysfs_block_name("bad\u{1b}name").is_none());
        assert!(sysfs_block_name(&"x".repeat(65)).is_none());
        assert_eq!(sysfs_block_name("../../etc").as_deref(), Some("..!..!etc"));
    }
}
