//! In-process replay pipeline for micro benchmarks: the REF fixture split
//! over replay hosts → `ReplayAdapter` discovery and paged reads → the real
//! collectors (arbitration included) → optionally the real live state.
//!
//! No threads, IPC or scheduler are involved, so a bench timing a step of
//! this pipeline measures exactly that hot path. Read groups follow their
//! configured period class (fast every cycle, slow/capacity every
//! `period / interval` cycles), as the Manager's scheduler would.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dgxtop_adapters::replay::{ReplayAdapter, ReplayError, ReplayFixture};
use dgxtop_collectors::registry::default_collectors;
use dgxtop_core::canonical::{
    CatalogSnapshot, CollectionBatch, CollectionLimits, CollectionPlan, CollectorState,
    PolicySnapshot,
};
use dgxtop_core::clock::{NANOS_PER_MILLI, TimeProvider};
use dgxtop_core::config::Config;
use dgxtop_core::identity::{AdapterId, BootId, CollectorId, HostId, IdentityError, ReadGroup};
use dgxtop_core::observation::{DiscoverRequest, ReadPlan, paged_group, split_page};
use dgxtop_core::ports::{Collector, IngressBatch, TelemetryPort};
use dgxtop_core::quality::Namespace;
use thiserror::Error;
use uuid::Uuid;

use super::coverage::replay_group_period_ms;

/// Engineering default tolerance for non-temperature gauges (D-08).
const RELATIVE_TOLERANCE_PERCENT: f64 = 10.0;
/// Pages read per group and cycle before giving up (a broken adapter must
/// not make a bench loop forever).
const MAX_PAGES: u32 = 4096;

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error(transparent)]
    Replay(#[from] ReplayError),
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("discovery of {adapter} did not finish within {pages} pages")]
    EndlessDiscovery { adapter: String, pages: u32 },
}

/// A boot clock the bench moves explicitly (deterministic timestamps).
#[derive(Debug)]
pub struct ManualClock {
    boot_ns: AtomicU64,
    boot_id: BootId,
}

impl ManualClock {
    pub fn new(start_boot_ns: u64) -> Result<Arc<Self>, IdentityError> {
        Ok(Arc::new(Self {
            boot_ns: AtomicU64::new(start_boot_ns),
            boot_id: BootId::new("bench-manual-boot")?,
        }))
    }

    pub fn advance_ns(&self, nanoseconds: u64) {
        self.boot_ns.fetch_add(nanoseconds, Ordering::AcqRel);
    }
}

impl TimeProvider for ManualClock {
    fn boot_id(&self) -> BootId {
        self.boot_id.clone()
    }

    fn now_boot_ns(&self) -> u64 {
        self.boot_ns.load(Ordering::Acquire)
    }

    fn now_utc_ns(&self) -> i128 {
        // Fixed epoch offset: wall time is irrelevant to these benches.
        1_700_000_000_000_000_000 + i128::from(self.now_boot_ns())
    }
}

/// One replay host: its adapter and the groups it serves with their period
/// in cycles.
struct ReplayHost {
    adapter_id: AdapterId,
    adapter: ReplayAdapter,
    groups: Vec<(String, u64)>,
}

pub struct ReplayPipeline {
    clock: Arc<ManualClock>,
    host_id: HostId,
    hosts: Vec<ReplayHost>,
    catalog: Arc<CatalogSnapshot>,
    policy: Arc<PolicySnapshot>,
    collectors: Vec<Arc<dyn Collector>>,
    states: BTreeMap<CollectorId, (u64, Arc<CollectorState>)>,
    limits: CollectionLimits,
    interval_ns: u64,
    cycle: u64,
}

impl ReplayPipeline {
    /// Splits `fixture` over `hosts` replay adapters (`replay-1..`) and
    /// discovers every catalog. Memory peaks at one extra fixture copy.
    pub fn new(
        fixture: &ReplayFixture,
        hosts: u32,
        config: &Config,
        start_boot_ns: u64,
    ) -> Result<Self, PipelineError> {
        let clock = ManualClock::new(start_boot_ns)?;
        let host_id = HostId::new("bench-replay-host")?;
        let time: Arc<dyn TimeProvider> = clock.clone();
        let interval_ms = u64::from(config.sampling.interval_ms.max(1));
        let mut catalog = CatalogSnapshot {
            revision: 1,
            host_id: Some(host_id.clone()),
            boot_id: Some(clock.boot_id()),
            ..CatalogSnapshot::default()
        };
        let mut replay_hosts = Vec::new();
        let count = hosts.max(1);
        for index in 0..count {
            let adapter_id = AdapterId::new(format!("replay-{}", index + 1))?;
            let mut adapter = ReplayAdapter::from_fixture(
                fixture.clone(),
                adapter_id.clone(),
                Arc::clone(&time),
                host_id.clone(),
                Some((index, count)),
            )?;
            discover_into(&mut adapter, &adapter_id, start_boot_ns, &mut catalog)?;
            replay_hosts.push(ReplayHost {
                adapter_id,
                adapter,
                groups: Vec::new(),
            });
        }
        catalog.regroup();
        assign_groups(&mut replay_hosts, &catalog, config, interval_ms);
        let policy = policy_for(config, &replay_hosts);
        Ok(Self {
            clock,
            host_id,
            hosts: replay_hosts,
            catalog: Arc::new(catalog),
            policy: Arc::new(policy),
            collectors: default_collectors(),
            states: BTreeMap::new(),
            limits: CollectionLimits {
                max_series: config.store.max_series,
                max_sources: config.store.max_sources,
                max_candidates_per_series: config.store.max_candidates_per_series,
                max_processes: config.store.max_processes,
                max_process_gpu_links: config.store.max_process_gpu_links,
            },
            interval_ns: interval_ms.saturating_mul(NANOS_PER_MILLI),
            cycle: 0,
        })
    }

