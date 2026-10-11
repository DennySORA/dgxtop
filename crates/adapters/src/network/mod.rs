//! Network adapter (`network`, read group `fast`, paged): per-interface
//! kernel counters from `/sys/class/net/<if>/statistics/*` on entity
//! `netif:<netns>:<ifindex>` (netns from `/proc/self/ns/net`), scope
//! `interface`. Loopback is included and marked by the `loopback` attribute.
//! A removed interface is removed from the catalog; a re-created one (new
//! ifindex, even under the same name) is a new entity and generation, so a
//! counter base is never continued.

mod interfaces;

use std::collections::BTreeMap;
use std::sync::Arc;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{HostId, ProfileId};
use dgxtop_core::naming::{adapters, groups};
use dgxtop_core::observation::{
    AdapterManifest, Capability, CatalogDelta, DiscoverRequest, Observation, ObservationBatch,
    ReadPlan, SourceDescriptor,
};
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::{Availability, Support};

use self::interfaces::{
    FAMILY_INTERFACE, InterfaceDiscovery, InterfaceRead, NET_CLASS, NetLocator, add_interface,
    read_interface,
};
use crate::common::{RecordFactory, SystemFiles};
#[cfg(any(test, feature = "test-support"))]
use crate::procfs::CatalogStats;
use crate::procfs::availability_of;
use crate::procfs::catalog::{
    CatalogBook, CatalogConfig, KeyContext, LiveSource, Selected, SnapshotBuilder,
};
use crate::procfs::mechanics::{
    GroupPagers, PlanTarget, ReadSnapshot, ReprobeGate, invariant, io_failure, namespace_link,
    plan_target, rejected_discovery, rejected_plan,
};
use crate::procfs::records::removed_record;
use crate::registry::{AdapterOptions, compile_time_adapter_id, kernel_file_manifest};

pub use self::interfaces::LOOPBACK_ATTRIBUTE;

/// Profile of the kernel-defined sysfs network statistics.
pub const PROFILE_ID: &str = "linux-sysfs-net";
pub const PROFILE_REVISION: u32 = 1;
const IMPLEMENTATION: &str = "NetworkAdapter";
const MINIMUM_SAMPLE_MS: u32 = 100;
const NET_NAMESPACE_PATH: &str = "/proc/self/ns/net";

/// Static manifest with default options.
pub fn manifest() -> AdapterManifest {
    manifest_with(&AdapterOptions::default())
}

/// Manifest for `options` (source limit = min(max_sources, 2048)).
pub fn manifest_with(options: &AdapterOptions) -> AdapterManifest {
    kernel_file_manifest(
        adapters::NETWORK,
        IMPLEMENTATION,
        MINIMUM_SAMPLE_MS,
        options,
    )
}

/// Read-only network interface telemetry adapter.
pub struct NetworkAdapter {
    clock: Arc<dyn TimeProvider>,
    host_id: HostId,
    files: Arc<dyn SystemFiles>,
    options: AdapterOptions,
    manifest: AdapterManifest,
    book: CatalogBook<NetLocator>,
    net_namespace: Option<String>,
    read_gate: ReprobeGate,
    pagers: GroupPagers,
    shut_down: bool,
}

impl NetworkAdapter {
    pub fn new(
        clock: Arc<dyn TimeProvider>,
        host_id: HostId,
        files: Arc<dyn SystemFiles>,
        options: AdapterOptions,
    ) -> Self {
        let config = CatalogConfig {
            adapter_id: compile_time_adapter_id(adapters::NETWORK),
            profile_id: invariant(ProfileId::new(PROFILE_ID), PROFILE_ID),
            profile_revision: PROFILE_REVISION,
            source_limit: usize::try_from(options.source_limit()).unwrap_or(usize::MAX),
            page_size: options.effective_page_size(),
        };
        Self {
            clock,
            host_id,
            files,
            manifest: manifest_with(&options),
            book: CatalogBook::new(config),
            net_namespace: None,
            read_gate: ReprobeGate::new(options.unsupported_probe_ms),
            pagers: GroupPagers::default(),
            shut_down: false,
            options,
        }
    }

    /// Size of the catalog bookkeeping (boundedness tests).
    #[cfg(any(test, feature = "test-support"))]
    pub fn catalog_stats(&self) -> CatalogStats {
        self.book.stats()
    }

