//! NVML telemetry adapter (`nvml`; read groups `fast`, `slow_health`,
//! `process`, all pageable).
//!
//! * `NvmlAdapter::new` never fails: the library is opened lazily on the
//!   first `discover`. A failed open reports the adapter unavailable (or
//!   permission denied) with a reason and every capability unsupported;
//!   re-open attempts are throttled (default 60 s).
//! * Entities are `gpu:<UUID>`; values are native (mW, mJ, MHz, percent,
//!   degC, bytes, count). PCIe throughput keeps NVML's unproven "KB/s" as
//!   `KB_unverified_per_second`, which the collector rejects on purpose.
//! * Unsupported or denied queries yield `unsupported`/`permission_denied`
//!   records (never 0) and are re-probed at most once per probe interval.
//! * Metadata is cached per catalog generation; a GPU that is lost only turns
//!   its own records into `removed`.

mod backend;
mod capabilities;
mod catalog;
mod channels;
mod discovery;
mod inventory;
mod library;
mod processes;
mod stack;
mod telemetry;
mod topology;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub use backend::{
    BackendConnector, BackendError, BackendResult, C2cLinks, ClockDomain, ConnectError,
    DeviceDetails, EccCounterScope, EccErrorKind, FramebufferMemory, GpuBackend, GpuDeviceQueries,
    GpuInventory, GpuProcessEntry, GpuProcessQueries, GpuTelemetry, HealthValue, PcieDirection,
    PcieLink, ProcessUtilizationSample, RetiredPageCause, StaticFact, ThermalThreshold,
    ThrottleCause, UtilizationRates,
};
pub use capabilities::DEFAULT_UNSUPPORTED_PROBE_MS;
pub use catalog::{PROFILE_ID, SOURCE_LIMIT};
pub use library::{NvmlLibraryBackend, NvmlLibraryConnector};
pub use processes::{MAX_PROCESSES_PER_GPU, UsageKind};
pub use stack::{NoFiles, StackFiles, SystemFiles};
pub use telemetry::PCIE_UNVERIFIED_REASON;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{EntityId, HostId, IdentityError, ProfileId, SourceId};
use dgxtop_core::naming;
use dgxtop_core::observation::{
    AdapterHealth, AdapterManifest, CatalogDelta, DiscoverRequest, MAX_RECORDS_PER_BATCH,
    Observation, ObservationBatch, ReadPlan, split_page,
};
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::Availability;

use crate::common::{Pager, ProbeThrottle, RecordFactory, SlotInterner};

use capabilities::CapabilityTracker;
use catalog::{CatalogIdentity, PublishedSources, publish_sources};
use channels::ChannelGroup;
use discovery::{CatalogView, Tombstones, available_page, failed_page, unavailable_page};
use inventory::{Enumeration, Inventory};
use processes::{ProcessRead, ProcessSampler};
use stack::read_stack;
use telemetry::TelemetryRead;

/// Minimum spacing of library re-open attempts after a failure.
pub const MIN_REINIT_INTERVAL_MS: u64 = 60_000;
const INIT_PROBE_KEY: &str = "nvml.init";

/// Static manifest of the NVML adapter (`examples/adapter-manifest.nvml.json`).
pub fn manifest() -> AdapterManifest {
    serde_json::from_str(include_str!(
        "../../../../spec/dgxtop-v2/examples/adapter-manifest.nvml.json"
    ))
    .unwrap_or_else(|error| panic!("embedded NVML manifest is invalid: {error}"))
}

/// Tunable intervals (both clamped to their safe minimums).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvmlSettings {
    /// Re-probe interval of unsupported/denied capabilities (≥ 10 s).
    pub unsupported_probe_ms: u64,
    /// Re-open interval after a failed library open (≥ 60 s).
    pub reinit_interval_ms: u64,
}

impl Default for NvmlSettings {
    fn default() -> Self {
        Self {
            unsupported_probe_ms: DEFAULT_UNSUPPORTED_PROBE_MS,
            reinit_interval_ms: MIN_REINIT_INTERVAL_MS,
        }
    }
}