    pub fn clock(&self) -> &Arc<ManualClock> {
        &self.clock
    }

    pub fn host_id(&self) -> &HostId {
        &self.host_id
    }

    pub fn catalog(&self) -> &Arc<CatalogSnapshot> {
        &self.catalog
    }

    pub fn cycle(&self) -> u64 {
        self.cycle
    }

    /// Moves time forward one sampling interval and reads every due group
    /// of every host (all pages).
    pub fn read_next_cycle(&mut self) -> Vec<Arc<IngressBatch>> {
        self.clock.advance_ns(self.interval_ns);
        self.cycle += 1;
        let now = self.clock.now_boot_ns();
        let revision = self.catalog.revision.max(1);
        let mut batches = Vec::new();
        for host in &mut self.hosts {
            let ReplayHost {
                adapter_id,
                adapter,
                groups,
            } = host;
            for (group, period_cycles) in groups.iter() {
                if !self.cycle.is_multiple_of((*period_cycles).max(1)) {
                    continue;
                }
                let target = ReadTarget {
                    adapter_id,
                    group,
                    cycle: self.cycle,
                    now,
                    catalog_revision: revision,
                };
                read_group(adapter, &target, &mut batches);
            }
        }
        batches
    }

    /// One collection plan per collector that consumes any of `batches`.
    pub fn plans(
        &self,
        batches: &[Arc<IngressBatch>],
    ) -> Vec<(Arc<dyn Collector>, CollectionPlan)> {
        self.collectors
            .iter()
            .filter_map(|collector| {
                let inputs: Vec<Arc<IngressBatch>> = batches
                    .iter()
                    .filter(|batch| {
                        collector
                            .consumes(&batch.adapter_id, split_page(batch.read_group.as_str()).0)
                    })
                    .cloned()
                    .collect();
                (!inputs.is_empty())
                    .then(|| (Arc::clone(collector), self.plan_for(collector, inputs)))
            })
            .collect()
    }

    fn plan_for(
        &self,
        collector: &Arc<dyn Collector>,
        inputs: Vec<Arc<IngressBatch>>,
    ) -> CollectionPlan {
        let (state_revision, state) = self
            .states
            .get(collector.id())
            .cloned()
            .unwrap_or_else(|| (0, Arc::new(CollectorState::default())));
        CollectionPlan {
            collector_id: collector.id().clone(),
            batch_id: Uuid::new_v4(),
            cycle_id: self.cycle,
            epoch: 1,
            config_revision: 1,
            host_id: self.host_id.clone(),
            boot_id: self.clock.boot_id(),
            catalog: Arc::clone(&self.catalog),
            policy: Arc::clone(&self.policy),
            state,
            state_revision,
            inputs,
            now_boot_ns: self.clock.now_boot_ns(),
            limits: self.limits,
        }
    }

    /// Adopts a batch's next state at `state_revision`, exactly what the
    /// Manager does after a committed store Ack.
    pub fn adopt(&mut self, batch: &CollectionBatch, state_revision: u64) {
        self.adopt_state(
            batch.collector_id.clone(),
            batch.next_state.clone(),
            state_revision,
        );
    }

    /// [`Self::adopt`] for a state taken out of a batch handed to a store.
    pub fn adopt_state(
        &mut self,
        collector_id: CollectorId,
        state: CollectorState,
        state_revision: u64,
    ) {
        self.states
            .insert(collector_id, (state_revision, Arc::new(state)));
    }
}

/// One group read of one cycle.
struct ReadTarget<'a> {
    adapter_id: &'a AdapterId,
    group: &'a str,
    cycle: u64,
    now: u64,
    catalog_revision: u32,
}

