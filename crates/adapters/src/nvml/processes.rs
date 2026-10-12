//! Reads the `process` group: per-GPU process associations.
//!
//! A PID that holds both a compute and a graphics context on the same GPU is
//! ONE association (its memory is reported once, never summed). The same PID
//! on two GPUs is two associations (`gpupid:<UUID>:<PID>` entities). Each
//! association yields one record per metric that the driver can answer:
//! * `process.gpu.memory_allocated` (bytes): always one record; a value the
//!   driver reports as unavailable becomes an `unsupported` record, never 0.
//! * `process.gpu.utilization` (percent): the newest SM sample since the last
//!   seen driver timestamp of that GPU; no record when the driver had no new
//!   sample for the PID, an `unsupported` record when the GPU cannot sample.
//!
//! The usage kind travels as reason code `gpu_usage:<kind>` so the process
//! collector can fill `ProcessGpuLink::usage` without a second source.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{BootId, EntityId, HostId, MetricId, MetricKey};
use dgxtop_core::metric::{Scalar, ids};
use dgxtop_core::naming;
use dgxtop_core::observation::{Observation, ReadPlan};
use dgxtop_core::quality::{Quality, ReadStatus};

use crate::common::RecordFactory;

use super::backend::{
    BackendError, BackendResult, GpuProcessEntry, GpuProcessQueries, ProcessUtilizationSample,
};
use super::capabilities::{Admission, CapabilityTracker};
use super::catalog::PublishedSources;
use super::channels::Channel;
use super::inventory::Inventory;

/// Associations kept per GPU per cycle (the default `max_process_gpu_links`).
pub const MAX_PROCESSES_PER_GPU: usize = 1024;
const COMPUTE_LIST_KEY: &str = "process.compute";
const GRAPHICS_LIST_KEY: &str = "process.graphics";

/// How a process uses one GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageKind {
    Compute,
    Graphics,
    ComputeAndGraphics,
}

impl UsageKind {
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::Compute => "gpu_usage:compute",
            Self::Graphics => "gpu_usage:graphics",
            Self::ComputeAndGraphics => "gpu_usage:compute_and_graphics",
        }
    }

    const fn with(self, other: Self) -> Self {
        match (self, other) {
            (Self::Compute, Self::Compute) => Self::Compute,
            (Self::Graphics, Self::Graphics) => Self::Graphics,
            _ => Self::ComputeAndGraphics,
        }
    }
}

/// One `(GPU, PID)` association after merging both context lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Association {
    pub pid: i32,
    /// `None` when every list reported the value as unavailable.
    pub memory_bytes: Option<u64>,
    pub usage: UsageKind,
}

/// Merged associations of one GPU.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MergedProcesses {
    pub associations: Vec<Association>,
    /// More associations than [`MAX_PROCESSES_PER_GPU`] (lowest PIDs kept).
    pub truncated: bool,
    /// Entries with a PID outside `1..=i32::MAX` (cannot form a ProcessKey).
    pub invalid_pids: u32,
}

/// Merges compute and graphics lists per PID; memory is the larger reported
/// value because both lists describe the same per-process allocation.
pub fn merge_processes(
    compute: &[GpuProcessEntry],
    graphics: &[GpuProcessEntry],
) -> MergedProcesses {
    let mut merged: BTreeMap<i32, Association> = BTreeMap::new();
    let mut invalid_pids = 0_u32;
    let tagged = compute
        .iter()
        .map(|entry| (entry, UsageKind::Compute))
        .chain(graphics.iter().map(|entry| (entry, UsageKind::Graphics)));
    for (entry, usage) in tagged {
        let Some(pid) = i32::try_from(entry.pid).ok().filter(|pid| *pid > 0) else {
            invalid_pids = invalid_pids.saturating_add(1);
            continue;
        };
        merged
            .entry(pid)
            .and_modify(|existing| {
                existing.usage = existing.usage.with(usage);
                existing.memory_bytes = larger(existing.memory_bytes, entry.used_memory_bytes);
            })
            .or_insert(Association {
                pid,
                memory_bytes: entry.used_memory_bytes,
                usage,
            });
    }
    let truncated = merged.len() > MAX_PROCESSES_PER_GPU;
    MergedProcesses {
        associations: merged.into_values().take(MAX_PROCESSES_PER_GPU).collect(),
        truncated,
        invalid_pids,
    }
}

fn larger(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (value, None) | (None, value) => value,
    }
}

/// Newest sample per PID plus the newest timestamp seen.
pub fn latest_samples(samples: &[ProcessUtilizationSample]) -> (BTreeMap<u32, u32>, Option<u64>) {
    let mut newest: BTreeMap<u32, (u64, u32)> = BTreeMap::new();
    for sample in samples {
        let entry = newest
            .entry(sample.pid)
            .or_insert((sample.timestamp_us, sample.sm_percent));
        if sample.timestamp_us >= entry.0 {
            *entry = (sample.timestamp_us, sample.sm_percent);
        }
    }
    let max_timestamp = samples.iter().map(|sample| sample.timestamp_us).max();
    (
        newest
            .into_iter()
            .map(|(pid, (_, percent))| (pid, percent))
            .collect(),
        max_timestamp,
    )
}

