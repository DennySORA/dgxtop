//! Generic sysfs telemetry adapter shared by hwmon, thermal and powercap.
//!
//! A driver-specific [`Scanner`] finds devices, channels, static metadata and
//! the profile mapping; [`SysfsAdapter`] owns everything else: catalog
//! identity ([`SourceTable`]), delta publication ([`CatalogPublisher`]),
//! capability probing, the paged read path of each read group the adapter
//! serves (`naming::READ_GROUPS`: `fast`, and hwmon's `disk`) and health.

use std::collections::BTreeSet;
use std::sync::Arc;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{BootId, EntityId, HostId, MetricId, MetricKey, Scope};
use dgxtop_core::naming;
use dgxtop_core::observation::{
    AdapterManifest, CatalogDelta, DiscoverRequest, MAX_RECORDS_PER_BATCH, Observation,
    ObservationBatch, ReadPlan, split_page,
};
use dgxtop_core::quality::{Availability, Quality, ReadStatus};

use super::catalog::CatalogPublisher;
use super::reader::{ReadFailure, Reading, reasons};
use super::table::{SOURCE_REMOVED, ScanOutput, SourceEntry, SourceTable};
use crate::common::{Pager, RecordFactory, SystemFiles};
use crate::registry::AdapterOptions;

/// Batch/discovery error codes of the sysfs adapters.
pub(crate) mod codes {
    pub const ADAPTER_SHUT_DOWN: &str = "ADAPTER_SHUT_DOWN";
    pub const ADAPTER_ID_MISMATCH: &str = "ADAPTER_ID_MISMATCH";
    pub const UNKNOWN_READ_GROUP: &str = "UNKNOWN_READ_GROUP";
    pub const PAGE_SNAPSHOT_MISSING: &str = "PAGE_SNAPSHOT_MISSING";
    pub const DEADLINE_EXPIRED: &str = "DEADLINE_EXPIRED";
    pub const UNKNOWN_SOURCE_SLOT: &str = "UNKNOWN_SOURCE_SLOT";
}

/// Inputs of one scan.
pub(crate) struct ScanContext<'a> {
    pub files: &'a dyn SystemFiles,
    pub clock: &'a dyn TimeProvider,
    pub host_id: &'a HostId,
    pub boot_id: &'a BootId,
    pub deadline_boot_ns: u64,
    /// Device keys whose cached static metadata must be re-read.
    pub invalidated: &'a BTreeSet<String>,
}

impl ScanContext<'_> {
    /// Whether the discovery deadline has passed.
    pub(crate) fn expired(&self) -> bool {
        self.clock.now_boot_ns() > self.deadline_boot_ns
    }

    /// MetricKey on this host/boot; `None` when an identity part is invalid.
    pub(crate) fn metric_key(
        &self,
        entity_id: &EntityId,
        metric_id: &str,
        scope: &str,
    ) -> Option<MetricKey> {
        Some(MetricKey::new(
            self.host_id.clone(),
            self.boot_id.clone(),
            entity_id.clone(),
            MetricId::new(metric_id).ok()?,
            Scope::new(scope).ok()?,
        ))
    }
}

