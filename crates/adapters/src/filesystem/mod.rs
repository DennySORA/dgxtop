//! Filesystem adapter (`filesystem`, read group `capacity`, paged):
//! mountinfo + statvfs. Every mount of a disk-backed filesystem is its own
//! entity `fs:<mntns>:<mount_id>` (scope `filesystem`); several mounts of one
//! device are distinct entities whose capacities describe the same
//! filesystem, so consumers must never sum them. Network filesystems are
//! reported as unsupported (`NETWORK_FS_SKIPPED`) and never probed; before
//! each statvfs the read path re-checks mountinfo so a mount that changed,
//! got shadowed or sits below a network mount is not touched.

mod capacity;
mod mountinfo;

use std::collections::BTreeMap;
use std::sync::Arc;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{EntityId, HostId, ProfileId, Scope};
use dgxtop_core::metric::{ids, native};
use dgxtop_core::naming::{adapters, attributes, entity, groups};
use dgxtop_core::observation::{
    AdapterManifest, Attribute, Capability, CatalogDelta, DiscoverRequest, EntityKind, Observation,
    ObservationBatch, ReadPlan, SourceDescriptor,
};
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::{Availability, RawKind, ReadStatus, Support};

use self::mountinfo::{MountClass, MountEntry, MountTable, classify, parse_mountinfo};
use crate::common::{RecordFactory, SystemFiles};
use crate::procfs::availability_of;
use crate::procfs::catalog::{
    CatalogBook, CatalogConfig, EntityParts, KeyContext, LiveSource, Selected, SnapshotBuilder,
    SourceDraft, SourceSpec, source_draft,
};
use crate::procfs::mechanics::{
    GroupPagers, PlanTarget, ReadSnapshot, ReprobeGate, invariant, io_failure, namespace_link,
    plan_target, rejected_discovery, rejected_plan, timed,
};
use crate::procfs::records::{
    ReadWindow, deferred_record, failed_record, removed_record, u64_outcome_record,
};
use crate::registry::{AdapterOptions, compile_time_adapter_id, kernel_file_manifest};

pub use self::capacity::{CapacityFigures, CapacityProbe, StatvfsProbe};

/// Profile of mountinfo + statvfs capacities.
pub const PROFILE_ID: &str = "linux-mountinfo-statvfs";
pub const PROFILE_REVISION: u32 = 1;
/// Reason code of a network filesystem that is deliberately not probed.
pub const NETWORK_FS_SKIPPED: &str = "NETWORK_FS_SKIPPED";
/// Entity attribute holding the mount root (differs for bind mounts).
pub const MOUNT_ROOT_ATTRIBUTE: &str = "mount_root";
const IMPLEMENTATION: &str = "FilesystemAdapter";
const MINIMUM_SAMPLE_MS: u32 = 1000;
const MOUNTINFO_PATH: &str = "/proc/self/mountinfo";
const MOUNT_NAMESPACE_PATH: &str = "/proc/self/ns/mnt";
const DRIVER: &str = "statvfs";
const FAMILY_MOUNT: &str = "mount";

/// Static manifest with default options.
pub fn manifest() -> AdapterManifest {
    manifest_with(&AdapterOptions::default())
}

/// Manifest for `options` (source limit = min(max_sources, 2048)).
pub fn manifest_with(options: &AdapterOptions) -> AdapterManifest {
    kernel_file_manifest(
        adapters::FILESYSTEM,
        IMPLEMENTATION,
        MINIMUM_SAMPLE_MS,
        options,
    )
}

/// One statvfs-derived figure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapacityFigure {
    Total,
    Free,
    Available,
}

const CAPACITY_FIGURES: [CapacityFigure; 3] = [
    CapacityFigure::Total,
    CapacityFigure::Free,
    CapacityFigure::Available,
];

impl CapacityFigure {
    fn channel(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::Free => "free",
            Self::Available => "available",
        }
    }

    fn metric_id(self) -> &'static str {
        match self {
            Self::Total => ids::FS_TOTAL,
            Self::Free => ids::FS_FREE,
            Self::Available => ids::FS_AVAILABLE,
        }
    }

    /// Bytes of this figure; `None` on u64 overflow.
    fn bytes(self, figures: &CapacityFigures) -> Option<u64> {
        let fragments = match self {
            Self::Total => figures.blocks,
            Self::Free => figures.blocks_free,
            Self::Available => figures.blocks_available,
        };
        fragments.checked_mul(figures.fragment_size)
    }
}

