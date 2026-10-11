//! Procfs adapter (`procfs`): read groups `cpu` (`/proc/stat`,
//! `/proc/loadavg`, cpufreq), `memory` (`/proc/meminfo`), `disk`
//! (`/proc/diskstats`) and `process` (`/proc/<pid>`). It only reads files
//! under `/proc` and `/sys` (resolved under the configured root), returns
//! native units with provenance and never substitutes 0 for a missing value.
//!
//! The submodules `catalog`, `mechanics` and `records` are shared with the
//! network and filesystem adapters.

pub(crate) mod catalog;
pub(crate) mod mechanics;
pub(crate) mod records;

mod command;
mod cpu;
mod disk;
mod host;
mod locator;
mod memory;
mod parse;
mod probe;
mod process;

use std::io;
use std::sync::Arc;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{HostId, PidNamespace, ProfileId};
use dgxtop_core::naming::{adapters, groups};
use dgxtop_core::observation::{
    AdapterManifest, Capability, CatalogDelta, DiscoverRequest, ObservationBatch, ReadPlan,
};
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::{Availability, Support};

use self::catalog::{CatalogBook, CatalogConfig, KeyContext};
use self::cpu::{CpuDiscovery, CpuRead, CpufreqCache};
use self::disk::DiskScopeCache;
use self::host::{HostMetadata, PID_NAMESPACE_PATH, load_host_metadata};
use self::locator::ProcfsLocator;
use self::mechanics::{
    GroupPagers, PlanTarget, ReprobeGate, invariant, namespace_link, plan_target,
    rejected_discovery, rejected_plan,
};
use self::process::{MAX_SCANNED_PROCESSES, ProcessMemo, ProcessScan};
use crate::common::{RecordFactory, SystemFiles, read_status_of};
use crate::registry::{AdapterOptions, compile_time_adapter_id, kernel_file_manifest};

#[cfg(any(test, feature = "test-support"))]
pub use self::catalog::CatalogStats;
pub use self::host::HostConstants;
pub use self::probe::ProcfsProcessProbe;

/// Profile of the kernel-defined procfs fields.
pub const PROFILE_ID: &str = "linux-procfs";
pub const PROFILE_REVISION: u32 = 1;
const IMPLEMENTATION: &str = "ProcfsAdapter";
const MINIMUM_SAMPLE_MS: u32 = 100;

/// Static manifest with default options.
pub fn manifest() -> AdapterManifest {
    manifest_with(&AdapterOptions::default())
}

/// Manifest for `options` (source limit = min(max_sources, 2048)).
pub fn manifest_with(options: &AdapterOptions) -> AdapterManifest {
    kernel_file_manifest(adapters::PROCFS, IMPLEMENTATION, MINIMUM_SAMPLE_MS, options)
}

/// Read-only procfs telemetry adapter.
pub struct ProcfsAdapter {
    clock: Arc<dyn TimeProvider>,
    host_id: HostId,
    files: Arc<dyn SystemFiles>,
    options: AdapterOptions,
    manifest: AdapterManifest,
    constants: HostConstants,
    book: CatalogBook<ProcfsLocator>,
    metadata: Option<HostMetadata>,
    pid_namespace: Option<PidNamespace>,
    cpufreq: CpufreqCache,
    disk_scopes: DiskScopeCache,
    read_gate: ReprobeGate,
    pagers: GroupPagers,
    process_memo: ProcessMemo,
    shut_down: bool,
}

impl ProcfsAdapter {
    pub fn new(
        clock: Arc<dyn TimeProvider>,
        host_id: HostId,
        files: Arc<dyn SystemFiles>,
        options: AdapterOptions,
    ) -> Self {
        Self::with_host_constants(
            clock,
            host_id,
            files,
            options,
            HostConstants::from_sysconf(),
        )
    }