/// Driver-specific discovery of devices, channels and their profile mapping.
pub(crate) trait Scanner: Send {
    fn scan(&mut self, context: &ScanContext<'_>) -> ScanOutput;
}

/// Telemetry adapter over one sysfs driver class.
pub(crate) struct SysfsAdapter<S> {
    manifest: AdapterManifest,
    files: Arc<dyn SystemFiles>,
    clock: Arc<dyn TimeProvider>,
    host_id: HostId,
    page_size: usize,
    /// The read groups scheduled for this adapter.
    read_groups: Vec<&'static str>,
    scanner: S,
    table: SourceTable,
    publisher: CatalogPublisher,
    pager: Pager,
    discovered: bool,
    shut_down: bool,
}

impl<S: Scanner> SysfsAdapter<S> {
    pub(crate) fn new(
        manifest: AdapterManifest,
        clock: Arc<dyn TimeProvider>,
        host_id: HostId,
        files: Arc<dyn SystemFiles>,
        options: &AdapterOptions,
        scanner: S,
    ) -> Self {
        let page_size = options.page_size.clamp(1, MAX_RECORDS_PER_BATCH);
        let read_groups = naming::read_groups_of(manifest.adapter_id.as_str())
            .map(|spec| spec.group)
            .collect();
        Self {
            table: SourceTable::new(manifest.adapter_id.clone(), options),
            read_groups,
            publisher: CatalogPublisher::new(manifest.adapter_id.clone(), page_size),
            manifest,
            files,
            clock,
            host_id,
            page_size,
            scanner,
            pager: Pager::new(),
            discovered: false,
            shut_down: false,
        }
    }

    pub(crate) fn manifest(&self) -> &AdapterManifest {
        &self.manifest
    }

    pub(crate) fn discover(&mut self, request: &DiscoverRequest) -> CatalogDelta {
        let delta = self.discover_page(request);
        let publisher = &self.publisher;
        self.table
            .prune_departed(|source_id| publisher.holds(source_id));
        delta
    }

    fn discover_page(&mut self, request: &DiscoverRequest) -> CatalogDelta {
        if self.shut_down {
            return self.publisher.failure(
                request,
                codes::ADAPTER_SHUT_DOWN,
                Some(Availability::Unavailable),
            );
        }
        if request.adapter_id != self.manifest.adapter_id {
            return self
                .publisher
                .failure(request, codes::ADAPTER_ID_MISMATCH, None);
        }
        if request.page > 0 {
            return self.publisher.next(request);
        }
        let now_boot_ns = self.clock.now_boot_ns();
        if now_boot_ns > request.deadline_boot_ns {
            return self
                .publisher
                .failure(request, codes::DEADLINE_EXPIRED, None);
        }
        let boot_id = self.clock.boot_id();
        let scan = {
            let context = ScanContext {
                files: &*self.files,
                clock: &*self.clock,
                host_id: &self.host_id,
                boot_id: &boot_id,
                deadline_boot_ns: request.deadline_boot_ns,
                invalidated: self.table.suspects(),
            };
            self.scanner.scan(&context)
        };
        match self.table.apply(scan, now_boot_ns) {
            Err(code) => {
                let availability = self.discovered.then(|| self.table.availability());
                self.publisher.failure(request, &code, availability)
            }
            Ok(error_code) => {
                let clock = &self.clock;
                self.table.probe_capabilities(
                    &*self.files,
                    || clock.now_boot_ns(),
                    request.deadline_boot_ns,
                );
                self.discovered = true;
                let content = self.table.content(error_code);
                self.publisher.begin(request, content)
            }
        }
    }