/// How the read path obtains one capacity figure.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MountLocator {
    mount_id: u64,
    mount_point: String,
    fingerprint: String,
    figure: CapacityFigure,
}

/// Read-only mounted-filesystem capacity adapter.
pub struct FilesystemAdapter {
    clock: Arc<dyn TimeProvider>,
    host_id: HostId,
    files: Arc<dyn SystemFiles>,
    probe: Box<dyn CapacityProbe>,
    options: AdapterOptions,
    manifest: AdapterManifest,
    book: CatalogBook<MountLocator>,
    mount_namespace: Option<String>,
    read_gate: ReprobeGate,
    pagers: GroupPagers,
    shut_down: bool,
}

impl FilesystemAdapter {
    /// Probes capacities with statvfs under `/`.
    pub fn new(
        clock: Arc<dyn TimeProvider>,
        host_id: HostId,
        files: Arc<dyn SystemFiles>,
        options: AdapterOptions,
    ) -> Self {
        Self::with_capacity_probe(
            clock,
            host_id,
            files,
            options,
            Box::new(StatvfsProbe::host()),
        )
    }

    /// Like [`FilesystemAdapter::new`] with an explicit capacity probe.
    pub fn with_capacity_probe(
        clock: Arc<dyn TimeProvider>,
        host_id: HostId,
        files: Arc<dyn SystemFiles>,
        options: AdapterOptions,
        probe: Box<dyn CapacityProbe>,
    ) -> Self {
        let config = CatalogConfig {
            adapter_id: compile_time_adapter_id(adapters::FILESYSTEM),
            profile_id: invariant(ProfileId::new(PROFILE_ID), PROFILE_ID),
            profile_revision: PROFILE_REVISION,
            source_limit: usize::try_from(options.source_limit()).unwrap_or(usize::MAX),
            page_size: options.effective_page_size(),
        };
        Self {
            clock,
            host_id,
            files,
            probe,
            manifest: manifest_with(&options),
            book: CatalogBook::new(config),
            mount_namespace: None,
            read_gate: ReprobeGate::new(options.unsupported_probe_ms),
            pagers: GroupPagers::default(),
            shut_down: false,
            options,
        }
    }

    fn discover_snapshot(&mut self, request: &DiscoverRequest, now_boot_ns: u64) -> CatalogDelta {
        let keys = KeyContext {
            host_id: self.host_id.clone(),
            boot_id: self.clock.boot_id(),
        };
        if self.mount_namespace.is_none() {
            self.mount_namespace = namespace_link(&*self.files, MOUNT_NAMESPACE_PATH);
        }
        let Self {
            book,
            files,
            mount_namespace,
            ..
        } = self;
        let mut builder = book.begin_snapshot(now_boot_ns);
        let availability = match mount_namespace.as_deref() {
            Some(namespace) => enumerate(&mut builder, &keys, &**files, namespace),
            None => {
                builder.carry_over_family(FAMILY_MOUNT);
                builder.error("MOUNT_NAMESPACE_UNKNOWN");
                Availability::Unavailable
            }
        };
        builder.finish(request, availability)
    }

    fn read_capacity(
        &mut self,
        plan: &ReadPlan,
        target: &PlanTarget,
        now_boot_ns: u64,
    ) -> ObservationBatch {
        if !self.book.is_discovered() {
            return RecordFactory::failed_batch(plan, now_boot_ns, "CATALOG_NOT_DISCOVERED");
        }
        let page_size = self.options.effective_page_size();
        let Self {
            pagers,
            book,
            files,
            clock,
            probe,
            read_gate,
            ..
        } = self;
        pagers.serve(plan, target, page_size, now_boot_ns, || {
            let selection = book.select(groups::CAPACITY, &plan.source_slots);
            let context = CapacityRead {
                plan,
                files: &**files,
                clock: &**clock,
                probe: &**probe,
            };
            let mut snapshot = read_selection(&context, &selection.sources, read_gate);
            if selection.unknown_slots > 0 {
                snapshot.final_error = Some("UNKNOWN_SOURCE_SLOT".to_owned());
            }
            snapshot
        })
    }
}

