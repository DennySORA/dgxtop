//! Deterministic fixtures for collector tests: plans, policies, catalogs and
//! observation shortcuts. Nothing here touches real drivers or files.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use dgxtop_core::canonical::{
    CanonicalValue, CatalogSnapshot, CollectionBatch, CollectionLimits, CollectionPlan,
    CollectorState, CommittedSamples, PolicySnapshot,
};
use dgxtop_core::identity::{
    AdapterId, CollectorId, EntityId, MetricKey, OriginIdentity, ProfileId, ReadGroup, SourceId,
};
use dgxtop_core::metric::NativeUnit;
use dgxtop_core::observation::{
    Attribute, BatchStatus, EntityKind, EntityRecord, Observation, SourceDescriptor,
};
use dgxtop_core::ports::{Collector, IngressBatch};
use dgxtop_core::quality::{Namespace, RawKind, SampleKind};
use dgxtop_core::testing::{ObservationBuilder, batch_of, boot_id, host_id};
use uuid::Uuid;

pub(crate) const SECOND: u64 = 1_000_000_000;
pub(crate) const MILLISECOND: u64 = 1_000_000;

/// Default policy (config defaults: 3 wins, 5 s hold, 8 °C, 250 ms, factor 3).
pub(crate) fn policy() -> PolicySnapshot {
    PolicySnapshot {
        revision: 1,
        switch_successes: 3,
        switch_min_hold_ms: 5000,
        temperature_disagreement_deg_c: 8.0,
        relative_tolerance_percent: 10.0,
        alignment_tolerance_ms: 250,
        excluded_sources: BTreeSet::new(),
        pinned_sources: BTreeMap::new(),
        stale_factor: 3,
        effective_periods_ms: BTreeMap::new(),
        namespace: Namespace::Live,
    }
}

pub(crate) fn replay_policy() -> PolicySnapshot {
    PolicySnapshot {
        namespace: Namespace::Replay,
        ..policy()
    }
}

pub(crate) fn limits() -> CollectionLimits {
    CollectionLimits {
        max_series: 512,
        max_sources: 2048,
        max_candidates_per_series: 4,
        max_processes: 256,
        max_process_gpu_links: 1024,
    }
}

/// Observation builder preset for `key` from `adapter`/`source`, read at `at`.
pub(crate) fn reading(key: &MetricKey, adapter: &str, source: &str, at: u64) -> ObservationBuilder {
    ObservationBuilder::new(key.clone(), adapter, source).read_at(at)
}

pub(crate) fn collector_id(name: &str) -> CollectorId {
    CollectorId::new(name).unwrap_or_else(|error| panic!("{error}"))
}

/// Builder of one collection plan.
pub(crate) struct PlanBuilder {
    collector: String,
    cycle: u64,
    now: u64,
    policy: PolicySnapshot,
    catalog: CatalogSnapshot,
    state: CollectorState,
    state_revision: u64,
    inputs: Vec<Arc<IngressBatch>>,
    limits: CollectionLimits,
    committed: CommittedSamples,
}

impl PlanBuilder {
    pub fn new(collector: &str) -> Self {
        Self {
            collector: collector.to_owned(),
            cycle: 1,
            now: SECOND,
            policy: policy(),
            catalog: CatalogSnapshot {
                revision: 1,
                host_id: Some(host_id("test-host")),
                boot_id: Some(boot_id("fake-boot")),
                ..CatalogSnapshot::default()
            },
            state: CollectorState::default(),
            state_revision: 0,
            inputs: Vec::new(),
            limits: limits(),
            committed: CommittedSamples::default(),
        }
    }

    pub fn at(mut self, now: u64) -> Self {
        self.now = now;
        self
    }

    pub fn cycle(mut self, cycle: u64) -> Self {
        self.cycle = cycle;
        self
    }

    pub fn policy(mut self, policy: PolicySnapshot) -> Self {
        self.policy = policy;
        self
    }

    pub fn catalog(mut self, catalog: CatalogSnapshot) -> Self {
        self.catalog = catalog;
        self
    }

    /// Continues from the state an earlier (acknowledged) batch proposed.
    pub fn after(mut self, previous: &CollectionBatch) -> Self {
        self.state = previous.next_state.clone();
        self.state_revision = previous.next_state.revision;
        self
    }

    pub fn state(mut self, state: CollectorState) -> Self {
        self.state = state;
        self
    }

    pub fn limits(mut self, limits: CollectionLimits) -> Self {
        self.limits = limits;
        self
    }

    /// What the latest snapshot holds of other domains' metrics (D-106).
    pub fn committed(mut self, committed: CommittedSamples) -> Self {
        self.committed = committed;
        self
    }

    pub fn batch(mut self, adapter: &str, group: &str, records: Vec<Observation>) -> Self {
        let batch = batch_of(adapter, group, self.cycle, records);
        self.inputs.push(Arc::new(IngressBatch::new(batch, None)));
        self
    }

    /// A batch with an explicit completion status and error code.
    pub fn batch_with_status(
        mut self,
        adapter: &str,
        group: &str,
        records: Vec<Observation>,
        status: BatchStatus,
        error_code: Option<&str>,
    ) -> Self {
        let mut batch = batch_of(adapter, group, self.cycle, records);
        batch.status = status;
        batch.error_code = error_code.map(str::to_owned);
        self.inputs.push(Arc::new(IngressBatch::new(batch, None)));
        self
    }