    pub(crate) fn read(&mut self, plan: &ReadPlan) -> ObservationBatch {
        let now_boot_ns = self.clock.now_boot_ns();
        if self.shut_down {
            return RecordFactory::failed_batch(plan, now_boot_ns, codes::ADAPTER_SHUT_DOWN);
        }
        if plan.adapter_id != self.manifest.adapter_id {
            return RecordFactory::failed_batch(plan, now_boot_ns, codes::ADAPTER_ID_MISMATCH);
        }
        let (base_group, encoded_page) = split_page(plan.read_group.as_str());
        let page = if plan.page > 0 {
            plan.page
        } else {
            encoded_page
        };
        if !self.read_groups.contains(&base_group) {
            return RecordFactory::failed_batch(plan, now_boot_ns, codes::UNKNOWN_READ_GROUP);
        }
        if page > 0 {
            return self.serve_page(plan, base_group, page, None);
        }
        if now_boot_ns > plan.deadline_boot_ns {
            return RecordFactory::failed_batch(plan, now_boot_ns, codes::DEADLINE_EXPIRED);
        }
        let publisher = &self.publisher;
        let targets = self
            .table
            .targets(base_group, &plan.source_slots, |source_id| {
                publisher.holds(source_id)
            });
        let mut records = Vec::with_capacity(targets.current.len() + targets.departed.len());
        for source_id in &targets.current {
            let started = self.clock.now_boot_ns();
            let outcome = if started > plan.deadline_boot_ns {
                Err(ReadFailure::new(
                    ReadStatus::Timeout,
                    reasons::DEADLINE_EXCEEDED,
                ))
            } else {
                self.table.read_one(&*self.files, source_id, started)
            };
            let finished = self.clock.now_boot_ns();
            if let Some(entry) = self.table.entry(source_id) {
                records.push(record(plan, entry, outcome, started, finished));
            }
        }
        for source_id in &targets.departed {
            let now = self.clock.now_boot_ns();
            if let Some(entry) = self.table.entry(source_id) {
                let removed = Err(ReadFailure::new(ReadStatus::Removed, SOURCE_REMOVED));
                records.push(record(plan, entry, removed, now, now));
            }
        }
        let started = records
            .first()
            .map_or(now_boot_ns, |first| first.read_started_boot_ns);
        let finished = records
            .last()
            .map_or(now_boot_ns, |last| last.read_finished_boot_ns)
            .max(started);
        self.pager
            .store(plan.cycle_id, base_group, records, started, finished);
        let error_code =
            (!targets.unknown_slots.is_empty()).then(|| codes::UNKNOWN_SOURCE_SLOT.to_owned());
        self.serve_page(plan, base_group, 0, error_code)
    }

    fn serve_page(
        &self,
        plan: &ReadPlan,
        base_group: &str,
        page: u32,
        error_code: Option<String>,
    ) -> ObservationBatch {
        match self
            .pager
            .page(plan.cycle_id, base_group, page, self.page_size)
        {
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
                codes::PAGE_SNAPSHOT_MISSING,
            ),
        }
    }

    pub(crate) fn shutdown(&mut self) {
        self.shut_down = true;
        self.pager = Pager::new();
    }

    /// Number of catalogued sources (diagnostics/tests).
    pub(crate) fn source_count(&self) -> usize {
        self.table.source_count()
    }
}

/// Builds the observation of one source read.
fn record(
    plan: &ReadPlan,
    entry: &SourceEntry,
    outcome: Result<Reading, ReadFailure>,
    started: u64,
    finished: u64,
) -> Observation {
    match outcome {
        Ok(reading) => {
            let quality = if reading.faulted {
                Quality::Invalid
            } else {
                entry.quality
            };
            RecordFactory::ok(
                plan,
                &entry.descriptor,
                entry.record_key.clone(),
                reading.value,
                quality,
                started,
                finished,
                reading.reasons,
            )
        }
        Err(failure) => RecordFactory::failed(
            plan,
            &entry.descriptor,
            entry.record_key.clone(),
            failure.status,
            started,
            finished,
            failure.reason,
        ),
    }
}

/// Implements `TelemetryPort` for a newtype wrapping `SysfsAdapter` in `inner`.
macro_rules! sysfs_telemetry_port {
    ($adapter:ty) => {
        impl dgxtop_core::ports::TelemetryPort for $adapter {
            fn manifest(&self) -> &dgxtop_core::observation::AdapterManifest {
                self.inner.manifest()
            }

            fn discover(
                &mut self,
                request: &dgxtop_core::observation::DiscoverRequest,
            ) -> dgxtop_core::observation::CatalogDelta {
                self.inner.discover(request)
            }

            fn read(
                &mut self,
                plan: &dgxtop_core::observation::ReadPlan,
            ) -> dgxtop_core::observation::ObservationBatch {
                self.inner.read(plan)
            }

            fn shutdown(&mut self) {
                self.inner.shutdown()
            }
        }
    };
}

pub(crate) use sysfs_telemetry_port;