    fn discover_snapshot(&mut self, request: &DiscoverRequest, now_boot_ns: u64) -> CatalogDelta {
        let keys = KeyContext {
            host_id: self.host_id.clone(),
            boot_id: self.clock.boot_id(),
        };
        if self.net_namespace.is_none() {
            self.net_namespace = namespace_link(&*self.files, NET_NAMESPACE_PATH);
        }
        let Self {
            book,
            files,
            net_namespace,
            ..
        } = self;
        let mut builder = book.begin_snapshot(now_boot_ns);
        let availability = match net_namespace.as_deref() {
            Some(namespace) => {
                let discovery = InterfaceDiscovery {
                    keys: &keys,
                    files: &**files,
                    net_namespace: namespace,
                };
                enumerate(&mut builder, &discovery)
            }
            None => {
                builder.carry_over_family(FAMILY_INTERFACE);
                builder.error("NET_NAMESPACE_UNKNOWN");
                Availability::Unavailable
            }
        };
        builder.capability(statistics_capability(availability));
        builder.finish(request, availability)
    }

    fn read_fast(
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
            read_gate,
            ..
        } = self;
        pagers.serve(plan, target, page_size, now_boot_ns, || {
            let selection = book.select(groups::FAST, &plan.source_slots);
            let context = InterfaceRead {
                plan,
                files: &**files,
                clock: &**clock,
            };
            let mut snapshot = read_selection(&context, &selection.sources, read_gate);
            if selection.unknown_slots > 0 {
                snapshot.final_error = Some("UNKNOWN_SOURCE_SLOT".to_owned());
            }
            snapshot
        })
    }
}

/// Adds every `/sys/class/net` entry; an unreadable class directory carries
/// the previous interfaces over.
fn enumerate(
    builder: &mut SnapshotBuilder<'_, NetLocator>,
    discovery: &InterfaceDiscovery<'_>,
) -> Availability {
    match discovery.files.read_dir(NET_CLASS) {
        Ok(entries) => {
            for entry in entries {
                add_interface(builder, discovery, &entry.name);
            }
            Availability::Available
        }
        Err(error) => {
            builder.carry_over_family(FAMILY_INTERFACE);
            builder.error(format!("SYSFS_NET_UNREADABLE:{}", io_failure(&error).1));
            availability_of(Some(&error))
        }
    }
}

fn statistics_capability(availability: Availability) -> Capability {
    let support = if availability == Availability::Available {
        Support::Supported
    } else {
        Support::Unknown
    };
    Capability {
        capability_id: "network.statistics".to_owned(),
        entity_id: None,
        support,
        reason: None,
        min_interval_ms: None,
    }
}

/// Selected live counters grouped per interface (first-seen order) and the
/// sources the parent still holds although their interface is gone.
struct GroupedSelection<'a> {
    interfaces: Vec<Vec<&'a LiveSource<NetLocator>>>,
    removed: Vec<&'a SourceDescriptor>,
}

fn group_by_interface<'a>(selection: &[Selected<'a, NetLocator>]) -> GroupedSelection<'a> {
    let mut index: BTreeMap<(&'a str, u32), usize> = BTreeMap::new();
    let mut grouped = GroupedSelection {
        interfaces: Vec::new(),
        removed: Vec::new(),
    };
    for selected in selection {
        match selected {
            Selected::Live(source) => {
                let key = (source.locator.ifname.as_str(), source.locator.ifindex);
                let position = *index.entry(key).or_insert_with(|| {
                    grouped.interfaces.push(Vec::new());
                    grouped.interfaces.len() - 1
                });
                grouped.interfaces[position].push(*source);
            }
            Selected::Removed(descriptor) => grouped.removed.push(*descriptor),
        }
    }
    grouped
}

fn read_selection(
    context: &InterfaceRead<'_>,
    selection: &[Selected<'_, NetLocator>],
    gate: &mut ReprobeGate,
) -> ReadSnapshot {
    let started_boot_ns = context.clock.now_boot_ns();
    let grouped = group_by_interface(selection);
    let mut records: Vec<Observation> = Vec::with_capacity(selection.len());
    for interface in &grouped.interfaces {
        records.extend(read_interface(context, interface, gate));
    }
    let now = context.clock.now_boot_ns();
    records.extend(
        grouped
            .removed
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

impl TelemetryPort for NetworkAdapter {
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
        if target.group != groups::FAST {
            return RecordFactory::failed_batch(plan, now_boot_ns, "UNKNOWN_READ_GROUP");
        }
        self.read_fast(plan, &target, now_boot_ns)
    }

    fn shutdown(&mut self) {
        self.shut_down = true;
        self.pagers.clear();
        self.book.clear_pending();
    }
}