/// Library session state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Connection {
    NotAttempted,
    Connected,
    Failed {
        availability: Availability,
        reason: String,
    },
    ShutDown,
}

/// NVML telemetry adapter.
pub struct NvmlAdapter {
    clock: Arc<dyn TimeProvider>,
    host_id: HostId,
    manifest: AdapterManifest,
    connector: Box<dyn BackendConnector>,
    backend: Option<Box<dyn GpuBackend>>,
    connection: Connection,
    init_throttle: ProbeThrottle,
    inventory: Inventory,
    published: PublishedSources,
    interner: SlotInterner,
    tracker: CapabilityTracker,
    lost_devices: BTreeSet<String>,
    processes: ProcessSampler,
    /// One snapshot per read group, so interleaved groups never evict a
    /// paged snapshot that is still being served.
    pagers: BTreeMap<ChannelGroup, Pager>,
    tombstones: Tombstones,
    /// Where the kernel module and CUDA toolkits are read from.
    stack_files: Box<dyn StackFiles>,
}

impl NvmlAdapter {
    /// Production adapter on the system NVML library and the host's files;
    /// never fails.
    pub fn new(clock: Arc<dyn TimeProvider>, host_id: HostId) -> Self {
        Self::with_connector(clock, host_id, Box::new(NvmlLibraryConnector))
            .with_stack_files(Box::new(SystemFiles))
    }

    /// Reads the software stack through `files` (an injected file tree in
    /// tests; none by default).
    #[must_use]
    pub fn with_stack_files(mut self, files: Box<dyn StackFiles>) -> Self {
        self.stack_files = files;
        self
    }

    /// Adapter on an injected backend connector (fakes in tests).
    pub fn with_connector(
        clock: Arc<dyn TimeProvider>,
        host_id: HostId,
        connector: Box<dyn BackendConnector>,
    ) -> Self {
        Self::with_settings(clock, host_id, connector, NvmlSettings::default())
    }

    pub fn with_settings(
        clock: Arc<dyn TimeProvider>,
        host_id: HostId,
        connector: Box<dyn BackendConnector>,
        settings: NvmlSettings,
    ) -> Self {
        Self {
            clock,
            host_id,
            manifest: manifest(),
            connector,
            backend: None,
            connection: Connection::NotAttempted,
            init_throttle: ProbeThrottle::new(
                settings.reinit_interval_ms.max(MIN_REINIT_INTERVAL_MS),
            ),
            inventory: Inventory::new(),
            published: PublishedSources::default(),
            interner: SlotInterner::new(),
            tracker: CapabilityTracker::new(settings.unsupported_probe_ms.max(10_000)),
            lost_devices: BTreeSet::new(),
            processes: ProcessSampler::new(),
            pagers: BTreeMap::new(),
            tombstones: Tombstones::new(),
            stack_files: Box::new(NoFiles),
        }
    }

    fn identity(&self) -> Result<CatalogIdentity, IdentityError> {
        Ok(CatalogIdentity {
            host_id: self.host_id.clone(),
            boot_id: self.clock.boot_id(),
            adapter_id: self.manifest.adapter_id.clone(),
            profile_id: ProfileId::new(PROFILE_ID)?,
        })
    }

    /// Opens the library when needed and allowed by the re-open throttle.
    fn ensure_connected(&mut self, now_boot_ns: u64) {
        if self.backend.is_some() || self.connection == Connection::ShutDown {
            return;
        }
        if !self.init_throttle.should_probe(INIT_PROBE_KEY, now_boot_ns) {
            return;
        }
        match self.connector.connect() {
            Ok(backend) => {
                self.backend = Some(backend);
                self.connection = Connection::Connected;
            }
            Err(error) => {
                let availability = match error {
                    ConnectError::NoPermission => Availability::PermissionDenied,
                    _ => Availability::Unavailable,
                };
                self.connection = Connection::Failed {
                    availability,
                    reason: error.reason_code(),
                };
            }
        }
    }