/// Per-GPU utilization windows (driver timestamps) across cycles.
#[derive(Debug, Clone, Default)]
pub struct ProcessSampler {
    last_seen_us: BTreeMap<String, u64>,
}

/// Outcome of one process-group read.
#[derive(Debug, Default)]
pub struct ProcessOutcome {
    pub records: Vec<Observation>,
    pub session_lost: bool,
    /// Some GPU's process lists could not be read or were truncated.
    pub partial: bool,
}

/// Inputs of one process-group read.
pub struct ProcessRead<'a> {
    pub plan: &'a ReadPlan,
    pub clock: &'a dyn TimeProvider,
    pub host_id: &'a HostId,
    pub boot_id: &'a BootId,
    pub published: &'a PublishedSources,
    pub tracker: &'a mut CapabilityTracker,
    pub lost_devices: &'a mut BTreeSet<String>,
    pub requested_slots: &'a BTreeSet<u32>,
}

impl ProcessSampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Driver reload / generation change: utilization windows restart.
    pub fn reset(&mut self) {
        self.last_seen_us.clear();
    }

    pub fn read(
        &mut self,
        backend: &dyn GpuProcessQueries,
        inventory: &Inventory,
        read: &mut ProcessRead<'_>,
    ) -> ProcessOutcome {
        let mut outcome = ProcessOutcome::default();
        for device in inventory.devices() {
            if outcome.session_lost || read.clock.now_boot_ns() >= read.plan.deadline_boot_ns {
                outcome.partial = true;
                continue;
            }
            if read.lost_devices.contains(&device.uuid) || !wants_device(read, &device.uuid) {
                outcome.partial |= read.lost_devices.contains(&device.uuid);
                continue;
            }
            self.read_device(backend, &device.uuid, read, &mut outcome);
        }
        outcome
    }

    fn read_device(
        &mut self,
        backend: &dyn GpuProcessQueries,
        uuid: &str,
        read: &mut ProcessRead<'_>,
        outcome: &mut ProcessOutcome,
    ) {
        let started = read.clock.now_boot_ns();
        let compute = list(read, uuid, COMPUTE_LIST_KEY, || {
            backend.compute_processes(uuid)
        });
        let graphics = list(read, uuid, GRAPHICS_LIST_KEY, || {
            backend.graphics_processes(uuid)
        });
        if note_fatal(read, uuid, &compute, outcome) || note_fatal(read, uuid, &graphics, outcome) {
            return;
        }
        if compute.is_err() && graphics.is_err() {
            outcome.partial = true;
            return;
        }
        let merged = merge_processes(
            compute.as_deref().unwrap_or_default(),
            graphics.as_deref().unwrap_or_default(),
        );
        outcome.partial |= merged.truncated || compute.is_err() || graphics.is_err();
        let utilization = self.utilization(backend, uuid, read, &merged);
        if note_fatal(read, uuid, &utilization, outcome) {
            return;
        }
        let finished = read.clock.now_boot_ns();
        let timing = (started, finished);
        for association in &merged.associations {
            outcome.records.extend(association_records(
                read,
                uuid,
                association,
                &utilization,
                timing,
            ));
        }
    }

    fn utilization(
        &mut self,
        backend: &dyn GpuProcessQueries,
        uuid: &str,
        read: &mut ProcessRead<'_>,
        merged: &MergedProcesses,
    ) -> BackendResult<BTreeMap<u32, u32>> {
        if merged.associations.is_empty() {
            return Ok(BTreeMap::new());
        }
        let query_key = Channel::ProcessUtilization.query_key();
        let now = read.clock.now_boot_ns();
        if let Admission::Skip(failure) = read.tracker.admit(uuid, &query_key, now) {
            return Err(failure);
        }
        let since = self.last_seen_us.get(uuid).copied().unwrap_or(0);
        let samples = backend.process_utilization(uuid, since);
        read.tracker.record(uuid, &query_key, &samples, now);
        let (latest, newest_timestamp) = latest_samples(&samples?);
        if let Some(timestamp) = newest_timestamp {
            self.last_seen_us
                .insert(uuid.to_owned(), timestamp.max(since));
        }
        Ok(latest)
    }
}

fn wants_device(read: &ProcessRead<'_>, uuid: &str) -> bool {
    if read.requested_slots.is_empty() {
        return true;
    }
    [Channel::ProcessMemory, Channel::ProcessUtilization]
        .into_iter()
        .filter_map(|channel| read.published.get(uuid, channel))
        .any(|descriptor| read.requested_slots.contains(&descriptor.slot))
}

fn list(
    read: &mut ProcessRead<'_>,
    uuid: &str,
    query_key: &str,
    query: impl FnOnce() -> BackendResult<Vec<GpuProcessEntry>>,
) -> BackendResult<Vec<GpuProcessEntry>> {
    let now = read.clock.now_boot_ns();
    if let Admission::Skip(failure) = read.tracker.admit(uuid, query_key, now) {
        return Err(failure);
    }
    let result = query();
    read.tracker.record(uuid, query_key, &result, now);
    result
}