fn read_group(
    adapter: &mut ReplayAdapter,
    target: &ReadTarget<'_>,
    batches: &mut Vec<Arc<IngressBatch>>,
) {
    for page in 0..MAX_PAGES {
        let Ok(read_group) = ReadGroup::new(paged_group(target.group, page)) else {
            return;
        };
        let plan = ReadPlan {
            request_id: Uuid::new_v4(),
            adapter_id: target.adapter_id.clone(),
            host_generation: 1,
            catalog_revision: target.catalog_revision,
            config_revision: 1,
            cycle_id: target.cycle,
            read_group,
            source_slots: Vec::new(),
            deadline_boot_ns: target.now.saturating_add(1_000_000_000),
            epoch: 1,
            page,
        };
        let batch = adapter.read(&plan);
        let continues = batch.continues();
        batches.push(Arc::new(IngressBatch::new(batch, None)));
        if !continues {
            return;
        }
    }
}

fn discover_into(
    adapter: &mut ReplayAdapter,
    adapter_id: &AdapterId,
    now: u64,
    catalog: &mut CatalogSnapshot,
) -> Result<(), PipelineError> {
    for page in 0..MAX_PAGES {
        let delta = adapter.discover(&DiscoverRequest {
            request_id: Uuid::new_v4(),
            adapter_id: adapter_id.clone(),
            host_generation: 1,
            catalog_revision: 1,
            config_revision: 1,
            deadline_boot_ns: now.saturating_add(1_000_000_000),
            previous_generation: 0,
            page,
        });
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
        catalog
            .capabilities
            .entry(adapter_id.clone())
            .or_default()
            .extend(delta.capabilities);
        match delta.next_page {
            Some(_) if !delta.complete => {}
            _ => return Ok(()),
        }
    }
    Err(PipelineError::EndlessDiscovery {
        adapter: adapter_id.to_string(),
        pages: MAX_PAGES,
    })
}

/// Groups each host serves (from its discovered sources) with the period of
/// the live group they mirror, in cycles of the fast interval.
fn assign_groups(
    hosts: &mut [ReplayHost],
    catalog: &CatalogSnapshot,
    config: &Config,
    interval_ms: u64,
) {
    for host in hosts {
        let mut groups: Vec<String> = catalog
            .sources
            .values()
            .filter(|source| source.adapter_id == host.adapter_id)
            .map(|source| source.read_group.as_str().to_owned())
            .collect();
        groups.sort();
        groups.dedup();
        host.groups = groups
            .into_iter()
            .map(|group| {
                let period_ms = u64::from(replay_group_period_ms(&group, &config.sampling));
                (group, (period_ms / interval_ms).max(1))
            })
            .collect();
    }
}

fn policy_for(config: &Config, hosts: &[ReplayHost]) -> PolicySnapshot {
    let effective_periods_ms = hosts
        .iter()
        .flat_map(|host| {
            host.groups.iter().map(|(group, _)| {
                (
                    format!("{}/{group}", host.adapter_id),
                    replay_group_period_ms(group, &config.sampling),
                )
            })
        })
        .collect();
    PolicySnapshot {
        revision: 1,
        switch_successes: config.quality.switch_successes,
        switch_min_hold_ms: config.quality.switch_min_hold_ms,
        temperature_disagreement_deg_c: config.quality.temperature_disagreement_deg_c,
        relative_tolerance_percent: RELATIVE_TOLERANCE_PERCENT,
        alignment_tolerance_ms: config.quality.alignment_tolerance_ms,
        excluded_sources: Default::default(),
        pinned_sources: Default::default(),
        stale_factor: config.sampling.stale_factor,
        effective_periods_ms,
        namespace: Namespace::Replay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_adapters::replay::{RefProfile, generate_ref_fixture};

    #[test]
    fn reference_fixture_flows_through_every_collector() {
        let fixture = generate_ref_fixture(
            3,
            &RefProfile {
                cycles: 3,
                simulated_latency_us: 0,
                ..RefProfile::reference()
            },
        );
        let config = Config::defaults().unwrap();
        let mut pipeline = ReplayPipeline::new(&fixture, 7, &config, 1_000_000_000_000).unwrap();
        assert_eq!(pipeline.catalog().sources.len(), fixture.sources.len());
        let mut published = 0;
        for _ in 0..3 {
            let batches = pipeline.read_next_cycle();
            assert!(!batches.is_empty());
            let plans = pipeline.plans(&batches);
            assert_eq!(plans.len(), 6, "cpu, gpu, memory, disk, network, process");
            for (collector, plan) in plans {
                let batch = collector.collect(&plan);
                batch.validate().unwrap();
                assert!(
                    batch
                        .canonical
                        .iter()
                        .all(|value| !value.eligible_for_history)
                );
                published += batch.canonical.len();
                let next = plan.state_revision + 1;
                pipeline.adopt(&batch, next);
            }
        }
        assert!(published > 512, "canonical values published: {published}");
        assert_eq!(pipeline.cycle(), 3);
    }
}