    /// The library session died: forget devices, allow an immediate re-open.
    fn drop_session(&mut self, reason: &str) {
        self.backend = None;
        self.connection = Connection::Failed {
            availability: Availability::Unavailable,
            reason: reason.to_owned(),
        };
        self.init_throttle.reset();
        let removed = self.inventory.invalidate();
        self.forget_devices(&removed);
        self.published = PublishedSources::default();
        self.tracker.clear();
        self.processes.reset();
        self.lost_devices.clear();
    }

    fn forget_devices(&mut self, uuids: &[String]) {
        let mut sources: Vec<SourceId> = Vec::new();
        let mut entities: Vec<EntityId> = Vec::new();
        for uuid in uuids {
            sources.extend(self.published.source_ids_of(uuid));
            if let Ok(entity) = naming::entity::gpu(uuid) {
                entities.push(entity);
            }
        }
        self.tombstones.bury(sources, entities);
    }

    fn unavailable_reason(&self) -> (Availability, String) {
        match &self.connection {
            Connection::Failed {
                availability,
                reason,
            } => (*availability, reason.clone()),
            Connection::ShutDown => (Availability::Unavailable, "NVML_SHUT_DOWN".to_owned()),
            Connection::NotAttempted => {
                (Availability::Unavailable, "NVML_NOT_INITIALIZED".to_owned())
            }
            Connection::Connected => (Availability::Unavailable, "NVML_NO_GPU".to_owned()),
        }
    }

    fn discover_connected(&mut self, request: &DiscoverRequest, now: u64) -> CatalogDelta {
        let Some(backend) = self.backend.as_deref() else {
            return self.unavailable_delta(request);
        };
        let enumeration = match Enumeration::read(backend) {
            Ok(enumeration) => enumeration,
            Err(BackendError::Uninitialized) => {
                self.drop_session("NVML_SESSION_LOST");
                return self.unavailable_delta(request);
            }
            Err(error) => {
                // GPUs cannot be listed: keep the parent's catalog (incomplete
                // page) but do not claim the adapter is available.
                let availability = match error {
                    BackendError::NoPermission => Availability::PermissionDenied,
                    _ => Availability::Unavailable,
                };
                let reason = format!("NVML_ENUMERATION_FAILED:{}", error.reason_code());
                return failed_page(request, self.inventory.generation(), availability, &reason);
            }
        };
        let previous = self.published.clone();
        let stack = read_stack(self.stack_files.as_ref());
        let applied = self.inventory.apply(
            backend,
            (&enumeration, &stack),
            request.previous_generation,
            now,
            &mut self.tracker,
        );
        let change = match applied {
            Ok(change) => change,
            Err(_) => {
                self.drop_session("NVML_SESSION_LOST");
                return self.unavailable_delta(request);
            }
        };
        self.lost_devices.clear();
        if change.generation_changed
            && let Err(error) = self.republish(&previous, &change.removed_uuids)
        {
            return failed_page(
                request,
                self.inventory.generation(),
                Availability::Available,
                &error,
            );
        }
        if self.inventory.is_empty() {
            self.connection = Connection::Failed {
                availability: Availability::Unavailable,
                reason: "NVML_NO_GPU".to_owned(),
            };
            return self.unavailable_delta(request);
        }
        self.connection = Connection::Connected;
        self.catalog_page(request, discovery_warning(&enumeration, &self.published))
    }

