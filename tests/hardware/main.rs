//! Hardware suite (run with `-- --ignored hardware_`): the real supervisor
//! launches every live adapter as an isolated `dgxtop --adapter-host` child
//! process, performs catalog discovery and reads every read group on this
//! machine's real procfs/sysfs/NVML (AT01, AT14 on real hosts). Only reads;
//! the only processes ever signalled are the managed adapter hosts during the
//! bounded shutdown.

// Test helpers may unwrap: an unexpected environment must fail loudly.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded};
use dgxtop_core::budget::{ByteLedger, MIB, Partition, QueueGate};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::config::Config;
use dgxtop_core::identity::{AdapterId, ReadGroup};
use dgxtop_core::naming;
use dgxtop_core::observation::{
    CatalogDelta, DiscoverRequest, ObservationBatch, ReadPlan, paged_group,
};
use dgxtop_core::ports::{AdapterJob, Dispatch, IngestionCompletion, NullLogger};
use dgxtop_core::quality::ReadStatus;
use dgxtop_core::view::HostState;
use dgxtop_runtime::clock::BootClock;
use dgxtop_runtime::supervisor::{
    HostLaunch, HostSpec, Supervisor, SupervisorConfig, SupervisorHandle,
};
use uuid::Uuid;

const BINARY: &str = env!("CARGO_BIN_EXE_dgxtop");
const WAIT: Duration = Duration::from_secs(15);

struct LiveHosts {
    handle: SupervisorHandle,
    completions: Receiver<IngestionCompletion>,
    clock: Arc<dyn TimeProvider>,
    generations: BTreeMap<String, u32>,
}

impl LiveHosts {
    fn start(adapters: &[&str]) -> Self {
        let config = Config::defaults().unwrap();
        let ledger = ByteLedger::for_memory_limit_mib(256);
        let ingress = QueueGate::new("ingress", 32, 8 * MIB, ledger.clone(), Partition::Ingress);
        let (sender, completions) = bounded(4096);
        let clock: Arc<dyn TimeProvider> = Arc::new(BootClock::new().unwrap());
        let specs = adapters
            .iter()
            .map(|adapter| HostSpec {
                adapter_id: AdapterId::new(*adapter).unwrap(),
                launch: HostLaunch::ChildProcess {
                    program: PathBuf::from(BINARY),
                    args: vec!["--adapter-host".to_owned(), (*adapter).to_owned()],
                },
            })
            .collect();
        let handle = Supervisor::start(
            SupervisorConfig::from_config(&config),
            specs,
            ledger,
            ingress,
            sender,
            Arc::clone(&clock),
            Arc::new(NullLogger),
        )
        .unwrap();
        let mut hosts = Self {
            handle,
            completions,
            clock,
            generations: BTreeMap::new(),
        };
        hosts.wait_healthy(adapters);
        hosts
    }

    fn wait_healthy(&mut self, adapters: &[&str]) {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            for status in self.handle.statuses() {
                if status.state == HostState::Healthy {
                    self.generations
                        .insert(status.adapter_id.clone(), status.generation);
                }
            }
            if adapters
                .iter()
                .all(|adapter| self.generations.contains_key(*adapter))
            {
                return;
            }
            let _ = self.completions.recv_timeout(Duration::from_millis(50));
        }
        panic!("hosts not healthy: {:?}", self.handle.statuses());
    }

    fn deadline(&self) -> u64 {
        self.clock.now_boot_ns() + 5_000_000_000
    }

    fn discover(&self, adapter: &str) -> Vec<CatalogDelta> {
        let mut pages = Vec::new();
        let mut page = 0;
        loop {
            let request = DiscoverRequest {
                request_id: Uuid::new_v4(),
                adapter_id: AdapterId::new(adapter).unwrap(),
                host_generation: self.generations[adapter],
                catalog_revision: 1,
                config_revision: 1,
                deadline_boot_ns: self.deadline(),
                previous_generation: 0,
                page,
            };
            let request_id = request.request_id;
            self.dispatch(AdapterJob::Discover(request));
            let delta = self.await_catalog(request_id);
            let next = delta.next_page.filter(|_| !delta.complete);
            pages.push(delta);
            match next {
                Some(next_page) => page = next_page,
                None => return pages,
            }
        }
    }

    fn read_group(&self, adapter: &str, group: &str, cycle: u64) -> Vec<ObservationBatch> {
        let mut batches = Vec::new();
        let mut page = 0;
        loop {
            let plan = ReadPlan {
                request_id: Uuid::new_v4(),
                adapter_id: AdapterId::new(adapter).unwrap(),
                host_generation: self.generations[adapter],
                catalog_revision: 1,
                config_revision: 1,
                cycle_id: cycle,
                read_group: ReadGroup::new(paged_group(group, page)).unwrap(),
                source_slots: Vec::new(),
                deadline_boot_ns: self.deadline(),
                epoch: 1,
                page,
            };
            let request_id = plan.request_id;
            self.dispatch(AdapterJob::Read(plan));
            let batch = self.await_batch(request_id);
            let continues = batch.continues();
            batches.push(batch);
            if !continues {
                return batches;
            }
            page += 1;
        }
    }

    fn dispatch(&self, job: AdapterJob) {
        let deadline = Instant::now() + WAIT;
        loop {
            match self.handle.try_dispatch(job.clone()) {
                Ok(()) => return,
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("dispatch failed: {error}"),
            }
        }
    }

    fn await_catalog(&self, request_id: Uuid) -> CatalogDelta {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if let Ok(IngestionCompletion::Catalog {
                request_id: id,
                result,
                ..
            }) = self.completions.recv_timeout(Duration::from_millis(100))
                && id == request_id
            {
                return result.unwrap_or_else(|failure| panic!("discover failed: {failure:?}"));
            }
        }
        panic!("no catalog reply for {request_id}");
    }

    fn await_batch(&self, request_id: Uuid) -> ObservationBatch {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if let Ok(IngestionCompletion::Batch {
                request_id: id,
                result,
                ..
            }) = self.completions.recv_timeout(Duration::from_millis(100))
                && id == request_id
            {
                let batch = result.unwrap_or_else(|failure| panic!("read failed: {failure:?}"));
                return batch.batch().clone();
            }
        }
        panic!("no batch for {request_id}");
    }
}

