//! Replay pipeline suite: the shipped replay fixture driven through the real
//! adapter (catalog discovery + paged reads) and the real collectors, with the
//! Store's acknowledgement simulated by adopting `next_state`. Verifies the
//! adapter/collector naming contract end to end (AT03, AT04, AT07, AT09) and
//! that replay data never becomes live or history-eligible.

// Test helpers may unwrap: a broken fixture must fail loudly.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use dgxtop_adapters::replay::ReplayAdapter;
use dgxtop_core::canonical::{
    CandidateDisposition, CanonicalValue, CatalogSnapshot, CollectionBatch, CollectionLimits,
    CollectionPlan, CollectorState, Explanation, PolicySnapshot, ProcessTable,
};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{AdapterId, CollectorId, HostId, MetricKey};
use dgxtop_core::observation::{DiscoverRequest, ReadPlan, paged_group, split_page};
use dgxtop_core::ports::{Collector, IngressBatch, TelemetryPort};
use dgxtop_core::quality::{Namespace, SampleKind, SelectionState};
use dgxtop_core::testing::FakeClock;
use uuid::Uuid;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../adapters/fixtures/replay/demo.json")
}

struct Pipeline {
    clock: Arc<FakeClock>,
    host_id: HostId,
    adapter: ReplayAdapter,
    catalog: Arc<CatalogSnapshot>,
    policy: Arc<PolicySnapshot>,
    collectors: Vec<Arc<dyn Collector>>,
    states: BTreeMap<CollectorId, (u64, Arc<CollectorState>)>,
    cycle: u64,
    latest: BTreeMap<MetricKey, CanonicalValue>,
    explanations: BTreeMap<MetricKey, Explanation>,
    process_table: Option<ProcessTable>,
    all_batches: Vec<CollectionBatch>,
}

impl Pipeline {
    fn new() -> Self {
        let clock = FakeClock::new(1_000_000_000_000);
        let host_id = HostId::new("replay-host").unwrap();
        let adapter_id = AdapterId::new("replay").unwrap();
        let time: Arc<dyn TimeProvider> = clock.clone();
        let mut adapter =
            ReplayAdapter::from_path(&fixture(), adapter_id.clone(), time, host_id.clone(), None)
                .unwrap()
                .with_in_process(true);
        let catalog = discover_all(&mut adapter, &adapter_id, clock.now_boot_ns());
        let policy = PolicySnapshot {
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
            namespace: Namespace::Replay,
        };
        Self {
            clock,
            host_id,
            adapter,
            catalog: Arc::new(catalog),
            policy: Arc::new(policy),
            collectors: dgxtop_collectors::registry::default_collectors(),
            states: BTreeMap::new(),
            cycle: 0,
            latest: BTreeMap::new(),
            explanations: BTreeMap::new(),
            process_table: None,
            all_batches: Vec::new(),
        }
    }

    fn read_groups(&self) -> BTreeSet<String> {
        self.catalog
            .sources
            .values()
            .map(|source| source.read_group.as_str().to_owned())
            .collect()
    }

    fn read_cycle(&mut self) -> Vec<Arc<IngressBatch>> {
        let mut batches = Vec::new();
        for group in self.read_groups() {
            let mut page = 0;
            loop {
                let plan = ReadPlan {
                    request_id: Uuid::new_v4(),
                    adapter_id: AdapterId::new("replay").unwrap(),
                    host_generation: 1,
                    catalog_revision: self.catalog.revision.max(1),
                    config_revision: 1,
                    cycle_id: self.cycle,
                    read_group: dgxtop_core::identity::ReadGroup::new(paged_group(&group, page))
                        .unwrap(),
                    source_slots: Vec::new(),
                    deadline_boot_ns: self.clock.now_boot_ns() + 1_000_000_000,
                    epoch: 1,
                    page,
                };
                let batch = self.adapter.read(&plan);
                batch.validate_against(&plan, 256).unwrap();
                let continues = batch.continues();
                batches.push(Arc::new(IngressBatch::new(batch, None)));
                if !continues {
                    break;
                }
                page += 1;
            }
        }
        batches
    }

    /// One sampling cycle: read every group, run every consuming collector,
    /// then adopt the next state exactly as a successful Store Ack would.
    fn step(&mut self) {
        self.clock.advance_ms(1000);
        self.cycle += 1;
        let batches = self.read_cycle();
        for collector in self.collectors.clone() {
            let inputs: Vec<Arc<IngressBatch>> = batches
                .iter()
                .filter(|batch| {
                    collector.consumes(&batch.adapter_id, split_page(batch.read_group.as_str()).0)
                })
                .cloned()
                .collect();
            if inputs.is_empty() {
                continue;
            }
            let (revision, state) = self
                .states
                .get(collector.id())
                .cloned()
                .unwrap_or_else(|| (0, Arc::new(CollectorState::default())));
            let plan = CollectionPlan {
                collector_id: collector.id().clone(),
                batch_id: Uuid::new_v4(),
                cycle_id: self.cycle,
                epoch: 1,
                config_revision: 1,
                host_id: self.host_id.clone(),
                boot_id: self.clock.boot_id(),
                catalog: Arc::clone(&self.catalog),
                policy: Arc::clone(&self.policy),
                state: Arc::clone(&state),
                state_revision: revision,
                inputs,
                now_boot_ns: self.clock.now_boot_ns(),
                limits: CollectionLimits {
                    max_series: 512,
                    max_sources: 2048,
                    max_candidates_per_series: 4,
                    max_processes: 256,
                    max_process_gpu_links: 1024,
                },
                committed: Arc::default(),
            };
            let output = collector.collect(&plan);
            output.validate().unwrap();
            assert_eq!(*state, *plan.state, "collector mutated the committed state");
            for value in &output.canonical {
                self.latest.insert(value.key.clone(), value.clone());
            }
            for explanation in &output.explanations {
                self.explanations
                    .insert(explanation.key.clone(), explanation.clone());
            }
            if let Some(table) = &output.process_table {
                self.process_table = Some(table.clone());
            }
            self.states.insert(
                collector.id().clone(),
                (revision + 1, Arc::new(output.next_state.clone())),
            );
            self.all_batches.push(output);
        }
    }