    fn republish(&mut self, previous: &PublishedSources, removed: &[String]) -> Result<(), String> {
        let identity = self
            .identity()
            .map_err(|error| format!("NVML_IDENTITY:{error}"))?;
        self.published = publish_sources(&self.inventory, &identity, &mut self.interner)
            .map_err(|error| format!("NVML_CATALOG:{error}"))?;
        let mut stale: Vec<SourceId> = removed
            .iter()
            .flat_map(|uuid| previous.source_ids_of(uuid))
            .collect();
        let live_sources: BTreeSet<SourceId> = self
            .published
            .descriptors()
            .map(|descriptor| descriptor.source_id.clone())
            .collect();
        // Channels that vanished on a surviving device (e.g. fewer fans).
        stale.extend(
            previous
                .descriptors()
                .map(|descriptor| descriptor.source_id.clone())
                .filter(|source| !live_sources.contains(source)),
        );
        let removed_entities = removed
            .iter()
            .filter_map(|uuid| naming::entity::gpu(uuid).ok())
            .collect();
        self.tombstones.bury(stale, removed_entities);
        let live_entities: BTreeSet<EntityId> = self
            .inventory
            .devices()
            .filter_map(|device| naming::entity::gpu(&device.uuid).ok())
            .collect();
        self.tombstones.revive(&live_sources, &live_entities);
        self.processes.reset();
        Ok(())
    }

    fn catalog_page(&self, request: &DiscoverRequest, warning: Option<String>) -> CatalogDelta {
        let view = CatalogView {
            inventory: &self.inventory,
            published: &self.published,
            tracker: &self.tracker,
            tombstones: &self.tombstones,
        };
        available_page(&view, request, warning).unwrap_or_else(|error| {
            failed_page(
                request,
                self.inventory.generation(),
                Availability::Available,
                &format!("NVML_CATALOG:{error}"),
            )
        })
    }

    fn unavailable_delta(&self, request: &DiscoverRequest) -> CatalogDelta {
        let (availability, reason) = self.unavailable_reason();
        unavailable_page(
            request,
            self.inventory.generation().max(request.previous_generation),
            availability,
            &reason,
            &self.tombstones,
        )
    }

    fn snapshot(
        &mut self,
        plan: &ReadPlan,
        group: ChannelGroup,
    ) -> (Vec<Observation>, Option<String>) {
        let Some(backend) = self.backend.as_deref() else {
            return (Vec::new(), Some(self.unavailable_reason().1));
        };
        let requested: BTreeSet<u32> = plan.source_slots.iter().copied().collect();
        let (records, session_lost, partial) = match group {
            ChannelGroup::Fast | ChannelGroup::SlowHealth => {
                let mut read = TelemetryRead {
                    plan,
                    clock: self.clock.as_ref(),
                    published: &self.published,
                    tracker: &mut self.tracker,
                    lost_devices: &mut self.lost_devices,
                    requested_slots: &requested,
                };
                let outcome = read.read_group(backend, &mut self.inventory, group);
                (outcome.records, outcome.session_lost, false)
            }
            ChannelGroup::Process => {
                let boot_id = self.clock.boot_id();
                let mut read = ProcessRead {
                    plan,
                    clock: self.clock.as_ref(),
                    host_id: &self.host_id,
                    boot_id: &boot_id,
                    published: &self.published,
                    tracker: &mut self.tracker,
                    lost_devices: &mut self.lost_devices,
                    requested_slots: &requested,
                };
                let outcome = self.processes.read(backend, &self.inventory, &mut read);
                (outcome.records, outcome.session_lost, outcome.partial)
            }
        };
        if session_lost {
            self.drop_session("NVML_SESSION_LOST");
            return (records, Some("NVML_SESSION_LOST".to_owned()));
        }
        (
            records,
            partial.then(|| "NVML_PROCESS_LIST_PARTIAL".to_owned()),
        )
    }
}

fn discovery_warning(enumeration: &Enumeration, published: &PublishedSources) -> Option<String> {
    if published.truncated {
        Some("NVML_SOURCE_LIMIT".to_owned())
    } else if enumeration.truncated {
        Some("NVML_GPU_LIMIT".to_owned())
    } else if enumeration.unreadable_indices > 0 {
        Some(format!(
            "NVML_UNREADABLE_DEVICES:{}",
            enumeration.unreadable_indices
        ))
    } else {
        None
    }
}

impl TelemetryPort for NvmlAdapter {
    fn manifest(&self) -> &AdapterManifest {
        &self.manifest
    }