/// Adds every disk-backed mount and every (skipped) network mount; an
/// unreadable mountinfo carries the previous mounts over.
fn enumerate(
    builder: &mut SnapshotBuilder<'_, MountLocator>,
    keys: &KeyContext,
    files: &dyn SystemFiles,
    namespace: &str,
) -> Availability {
    let text = match files.read_to_string(MOUNTINFO_PATH) {
        Ok(text) => text,
        Err(error) => {
            builder.carry_over_family(FAMILY_MOUNT);
            builder.error(format!("MOUNTINFO_UNREADABLE:{}", io_failure(&error).1));
            return availability_of(Some(&error));
        }
    };
    let mut skipped_network = 0usize;
    for entry in parse_mountinfo(&text) {
        match classify(&entry) {
            MountClass::Capacity => add_mount(builder, keys, namespace, &entry),
            MountClass::NetworkSkipped => {
                skipped_network += 1;
                add_network_mount(builder, namespace, &entry);
            }
            MountClass::Ignored => {}
        }
    }
    builder.capability(Capability {
        capability_id: "filesystem.capacity".to_owned(),
        entity_id: None,
        support: Support::Supported,
        reason: (skipped_network > 0).then(|| format!("{NETWORK_FS_SKIPPED}:{skipped_network}")),
        min_interval_ms: None,
    });
    Availability::Available
}

fn mount_parts(namespace: &str, entry: &MountEntry) -> Option<EntityParts> {
    let entity_id = entity::filesystem(namespace, entry.mount_id).ok()?;
    let parent_id = if entry.major == 0 {
        None
    } else {
        entity::block_device(entry.major, entry.minor).ok()
    };
    let attribute = |key: &str, value: &str| Attribute {
        key: key.to_owned(),
        value: value.to_owned(),
    };
    Some(EntityParts {
        entity_id,
        kind: EntityKind::Filesystem,
        parent_id,
        display_name: entry.mount_point.clone(),
        attributes: vec![
            attribute(attributes::MOUNT_POINT, &entry.mount_point),
            attribute(attributes::DEVICE, &entry.source),
            attribute(attributes::FS_TYPE, &entry.fs_type),
            attribute(attributes::MAJOR_MINOR, &entry.major_minor()),
            attribute(MOUNT_ROOT_ATTRIBUTE, &entry.root),
        ],
    })
}

fn add_mount(
    builder: &mut SnapshotBuilder<'_, MountLocator>,
    keys: &KeyContext,
    namespace: &str,
    entry: &MountEntry,
) {
    let Some(parts) = mount_parts(namespace, entry) else {
        builder.error("MOUNT_IDENTITY_INVALID");
        return;
    };
    let fingerprint = entry.fingerprint();
    let Some(incarnation) = builder.incarnation(&parts.entity_id, &fingerprint) else {
        return;
    };
    let sources = mount_sources(builder, keys, &parts.entity_id, entry, &fingerprint);
    builder.add(FAMILY_MOUNT, parts, incarnation, sources);
}

/// Network mounts are listed (without sources) so the unsupported
/// capability can point at them; statvfs is never called on them.
fn add_network_mount(
    builder: &mut SnapshotBuilder<'_, MountLocator>,
    namespace: &str,
    entry: &MountEntry,
) {
    let Some(parts) = mount_parts(namespace, entry) else {
        builder.error("MOUNT_IDENTITY_INVALID");
        return;
    };
    let Some(incarnation) = builder.incarnation(&parts.entity_id, &entry.fingerprint()) else {
        return;
    };
    builder.capability(Capability {
        capability_id: "filesystem.capacity".to_owned(),
        entity_id: Some(parts.entity_id.clone()),
        support: Support::Unsupported,
        reason: Some(NETWORK_FS_SKIPPED.to_owned()),
        min_interval_ms: None,
    });
    builder.add(FAMILY_MOUNT, parts, incarnation, Vec::new());
}