    pub fn build(self) -> CollectionPlan {
        CollectionPlan {
            collector_id: collector_id(&self.collector),
            batch_id: Uuid::new_v4(),
            cycle_id: self.cycle,
            epoch: 1,
            config_revision: 1,
            host_id: host_id("test-host"),
            boot_id: boot_id("fake-boot"),
            catalog: Arc::new(self.catalog),
            policy: Arc::new(self.policy),
            state: Arc::new(self.state),
            state_revision: self.state_revision,
            inputs: self.inputs,
            now_boot_ns: self.now,
            limits: self.limits,
            committed: Arc::new(self.committed),
        }
    }
}

/// Catalog source descriptor with neutral defaults.
pub(crate) struct SourceSpec<'a> {
    pub source: &'a str,
    pub adapter: &'a str,
    pub key: Option<MetricKey>,
    pub read_group: &'a str,
    pub rank: u16,
    pub raw_kind: RawKind,
    pub native_unit: &'a str,
    pub modulus: Option<u64>,
    pub origin: OriginIdentity,
}

impl<'a> SourceSpec<'a> {
    pub fn new(source: &'a str, adapter: &'a str, key: &MetricKey, read_group: &'a str) -> Self {
        Self {
            source,
            adapter,
            key: Some(key.clone()),
            read_group,
            rank: 0,
            raw_kind: RawKind::Gauge,
            native_unit: "degC",
            modulus: None,
            origin: OriginIdentity::unknown(),
        }
    }

    pub fn descriptor(&self) -> SourceDescriptor {
        let entity = self
            .key
            .as_ref()
            .map_or_else(|| entity("sensor:test"), |key| key.entity_id.clone());
        SourceDescriptor {
            source_id: SourceId::new(self.source).unwrap_or_else(|error| panic!("{error}")),
            adapter_id: AdapterId::new(self.adapter).unwrap_or_else(|error| panic!("{error}")),
            slot: 0,
            generation: 1,
            entity_id: entity,
            device_id: "device".to_owned(),
            channel: "channel".to_owned(),
            metric: self.key.clone(),
            origin: self.origin.clone(),
            driver: "test-driver".to_owned(),
            api_version: None,
            device_path: "test:///device".to_owned(),
            native_unit: NativeUnit::new(self.native_unit)
                .unwrap_or_else(|error| panic!("{error}")),
            raw_kind: self.raw_kind,
            sample_kind: SampleKind::Measured,
            read_group: ReadGroup::new(self.read_group).unwrap_or_else(|error| panic!("{error}")),
            update_interval_ms: None,
            profile_id: ProfileId::new("test-profile").unwrap_or_else(|error| panic!("{error}")),
            profile_revision: 1,
            profile_rank: self.rank,
            label: None,
            counter_modulus: self.modulus,
        }
    }
}

pub(crate) fn entity(value: &str) -> EntityId {
    EntityId::new(value).unwrap_or_else(|error| panic!("{error}"))
}

/// Catalog with the given sources and entities.
pub(crate) fn catalog(sources: &[SourceSpec<'_>], entities: Vec<EntityRecord>) -> CatalogSnapshot {
    let mut snapshot = CatalogSnapshot {
        revision: 1,
        host_id: Some(host_id("test-host")),
        boot_id: Some(boot_id("fake-boot")),
        ..CatalogSnapshot::default()
    };
    for spec in sources {
        let descriptor = spec.descriptor();
        snapshot
            .sources
            .insert(descriptor.source_id.clone(), Arc::new(descriptor));
    }
    for record in entities {
        snapshot
            .entities
            .insert(record.entity_id.clone(), Arc::new(record));
    }
    snapshot.regroup();
    snapshot
}

/// Host entity publishing `clk_tck`.
pub(crate) fn host_entity(clk_tck: &str) -> EntityRecord {
    EntityRecord {
        entity_id: entity(dgxtop_core::naming::entity::HOST),
        kind: EntityKind::Host,
        parent_id: None,
        generation: 1,
        discovered_boot_ns: 0,
        removed_boot_ns: None,
        display_name: "host".to_owned(),
        attributes: vec![Attribute {
            key: dgxtop_core::naming::attributes::CLK_TCK.to_owned(),
            value: clk_tck.to_owned(),
        }],
    }
}

/// Canonical value of `metric_id` (first match) in a batch.
pub(crate) fn value_of<'b>(
    batch: &'b CollectionBatch,
    key: &MetricKey,
) -> Option<&'b CanonicalValue> {
    batch.canonical.iter().find(|value| value.key == *key)
}

/// `f64` view of a published value.
pub(crate) fn number(batch: &CollectionBatch, key: &MetricKey) -> Option<f64> {
    value_of(batch, key)
        .and_then(|value| value.value)
        .map(dgxtop_core::metric::Scalar::to_f64_lossy)
}

/// Runs `collector` and checks the invariants every batch must satisfy.
pub(crate) fn collect(collector: &dyn Collector, plan: &CollectionPlan) -> CollectionBatch {
    let before = (*plan.state).clone();
    let batch = collector.collect(plan);
    assert_eq!(*plan.state, before, "the plan state must never be mutated");
    batch
        .validate()
        .unwrap_or_else(|error| panic!("invalid canonical value: {error}"));
    assert_eq!(batch.batch_id, plan.batch_id);
    assert_eq!(batch.state_base_revision, plan.state_revision);
    assert_eq!(batch.inputs.len(), plan.inputs.len());
    for (output, input) in batch.inputs.iter().zip(&plan.inputs) {
        assert!(Arc::ptr_eq(output, input), "inputs are shared, not copied");
    }
    batch
}

/// Asserts that `left` and `right` agree within `1e-9`.
pub(crate) fn assert_close(left: f64, right: f64) {
    assert!((left - right).abs() < 1e-9, "{left} != {right}");
}