    fn discover(&mut self, request: &DiscoverRequest) -> CatalogDelta {
        let now = self.clock.now_boot_ns();
        if request.page > 0 {
            return if self.backend.is_some() && !self.inventory.is_empty() {
                self.catalog_page(request, None)
            } else {
                self.unavailable_delta(request)
            };
        }
        self.ensure_connected(now);
        if self.backend.is_none() {
            return self.unavailable_delta(request);
        }
        self.discover_connected(request, now)
    }

    fn read(&mut self, plan: &ReadPlan) -> ObservationBatch {
        let now = self.clock.now_boot_ns();
        if now >= plan.deadline_boot_ns {
            return RecordFactory::failed_batch(plan, now, "NVML_DEADLINE_EXPIRED");
        }
        let (base_group, suffix_page) = split_page(plan.read_group.as_str());
        let Some(group) = ChannelGroup::from_name(base_group) else {
            return RecordFactory::failed_batch(plan, now, "NVML_UNKNOWN_READ_GROUP");
        };
        let page = if plan.page > 0 {
            plan.page
        } else {
            suffix_page
        };
        if page == 0 {
            if self.backend.is_none() {
                let reason = self.unavailable_reason().1;
                return RecordFactory::failed_batch(plan, now, &reason);
            }
            let (records, error_code) = self.snapshot(plan, group);
            let finished = self.clock.now_boot_ns();
            self.pagers.entry(group).or_default().store(
                plan.cycle_id,
                group.name(),
                records,
                now,
                finished,
            );
            return self.page_batch(plan, group, 0, error_code);
        }
        self.page_batch(plan, group, page, None)
    }

    fn health(&self) -> AdapterHealth {
        if self.backend.is_some() && !self.inventory.is_empty() {
            return AdapterHealth {
                availability: Availability::Available,
                detail: None,
            };
        }
        let (availability, reason) = self.unavailable_reason();
        AdapterHealth {
            availability,
            detail: Some(reason),
        }
    }

    fn shutdown(&mut self) {
        self.backend = None;
        self.connection = Connection::ShutDown;
    }
}

impl NvmlAdapter {
    fn page_batch(
        &self,
        plan: &ReadPlan,
        group: ChannelGroup,
        page: u32,
        error_code: Option<String>,
    ) -> ObservationBatch {
        let served = self
            .pagers
            .get(&group)
            .and_then(|pager| pager.page(plan.cycle_id, group.name(), page, MAX_RECORDS_PER_BATCH));
        match served {
            Some(served) => RecordFactory::batch(
                plan,
                served.records,
                served.started_boot_ns,
                served.finished_boot_ns,
                served.continues,
                error_code,
            ),
            None => RecordFactory::failed_batch(
                plan,
                self.clock.now_boot_ns(),
                "NVML_PAGE_SNAPSHOT_MISSING",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_matches_spec_example() {
        let manifest = manifest();
        manifest.validate().unwrap();
        assert_eq!(manifest.adapter_id.as_str(), "nvml");
        assert_eq!(
            manifest.runtime_library.as_deref(),
            Some("libnvidia-ml.so.1")
        );
        assert_eq!(manifest.minimum_sample_ms, 1000);
        assert_eq!(manifest.source_limit, 2048);
        assert_eq!(
            manifest.sample_namespace,
            dgxtop_core::quality::Namespace::Live
        );
        assert_eq!(
            usize::try_from(manifest.source_limit).unwrap(),
            SOURCE_LIMIT
        );
    }

    #[test]
    fn settings_are_clamped_to_safe_minimums() {
        let clock = dgxtop_core::testing::FakeClock::new(0);
        let adapter = NvmlAdapter::with_settings(
            clock,
            dgxtop_core::testing::host_id("h"),
            Box::new(NvmlLibraryConnector),
            NvmlSettings {
                unsupported_probe_ms: 1,
                reinit_interval_ms: 1,
            },
        );
        assert_eq!(adapter.health().availability, Availability::Unavailable);
        assert_eq!(
            adapter.health().detail.as_deref(),
            Some("NVML_NOT_INITIALIZED")
        );
    }
}