fn mount_sources(
    builder: &SnapshotBuilder<'_, MountLocator>,
    keys: &KeyContext,
    entity_id: &EntityId,
    entry: &MountEntry,
    fingerprint: &str,
) -> Vec<SourceDraft<MountLocator>> {
    let scope = invariant(
        Scope::new(entity::SCOPE_FILESYSTEM),
        entity::SCOPE_FILESYSTEM,
    );
    let device = format!("mount.{}", entry.mount_id);
    CAPACITY_FIGURES
        .into_iter()
        .filter_map(|figure| {
            let origin = format!("statvfs:{}:{}", entity_id.as_str(), figure.channel());
            let spec = SourceSpec {
                device_id: &device,
                channel: figure.channel(),
                entity: entity_id,
                scope: &scope,
                metric_id: figure.metric_id(),
                group: groups::CAPACITY,
                native_unit: native::BYTES,
                raw_kind: RawKind::Gauge,
                driver: DRIVER,
                device_path: &entry.mount_point,
                origin: &origin,
                update_interval_ms: None,
                label: Some(&entry.mount_point),
                rank: 0,
            };
            let locator = MountLocator {
                mount_id: entry.mount_id,
                mount_point: entry.mount_point.clone(),
                fingerprint: fingerprint.to_owned(),
                figure,
            };
            source_draft(builder.config(), keys, &spec, locator)
        })
        .collect()
}

/// Everything one capacity read needs.
struct CapacityRead<'a> {
    plan: &'a ReadPlan,
    files: &'a dyn SystemFiles,
    clock: &'a dyn TimeProvider,
    probe: &'a dyn CapacityProbe,
}

fn read_selection(
    context: &CapacityRead<'_>,
    selection: &[Selected<'_, MountLocator>],
    gate: &mut ReprobeGate,
) -> ReadSnapshot {
    let started_boot_ns = context.clock.now_boot_ns();
    let mounts_now = VerifiedMounts::read(context);
    let (mounts, removed) = group_by_mount(selection);
    let mut records = Vec::with_capacity(selection.len());
    for sources in &mounts {
        records.extend(read_mount(context, &mounts_now, sources, gate));
    }
    let now = context.clock.now_boot_ns();
    records.extend(
        removed
            .iter()
            .filter_map(|descriptor| removed_record(context.plan, descriptor, now)),
    );
    ReadSnapshot {
        records,
        started_boot_ns,
        finished_boot_ns: context.clock.now_boot_ns(),
        final_error: None,
    }
}

/// The mount table re-read before any statvfs; without it nothing is probed.
struct VerifiedMounts {
    table: Result<MountTable, (ReadStatus, String)>,
    window: ReadWindow,
}

impl VerifiedMounts {
    fn read(context: &CapacityRead<'_>) -> Self {
        let mountinfo = timed(context.clock, || {
            context.files.read_to_string(MOUNTINFO_PATH)
        });
        let table = match &mountinfo.result {
            Ok(text) => Ok(MountTable::new(parse_mountinfo(text))),
            Err(error) => Err(io_failure(error)),
        };
        Self {
            table,
            window: ReadWindow::from(&mountinfo),
        }
    }

    /// Why the catalogued mount must not be probed now, if anything.
    fn blocker(&self, locator: &MountLocator) -> Option<(ReadStatus, String)> {
        match &self.table {
            Err(failure) => Some(failure.clone()),
            Ok(table) => {
                mount_check(table, locator).map(|(status, reason)| (status, reason.to_owned()))
            }
        }
    }
}

type MountGroups<'a> = (
    Vec<Vec<&'a LiveSource<MountLocator>>>,
    Vec<&'a SourceDescriptor>,
);

/// Live figures grouped per mount (first-seen order) plus removed sources.
fn group_by_mount<'a>(selection: &[Selected<'a, MountLocator>]) -> MountGroups<'a> {
    let mut index: BTreeMap<u64, usize> = BTreeMap::new();
    let mut mounts: Vec<Vec<&'a LiveSource<MountLocator>>> = Vec::new();
    let mut removed = Vec::new();
    for selected in selection {
        match selected {
            Selected::Live(source) => {
                let position = *index.entry(source.locator.mount_id).or_insert_with(|| {
                    mounts.push(Vec::new());
                    mounts.len() - 1
                });
                mounts[position].push(*source);
            }
            Selected::Removed(descriptor) => removed.push(*descriptor),
        }
    }
    (mounts, removed)
}

/// Why a catalogued mount must not be probed now.
fn mount_check(table: &MountTable, locator: &MountLocator) -> Option<(ReadStatus, &'static str)> {
    let Some(entry) = table.get(locator.mount_id) else {
        return Some((ReadStatus::Removed, "MOUNT_REMOVED"));
    };
    if entry.fingerprint() != locator.fingerprint {
        return Some((ReadStatus::Removed, "MOUNT_CHANGED"));
    }
    if table.is_shadowed(entry) {
        return Some((ReadStatus::Error, "MOUNT_SHADOWED"));
    }
    if table.has_network_in_path(entry) {
        return Some((ReadStatus::Unsupported, "NETWORK_FS_IN_PATH"));
    }
    None
}