    fn value(&self, metric: &str, scope: &str, entity: &str) -> Option<&CanonicalValue> {
        self.latest.iter().find_map(|(key, value)| {
            (key.metric_id.as_str() == metric
                && key.scope.as_str() == scope
                && key.entity_id.as_str() == entity)
                .then_some(value)
        })
    }
}

fn discover_all(adapter: &mut ReplayAdapter, adapter_id: &AdapterId, now: u64) -> CatalogSnapshot {
    let mut catalog = CatalogSnapshot {
        revision: 1,
        ..CatalogSnapshot::default()
    };
    let mut page = 0;
    loop {
        let delta = adapter.discover(&DiscoverRequest {
            request_id: Uuid::new_v4(),
            adapter_id: adapter_id.clone(),
            host_generation: 1,
            catalog_revision: 1,
            config_revision: 1,
            deadline_boot_ns: now + 1_000_000_000,
            previous_generation: 0,
            page,
        });
        delta.validate(256).unwrap();
        for entity in delta.entities {
            catalog
                .entities
                .insert(entity.entity_id.clone(), Arc::new(entity));
        }
        for source in delta.sources {
            catalog
                .sources
                .insert(source.source_id.clone(), Arc::new(source));
        }
        match delta.next_page {
            Some(next) if !delta.complete => page = next,
            _ => break,
        }
    }
    catalog.regroup();
    catalog
}

fn run(cycles: usize) -> Pipeline {
    let mut pipeline = Pipeline::new();
    for _ in 0..cycles {
        pipeline.step();
    }
    pipeline
}

#[test]
fn package_temperature_alias_counts_once_and_soc_stays_separate() {
    let pipeline = run(3);
    let package = pipeline
        .value("cpu.temperature.package", "package:0", "cpu.package.0")
        .expect("package 0 temperature published");
    assert_eq!(package.selection_state, SelectionState::Selected);
    let celsius = package.value.unwrap().to_f64_lossy();
    assert!((60.0..80.0).contains(&celsius), "package 0 = {celsius}");
    let soc = pipeline
        .value("soc.temperature", "soc", "soc")
        .expect("SoC temperature published");
    assert!(soc.selection_state.has_value());
    assert!((40.0..50.0).contains(&soc.value.unwrap().to_f64_lossy()));
    // The thermal x86_pkg_temp zone is the same physical sensor: an alias, not a vote.
    let explanation = pipeline
        .explanations
        .iter()
        .find(|(key, _)| {
            key.scope.as_str() == "package:0" && key.metric_id.as_str() == "cpu.temperature.package"
        })
        .map(|(_, explanation)| explanation)
        .expect("explanation for package 0");
    assert!(
        explanation
            .candidates
            .iter()
            .any(|candidate| matches!(candidate.disposition, CandidateDisposition::Alias { .. })),
        "alias not recognised: {explanation:#?}"
    );
}

#[test]
fn independent_sources_beyond_tolerance_are_a_conflict_without_value() {
    let pipeline = run(3);
    let package1 = pipeline
        .value("cpu.temperature.package", "package:1", "cpu.package.1")
        .expect("package 1 published");
    assert_eq!(
        package1.selection_state,
        SelectionState::Conflict,
        "{package1:#?}"
    );
    assert!(
        package1.value.is_none(),
        "a conflict never carries an averaged value"
    );
    assert!(package1.source_id.is_none());
}

#[test]
fn replay_values_are_never_live_or_history_eligible() {
    let pipeline = run(3);
    assert!(!pipeline.latest.is_empty());
    for value in pipeline.latest.values() {
        assert!(
            matches!(
                value.sample_kind,
                SampleKind::Simulated | SampleKind::Replay
            ),
            "{:?} has live kind {:?}",
            value.key,
            value.sample_kind
        );
        assert!(
            !value.eligible_for_history,
            "{:?} is history-eligible",
            value.key
        );
    }
    let intervals: usize = pipeline
        .all_batches
        .iter()
        .map(|batch| batch.counter_intervals.len())
        .sum();
    assert_eq!(
        intervals, 0,
        "replay counter intervals must not reach live history"
    );
}
