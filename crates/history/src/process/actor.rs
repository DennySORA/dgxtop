//! The process actor (D-79): one owner of the runs, the process memory
//! record and the jobs that leave for the probe and the stores. It takes
//! one input at a time, reads its clock for it, and publishes a new read
//! model when what it shows changed.

use std::sync::{Arc, PoisonError, RwLock};

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::BootId;
use dgxtop_core::ports::{
    Dispatch, Logger, ProbeDeriver, ProcessHistoryInput, ProcessHistoryJob, ProcessProbeJob,
};
use dgxtop_core::process_model::{ProcessTableSample, WriteProgress};

use super::Now;
use super::memory::ProcessMemoryBook;
use super::model::{ModelBuilder, ModelInputs};
use super::published::PublishedProcessHistory;
use super::runs::{ProcessHistoryService, SampleInputs, ServicePorts};

/// The memory record's period until the first table brings its own.
const DEFAULT_PERIOD_MS: u64 = 1_000;

/// Where the runs start from.
#[derive(Debug, Clone)]
pub struct ProcessHistorySettings {
    pub boot_id: BootId,
    /// Local time's offset from UTC, seconds east: buckets follow the
    /// local clock (D-77).
    pub utc_offset_s: i32,
}

/// The actor's ports; the probe and the stores may be absent (no probe in
/// a replay, no stores with the process history off disk, D-80).
pub struct ProcessHistoryPorts {
    pub probe: Option<Box<dyn Dispatch<ProcessProbeJob>>>,
    /// Makes the values of the probe's readings (the collectors' process
    /// part, D-79).
    pub probe_deriver: Arc<dyn ProbeDeriver>,
    pub store: Option<Box<dyn Dispatch<ProcessHistoryJob>>>,
    pub clock: Arc<dyn TimeProvider>,
    pub logger: Arc<dyn Logger>,
}

/// The process actor's state (module docs).
pub struct ProcessHistory {
    service: ProcessHistoryService,
    memory: Arc<RwLock<ProcessMemoryBook>>,
    model: ModelBuilder,
    published: PublishedProcessHistory,
    clock: Arc<dyn TimeProvider>,
    started: bool,
    flushes: u64,
}

impl ProcessHistory {
    /// The actor and what it publishes. It asks the stores for what they
    /// kept at its first input, once the composition root has wired their
    /// answers back to it.
    pub fn new(
        settings: ProcessHistorySettings,
        ports: ProcessHistoryPorts,
    ) -> (Self, PublishedProcessHistory) {
        let now_ns = ports.clock.now_boot_ns();
        let service = ProcessHistoryService::new(
            ServicePorts {
                probe: ports.probe,
                probe_deriver: ports.probe_deriver,
                store: ports.store,
            },
            (settings.boot_id, settings.utc_offset_s),
            ports.logger,
            now_ns,
        );
        // Every table brings the period it was read at.
        let memory = Arc::new(RwLock::new(ProcessMemoryBook::new(DEFAULT_PERIOD_MS)));
        let published = PublishedProcessHistory::new(Arc::clone(&memory));
        let mut actor = Self {
            service,
            memory,
            model: ModelBuilder::default(),
            published: published.clone(),
            clock: ports.clock,
            started: false,
            flushes: 0,
        };
        actor.publish();
        (actor, published)
    }

    /// Takes one input and publishes what changed.
    pub fn handle(&mut self, input: ProcessHistoryInput) {
        if !self.started {
            self.started = true;
            self.service.start();
        }
        let now = Now {
            boot_ns: self.clock.now_boot_ns(),
            utc_ns: self.clock.now_utc_ns(),
        };
        // A flush sends everything; nothing is due after it.
        let due = !matches!(input, ProcessHistoryInput::Flush);
        match input {
            ProcessHistoryInput::Table(sample) => self.on_table(&sample, now),
            ProcessHistoryInput::Tick => self.service.on_sampled_table(),
            ProcessHistoryInput::Probe(report) => self.service.on_probe(&report, now),
            ProcessHistoryInput::Store(completion) => self.service.on_completion(completion, now),
            ProcessHistoryInput::Signal { key, signal } => {
                self.service.note_signal(&key, signal, now);
            }
            ProcessHistoryInput::Show(request) => {
                self.service.open(&request.key);
                self.model.show(request);
            }
            ProcessHistoryInput::Flush => {
                self.service.flush(now);
                self.flushes += 1;
            }
        }
        if due {
            self.service.tick(now);
        }
        self.publish();
    }

    fn on_table(&mut self, sample: &ProcessTableSample, now: Now) {
        {
            let mut memory = self.memory.write().unwrap_or_else(PoisonError::into_inner);
            memory.set_interval(sample.process_period_ms);
            memory.record(&sample.table);
        }
        self.service.on_table(
            &sample.table,
            &SampleInputs {
                now,
                clk_tck: sample.clk_tck,
                gpu_labels: &sample.gpu_labels,
                host_network: sample.host_network,
            },
        );
    }

    fn publish(&mut self) {
        let memory = self.memory.read().unwrap_or_else(PoisonError::into_inner);
        let built = self.model.build(&ModelInputs {
            book: self.service.book(),
            memory: &memory,
            kept_on_disk: self.service.keeps_runs_on_disk(),
            writes: WriteProgress {
                flushes: self.flushes,
                pending: self.service.writes_pending(),
            },
        });
        drop(memory);
        if let Some(model) = built {
            self.published.publish(model);
        }
    }
}

#[cfg(test)]
mod tests;