impl Drop for LiveHosts {
    fn drop(&mut self) {
        let report = self.handle.shutdown(Duration::from_secs(3));
        assert!(
            report.completed_within_deadline,
            "bounded shutdown exceeded: {report:?}"
        );
    }
}

fn record_count(batches: &[ObservationBatch]) -> (usize, usize) {
    let total = batches.iter().map(|batch| batch.records.len()).sum();
    let ok = batches
        .iter()
        .flat_map(|batch| &batch.records)
        .filter(|record| record.read_status == ReadStatus::Ok)
        .count();
    (total, ok)
}

#[test]
#[ignore = "hardware: reads this machine's real procfs/sysfs/NVML through isolated hosts"]
fn hardware_every_live_adapter_discovers_and_reads_through_isolated_hosts() {
    let gpu_present = std::path::Path::new("/dev/nvidia0").exists();
    let adapters: Vec<&str> = naming::adapters::LIVE
        .iter()
        .copied()
        .filter(|adapter| gpu_present || *adapter != naming::adapters::NVML)
        .collect();
    let hosts = LiveHosts::start(&adapters);
    let pids: BTreeSet<i32> = hosts.handle.managed_pids().into_iter().collect();
    assert_eq!(
        pids.len(),
        adapters.len(),
        "one isolated process per live adapter"
    );
    assert!(!pids.contains(&i32::try_from(std::process::id()).unwrap()));

    for adapter in &adapters {
        let catalog = hosts.discover(adapter);
        let entities: usize = catalog.iter().map(|page| page.entities.len()).sum();
        let sources: usize = catalog.iter().map(|page| page.sources.len()).sum();
        println!(
            "{adapter}: {entities} entities, {sources} sources, {} pages",
            catalog.len()
        );
        for spec in naming::read_groups_of(adapter) {
            let batches = hosts.read_group(adapter, spec.group, 1);
            let (total, ok) = record_count(&batches);
            println!(
                "  {}/{}: {} pages, {total} records ({ok} ok)",
                adapter,
                spec.group,
                batches.len()
            );
            for batch in &batches {
                for record in &batch.records {
                    record.validate().unwrap();
                    if record.read_status != ReadStatus::Ok {
                        assert!(
                            record.value.is_none(),
                            "failed read carried a value: {record:?}"
                        );
                    }
                }
            }
        }
    }

    let cpu = hosts.read_group(naming::adapters::PROCFS, naming::groups::CPU, 2);
    assert!(record_count(&cpu).1 > 0, "procfs cpu counters readable");
    let memory = hosts.read_group(naming::adapters::PROCFS, naming::groups::MEMORY, 2);
    assert!(record_count(&memory).1 >= 5, "meminfo readable");
    if gpu_present {
        let gpu = hosts.read_group(naming::adapters::NVML, naming::groups::FAST, 2);
        assert!(record_count(&gpu).1 > 0, "NVML fast metrics readable");
    }
}