    /// Like [`ProcfsAdapter::new`] with explicit tick rate and page size
    /// (deterministic tests, foreign roots).
    pub fn with_host_constants(
        clock: Arc<dyn TimeProvider>,
        host_id: HostId,
        files: Arc<dyn SystemFiles>,
        options: AdapterOptions,
        constants: HostConstants,
    ) -> Self {
        let config = CatalogConfig {
            adapter_id: compile_time_adapter_id(adapters::PROCFS),
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
            constants,
            book: CatalogBook::new(config),
            metadata: None,
            pid_namespace: None,
            cpufreq: CpufreqCache::new(options.unsupported_probe_ms),
            disk_scopes: DiskScopeCache::default(),
            read_gate: ReprobeGate::new(options.unsupported_probe_ms),
            pagers: GroupPagers::default(),
            process_memo: ProcessMemo::default(),
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
        let metadata = self
            .metadata
            .get_or_insert_with(|| load_host_metadata(&*self.files))
            .clone();
        if self.pid_namespace.is_none() {
            self.pid_namespace = metadata
                .pid_namespace
                .as_deref()
                .and_then(|namespace| PidNamespace::new(namespace).ok());
        }
        let namespace_known = self.pid_namespace.is_some();
        let Self {
            book,
            files,
            clock,
            cpufreq,
            disk_scopes,
            constants,
            ..
        } = self;
        let mut builder = book.begin_snapshot(now_boot_ns);
        host::add_host(&mut builder, &keys, &metadata, constants);
        let discovery = CpuDiscovery {
            keys: &keys,
            files: &**files,
            clock: &**clock,
        };
        let cpu_result = cpu::discover(&mut builder, &discovery, cpufreq);
        // Memory and disk failures are carried over and reported in the
        // delta's error code; only /proc/stat decides availability.
        memory::discover(&mut builder, &keys, &**files);
        disk::discover(&mut builder, &keys, &**files, disk_scopes);
        builder.capability(process_capability(namespace_known));
        let availability = availability_of(cpu_result.as_ref().err());
        builder.finish(request, availability)
    }

    fn read_catalogued(
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
            let selection = book.select(&target.group, &plan.source_slots);
            let mut snapshot = match target.group.as_str() {
                groups::CPU => {
                    let context = CpuRead {
                        plan,
                        files: &**files,
                        clock: &**clock,
                    };
                    cpu::read(&context, &selection.sources, read_gate)
                }
                groups::MEMORY => memory::read(plan, &selection.sources, &**files, &**clock),
                _ => disk::read(plan, &selection.sources, &**files, &**clock),
            };
            if selection.unknown_slots > 0 {
                snapshot.final_error = Some("UNKNOWN_SOURCE_SLOT".to_owned());
            }
            snapshot
        })
    }

    fn read_processes(
        &mut self,
        plan: &ReadPlan,
        target: &PlanTarget,
        now_boot_ns: u64,
    ) -> ObservationBatch {
        if self.pid_namespace.is_none() {
            self.pid_namespace = namespace_link(&*self.files, PID_NAMESPACE_PATH)
                .and_then(|namespace| PidNamespace::new(namespace).ok());
        }
        let Some(namespace) = self.pid_namespace.clone() else {
            return RecordFactory::failed_batch(plan, now_boot_ns, "PID_NAMESPACE_UNKNOWN");
        };
        let keys = KeyContext {
            host_id: self.host_id.clone(),
            boot_id: self.clock.boot_id(),
        };
        let page_size = self.options.effective_page_size();
        let Self {
            pagers,
            book,
            files,
            clock,
            constants,
            process_memo,
            ..
        } = self;
        pagers.serve(plan, target, page_size, now_boot_ns, || {
            process::scan(
                &ProcessScan {
                    plan,
                    files: &**files,
                    clock: &**clock,
                    config: book.config(),
                    keys: &keys,
                    pid_namespace: &namespace,
                    page_size: constants.page_size,
                },
                process_memo,
            )
        })
    }
}

/// Availability from the essential enumeration file's read outcome.
pub(crate) fn availability_of(error: Option<&io::Error>) -> Availability {
    match error.map(read_status_of) {
        None => Availability::Available,
        Some(dgxtop_core::quality::ReadStatus::PermissionDenied) => Availability::PermissionDenied,
        Some(_) => Availability::Unavailable,
    }
}

fn process_capability(namespace_known: bool) -> Capability {
    let (support, reason) = if namespace_known {
        (
            Support::Supported,
            format!("MAX_PROCESSES:{MAX_SCANNED_PROCESSES}"),
        )
    } else {
        (Support::Unknown, "PID_NAMESPACE_UNKNOWN".to_owned())
    };
    Capability {
        capability_id: "process.scan".to_owned(),
        entity_id: None,
        support,
        reason: Some(reason),
        min_interval_ms: None,
    }
}

impl TelemetryPort for ProcfsAdapter {
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
        match target.group.as_str() {
            groups::CPU | groups::MEMORY | groups::DISK => {
                self.read_catalogued(plan, &target, now_boot_ns)
            }
            groups::PROCESS => self.read_processes(plan, &target, now_boot_ns),
            _ => RecordFactory::failed_batch(plan, now_boot_ns, "UNKNOWN_READ_GROUP"),
        }
    }

    fn shutdown(&mut self) {
        self.shut_down = true;
        self.pagers.clear();
        self.book.clear_pending();
    }
}