fn read_mount(
    context: &CapacityRead<'_>,
    mounts_now: &VerifiedMounts,
    sources: &[&LiveSource<MountLocator>],
    gate: &mut ReprobeGate,
) -> Vec<Observation> {
    let Some(first) = sources.first() else {
        return Vec::new();
    };
    let Some((status, reason)) = mounts_now.blocker(&first.locator) else {
        return probe_mount(context, sources, gate);
    };
    sources
        .iter()
        .filter_map(|source| {
            failed_record(
                context.plan,
                &source.descriptor,
                status,
                &reason,
                mounts_now.window,
            )
        })
        .collect()
}

fn probe_mount(
    context: &CapacityRead<'_>,
    sources: &[&LiveSource<MountLocator>],
    gate: &mut ReprobeGate,
) -> Vec<Observation> {
    let Some(first) = sources.first() else {
        return Vec::new();
    };
    let key = format!("mount:{}", first.locator.mount_id);
    let now = context.clock.now_boot_ns();
    if let Some(failure) = gate.deferred(&key, now) {
        return sources
            .iter()
            .filter_map(|source| deferred_record(context.plan, &source.descriptor, &failure, now))
            .collect();
    }
    let read = timed(context.clock, || {
        context.probe.capacity(&first.locator.mount_point)
    });
    let window = ReadWindow::from(&read);
    match &read.result {
        Ok(figures) => {
            gate.record_success(&key);
            sources
                .iter()
                .filter_map(|source| figure_record(context.plan, source, figures, window))
                .collect()
        }
        Err(error) => {
            let (status, reason) = io_failure(error);
            gate.record_failure(&key, read.finished_boot_ns, status, &reason);
            sources
                .iter()
                .filter_map(|source| {
                    failed_record(context.plan, &source.descriptor, status, &reason, window)
                })
                .collect()
        }
    }
}

fn figure_record(
    plan: &ReadPlan,
    source: &LiveSource<MountLocator>,
    figures: &CapacityFigures,
    window: ReadWindow,
) -> Option<Observation> {
    let bytes = source
        .locator
        .figure
        .bytes(figures)
        .ok_or((ReadStatus::Error, "CAPACITY_OVERFLOW"));
    u64_outcome_record(plan, &source.descriptor, bytes, window)
}

impl TelemetryPort for FilesystemAdapter {
    fn manifest(&self) -> &AdapterManifest {
        &self.manifest
    }

    fn discover(&mut self, request: &DiscoverRequest) -> CatalogDelta {
        let now_boot_ns = self.clock.now_boot_ns();
        let adapter_id = &self.manifest.adapter_id;
        if let Some(code) = rejected_discovery(request, adapter_id, self.shut_down, now_boot_ns) {
            return self.book.unavailable_page(request, code);
        }
        if request.page > 0 {
            return self.book.serve_page(request);
        }
        self.discover_snapshot(request, now_boot_ns)
    }

    fn read(&mut self, plan: &ReadPlan) -> ObservationBatch {
        let now_boot_ns = self.clock.now_boot_ns();
        if let Some(rejected) =
            rejected_plan(plan, &self.manifest.adapter_id, self.shut_down, now_boot_ns)
        {
            return rejected;
        }
        let target = plan_target(plan);
        if target.group != groups::CAPACITY {
            return RecordFactory::failed_batch(plan, now_boot_ns, "UNKNOWN_READ_GROUP");
        }
        self.read_capacity(plan, &target, now_boot_ns)
    }

    fn shutdown(&mut self) {
        self.shut_down = true;
        self.pagers.clear();
        self.book.clear_pending();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figures_multiply_with_overflow_detection() {
        let figures = CapacityFigures {
            blocks: 100,
            blocks_free: 40,
            blocks_available: 30,
            fragment_size: 4096,
        };
        assert_eq!(CapacityFigure::Total.bytes(&figures), Some(409_600));
        assert_eq!(CapacityFigure::Free.bytes(&figures), Some(163_840));
        assert_eq!(CapacityFigure::Available.bytes(&figures), Some(122_880));
        let huge = CapacityFigures {
            blocks: u64::MAX,
            ..figures
        };
        assert_eq!(CapacityFigure::Total.bytes(&huge), None);
    }
}
