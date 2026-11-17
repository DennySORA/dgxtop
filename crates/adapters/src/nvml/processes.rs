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
//! * `process.gpu.encoder` and `process.gpu.decoder` (percent, D-139): the
//!   video encoder's and decoder's share in that same sample, so they come
//!   and go with the utilization. A failed query is told once, by the
//!   utilization's record.
//!
//! A share above 100 % is a failed record (`NVML_PROCESS_SHARE_OUT_OF_RANGE`),
//! never a figure: the driver has answered inconsistent samples.
//!
//! The usage kind travels as reason code `gpu_usage:<kind>` so the process
//! collector can fill `ProcessGpuLink::usage` without a second source.
//!
//! A GPU whose two lists were read and name no process gets one record of
//! `gpu.memory.process_allocated`, 0 bytes (D-119): nothing else tells the
//! collector that the lists were read and are empty, not unread. A graphics
//! list the device does not support counts as read and empty (a
//! compute-only device lists no graphics context). A list that failed gets
//! a failed record there instead, so the collector drops the last 0 at
//! once.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{BootId, EntityId, HostId, MetricId, MetricKey};
use dgxtop_core::metric::Scalar;
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
/// The compute list's query, whose support the process memory's channels
/// report (D-119).
pub(super) const COMPUTE_LIST_KEY: &str = "process.compute";
const GRAPHICS_LIST_KEY: &str = "process.graphics";
/// The channels the process group serves.
const PROCESS_CHANNELS: [Channel; 5] = [
    Channel::ProcessMemory,
    Channel::ProcessUtilization,
    Channel::ProcessEncoder,
    Channel::ProcessDecoder,
    Channel::ProcessMemoryTotal,
];
/// One of a sample's shares, in percent.
type SampleShare = fn(&ProcessUtilizationSample) -> u32;
/// The channels one utilization sample answers and the share each reads
/// (D-139).
const SAMPLED_CHANNELS: [(Channel, SampleShare); 3] = [
    (Channel::ProcessUtilization, |sample| sample.sm_percent),
    (Channel::ProcessEncoder, |sample| sample.encoder_percent),
    (Channel::ProcessDecoder, |sample| sample.decoder_percent),
];
/// The whole sample period. The driver has answered inconsistent samples
/// with shares above it, which nvtop drops as well.
const FULL_SHARE_PERCENT: u32 = 100;
const SHARE_OUT_OF_RANGE: &str = "NVML_PROCESS_SHARE_OUT_OF_RANGE";

/// The newest utilization sample of each PID of one GPU.
pub type LatestSamples = BTreeMap<u32, ProcessUtilizationSample>;

/// How a process uses one GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageKind {
    Compute,
    Graphics,
    ComputeAndGraphics,
}

impl UsageKind {
    #[must_use]
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
    /// Entries with a PID outside `1..=i32::MAX` (cannot form a `ProcessKey`).
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

/// The newest sample of each PID plus the newest timestamp seen.
pub fn latest_samples(samples: &[ProcessUtilizationSample]) -> (LatestSamples, Option<u64>) {
    let mut newest = LatestSamples::new();
    for sample in samples {
        let entry = newest.entry(sample.pid).or_insert(*sample);
        if sample.timestamp_us >= entry.timestamp_us {
            *entry = *sample;
        }
    }
    let max_timestamp = samples.iter().map(|sample| sample.timestamp_us).max();
    (newest, max_timestamp)
}

/// What each sampled channel reads of `sample`.
fn sampled_shares(
    sample: &ProcessUtilizationSample,
) -> impl Iterator<Item = (Channel, Result<u64, (String, ReadStatus)>)> + '_ {
    SAMPLED_CHANNELS
        .into_iter()
        .map(move |(channel, share)| (channel, share_of(share(sample))))
}