/// Records lost GPUs / sessions; returns whether the device read must stop.
fn note_fatal<T>(
    read: &mut ProcessRead<'_>,
    uuid: &str,
    result: &BackendResult<T>,
    outcome: &mut ProcessOutcome,
) -> bool {
    match result {
        Err(BackendError::GpuLost) => {
            read.lost_devices.insert(uuid.to_owned());
            outcome.partial = true;
            true
        }
        Err(BackendError::Uninitialized) => {
            outcome.session_lost = true;
            outcome.partial = true;
            true
        }
        _ => false,
    }
}

fn association_records(
    read: &ProcessRead<'_>,
    uuid: &str,
    association: &Association,
    utilization: &BackendResult<BTreeMap<u32, u32>>,
    (started, finished): (u64, u64),
) -> Vec<Observation> {
    let mut records = Vec::with_capacity(2);
    let usage = association.usage.reason_code().to_owned();
    let memory = association.memory_bytes.ok_or_else(|| {
        (
            "NVML_PROCESS_MEMORY_UNAVAILABLE".to_owned(),
            ReadStatus::Unsupported,
        )
    });
    if let Some(record) = process_record(
        read,
        uuid,
        association.pid,
        Channel::ProcessMemory,
        memory,
        &usage,
        (started, finished),
    ) {
        records.push(record);
    }
    let sample = match utilization {
        Ok(latest) => u32::try_from(association.pid)
            .ok()
            .and_then(|pid| latest.get(&pid))
            .map(|percent| Ok(u64::from(*percent))),
        Err(error) => Some(Err((error.reason_code(), error.read_status()))),
    };
    if let Some(sample) = sample
        && let Some(record) = process_record(
            read,
            uuid,
            association.pid,
            Channel::ProcessUtilization,
            sample,
            &usage,
            (started, finished),
        )
    {
        records.push(record);
    }
    records
}

fn process_record(
    read: &ProcessRead<'_>,
    uuid: &str,
    pid: i32,
    channel: Channel,
    value: Result<u64, (String, ReadStatus)>,
    usage: &str,
    (started, finished): (u64, u64),
) -> Option<Observation> {
    let descriptor = read.published.get(uuid, channel)?;
    let key = process_key(read, uuid, pid, channel).ok()?;
    Some(match value {
        Ok(value) => RecordFactory::ok(
            read.plan,
            descriptor,
            key,
            Scalar::from_u64(value),
            Quality::Usable,
            started,
            finished,
            vec![usage.to_owned()],
        ),
        Err((reason, status)) => {
            let mut record = RecordFactory::failed(
                read.plan, descriptor, key, status, started, finished, reason,
            );
            record.reason_codes.push(usage.to_owned());
            record
        }
    })
}

fn process_key(
    read: &ProcessRead<'_>,
    uuid: &str,
    pid: i32,
    channel: Channel,
) -> Result<MetricKey, dgxtop_core::identity::IdentityError> {
    let entity: EntityId = naming::entity::gpu_pid(uuid, pid)?;
    let metric_id = match channel {
        Channel::ProcessUtilization => ids::PROCESS_GPU_UTILIZATION,
        _ => ids::PROCESS_GPU_MEMORY,
    };
    Ok(MetricKey::new(
        read.host_id.clone(),
        read.boot_id.clone(),
        entity,
        MetricId::new(metric_id)?,
        channel.scope(uuid)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pid: u32, memory: Option<u64>) -> GpuProcessEntry {
        GpuProcessEntry {
            pid,
            used_memory_bytes: memory,
        }
    }

    #[test]
    fn invalid_pids_are_dropped_and_lists_are_bounded() {
        let merged = merge_processes(&[entry(0, Some(1)), entry(u32::MAX, Some(1))], &[]);
        assert!(merged.associations.is_empty());
        assert_eq!(merged.invalid_pids, 2);
        let many: Vec<GpuProcessEntry> = (1..=2000).map(|pid| entry(pid, Some(1))).collect();
        let merged = merge_processes(&many, &[]);
        assert_eq!(merged.associations.len(), MAX_PROCESSES_PER_GPU);
        assert!(merged.truncated);
        assert_eq!(merged.associations[0].pid, 1);
    }

    #[test]
    fn newest_sample_per_pid_wins() {
        let samples = [
            ProcessUtilizationSample {
                pid: 1,
                timestamp_us: 10,
                sm_percent: 5,
            },
            ProcessUtilizationSample {
                pid: 1,
                timestamp_us: 30,
                sm_percent: 50,
            },
            ProcessUtilizationSample {
                pid: 2,
                timestamp_us: 20,
                sm_percent: 7,
            },
        ];
        let (latest, newest) = latest_samples(&samples);
        assert_eq!(latest.get(&1), Some(&50));
        assert_eq!(latest.get(&2), Some(&7));
        assert_eq!(newest, Some(30));
        assert_eq!(latest_samples(&[]), (BTreeMap::new(), None));
    }
}