/// A sampled share as a record's value; one above 100 % is a failed read,
/// never a figure.
fn share_of(percent: u32) -> Result<u64, (String, ReadStatus)> {
    if percent <= FULL_SHARE_PERCENT {
        Ok(u64::from(percent))
    } else {
        Err((SHARE_OUT_OF_RANGE.to_owned(), ReadStatus::Error))
    }
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
            let timing = (started, read.clock.now_boot_ns());
            // A failure, the compute list's: never 0.
            let failed = lists_read(&compute, &graphics).map(|()| 0);
            outcome
                .records
                .extend(process_memory_total(read, uuid, failed, timing));
            return;
        }
        let merged = merge_processes(
            compute.as_deref().unwrap_or_default(),
            graphics.as_deref().unwrap_or_default(),
        );
        // A graphics list the device does not support leaves the read whole.
        outcome.partial |= merged.truncated || lists_read(&compute, &graphics).is_err();
        let utilization = self.utilization(backend, uuid, read, &merged);
        if note_fatal(read, uuid, &utilization, outcome) {
            return;
        }
        let finished = read.clock.now_boot_ns();
        let timing = (started, finished);
        let total = match lists_read(&compute, &graphics) {
            Ok(()) if merged.associations.is_empty() && merged.invalid_pids == 0 => Some(Ok(0)),
            // Only PIDs no process key holds: memory held unseen.
            Ok(()) if merged.associations.is_empty() => Some(Err((
                "NVML_PROCESS_PID_INVALID".to_owned(),
                ReadStatus::Error,
            ))),
            // The collector sums the processes listed.
            Ok(()) => None,
            Err(failure) => Some(Err(failure)),
        };
        if let Some(total) = total {
            outcome
                .records
                .extend(process_memory_total(read, uuid, total, timing));
        }
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
    ) -> BackendResult<LatestSamples> {
        if merged.associations.is_empty() {
            return Ok(LatestSamples::new());
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
    PROCESS_CHANNELS
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
    utilization: &BackendResult<LatestSamples>,
    timing: (u64, u64),
) -> Vec<Observation> {
    let usage = association.usage.reason_code().to_owned();
    let record = |channel: Channel, value: Result<u64, (String, ReadStatus)>| {
        process_record(read, uuid, association.pid, channel, value, &usage, timing)
    };
    let memory = association.memory_bytes.ok_or_else(|| {
        (
            "NVML_PROCESS_MEMORY_UNAVAILABLE".to_owned(),
            ReadStatus::Unsupported,
        )
    });
    let mut records = Vec::with_capacity(1 + SAMPLED_CHANNELS.len());
    records.extend(record(Channel::ProcessMemory, memory));
    match utilization {
        Ok(latest) => {
            // Without a new sample of the PID there is no record, never 0.
            let sample = u32::try_from(association.pid)
                .ok()
                .and_then(|pid| latest.get(&pid));
            for (channel, share) in sample.into_iter().flat_map(sampled_shares) {
                records.extend(record(channel, share));
            }
        }
        // The one query failed: its reason travels once, as the
        // utilization's.
        Err(error) => records.extend(record(
            Channel::ProcessUtilization,
            Err((error.reason_code(), error.read_status())),
        )),
    }
    records
}

/// Whether both of a GPU's lists were read; a graphics list the device does
/// not support counts as read and empty. Else the first failure's reason
/// and status.
fn lists_read(
    compute: &BackendResult<Vec<GpuProcessEntry>>,
    graphics: &BackendResult<Vec<GpuProcessEntry>>,
) -> Result<(), (String, ReadStatus)> {
    let failure = |error: &BackendError| (error.reason_code(), error.read_status());
    if let Err(error) = compute {
        return Err(failure(error));
    }
    match graphics {
        Ok(_) | Err(BackendError::NotSupported) => Ok(()),
        Err(error) => Err(failure(error)),
    }
}

/// What a GPU's processes hold together, as its lists say it (D-119): 0
/// bytes when they name none, or why they could not be read.
fn process_memory_total(
    read: &ProcessRead<'_>,
    uuid: &str,
    value: Result<u64, (String, ReadStatus)>,
    (started, finished): (u64, u64),
) -> Option<Observation> {
    let descriptor = read.published.get(uuid, Channel::ProcessMemoryTotal)?;
    let key = descriptor.metric.clone()?;
    Some(match value {
        Ok(bytes) => RecordFactory::ok(
            read.plan,
            descriptor,
            key,
            Scalar::from_u64(bytes),
            Quality::Usable,
            started,
            finished,
            Vec::new(),
        ),
        Err((reason, status)) => RecordFactory::failed(
            read.plan, descriptor, key, status, started, finished, reason,
        ),
    })
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
    Ok(MetricKey::new(
        read.host_id.clone(),
        read.boot_id.clone(),
        entity,
        MetricId::new(channel.metric_id())?,
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
        assert_eq!(merged.associations.len(), 0);
        assert_eq!(merged.invalid_pids, 2);
        let many: Vec<GpuProcessEntry> = (1..=2000).map(|pid| entry(pid, Some(1))).collect();
        let merged = merge_processes(&many, &[]);
        assert_eq!(merged.associations.len(), MAX_PROCESSES_PER_GPU);
        assert!(merged.truncated);
        assert_eq!(merged.associations[0].pid, 1);
    }

    fn sample(pid: u32, timestamp_us: u64, shares: [u32; 3]) -> ProcessUtilizationSample {
        let [sm_percent, encoder_percent, decoder_percent] = shares;
        ProcessUtilizationSample {
            pid,
            timestamp_us,
            sm_percent,
            encoder_percent,
            decoder_percent,
        }
    }

    /// The newest sample of a PID is taken whole: its encoder and decoder
    /// shares never come from an older sample than its utilization.
    #[test]
    fn newest_sample_per_pid_wins() {
        let samples = [
            sample(1, 10, [5, 90, 80]),
            sample(1, 30, [50, 12, 0]),
            sample(2, 20, [7, 0, 33]),
        ];
        let (latest, newest) = latest_samples(&samples);
        assert_eq!(latest.get(&1), Some(&sample(1, 30, [50, 12, 0])));
        assert_eq!(latest.get(&2), Some(&sample(2, 20, [7, 0, 33])));
        assert_eq!(newest, Some(30));
        assert_eq!(latest_samples(&[]), (LatestSamples::new(), None));
    }

    /// One sample answers the three sampled channels, each with its own
    /// share; a share above the whole period is a failed read of that
    /// channel only.
    #[test]
    fn a_sample_reads_each_channel_and_rejects_impossible_shares() {
        let shares: Vec<_> = sampled_shares(&sample(1, 10, [100, 12, 0])).collect();
        assert_eq!(
            shares,
            [
                (Channel::ProcessUtilization, Ok(100)),
                (Channel::ProcessEncoder, Ok(12)),
                (Channel::ProcessDecoder, Ok(0)),
            ]
        );
        let out_of_range = || Err((SHARE_OUT_OF_RANGE.to_owned(), ReadStatus::Error));
        let shares: Vec<_> = sampled_shares(&sample(1, 10, [101, u32::MAX, 7])).collect();
        assert_eq!(
            shares,
            [
                (Channel::ProcessUtilization, out_of_range()),
                (Channel::ProcessEncoder, out_of_range()),
                (Channel::ProcessDecoder, Ok(7)),
            ]
        );
    }
}
