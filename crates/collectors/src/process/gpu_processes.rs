//! NVML per-process records grouped per GPU.
//!
//! Adapter convention: key `gpupid:<UUID>:<PID>` with scope `gpu:<UUID>`,
//! metrics `process.gpu.memory_allocated` (bytes; `unsupported` when the
//! figure is unavailable) and `process.gpu.utilization` (percent; absent when
//! not sampled), one catalog source per GPU and metric without a fixed key,
//! and the list membership as a `gpu_usage:*` reason code (compute and
//! graphics of one PID on one GPU already merged into one association).
//!
//! Only the newest process-list snapshot of each GPU counts: a retained
//! record from an older cycle belongs to a process that is no longer listed
//! and must not be summed or linked again.

use std::collections::BTreeMap;

use dgxtop_core::identity::{EntityId, MetricKey};
use dgxtop_core::metric::ids;
use dgxtop_core::naming::entity;

use crate::shared::context::{Record, Session};

/// Process metrics NVML reports per `(GPU, PID)`.
pub(crate) const PROCESS_GPU_METRICS: [&str; 2] =
    [ids::PROCESS_GPU_MEMORY, ids::PROCESS_GPU_UTILIZATION];

/// Records of one PID on one GPU, by metric.
#[derive(Debug, Clone, Default)]
pub(crate) struct PidRecords<'a> {
    pub memory: Vec<Record<'a>>,
    pub utilization: Vec<Record<'a>>,
}

impl<'a> PidRecords<'a> {
    pub fn all(&self) -> impl Iterator<Item = &Record<'a>> {
        self.memory.iter().chain(&self.utilization)
    }
}

/// Newest process-list snapshot of one GPU.
#[derive(Debug, Clone)]
pub(crate) struct GpuProcesses<'a> {
    pub gpu_entity: EntityId,
    /// Any record of the snapshot (host and boot of derived keys).
    pub reference_key: &'a MetricKey,
    pub by_pid: BTreeMap<i32, PidRecords<'a>>,
    /// A record of this GPU was delivered by the plan.
    pub has_new: bool,
    /// A retained record of this GPU reached its deadline now.
    pub expiring: bool,
    /// A record of this GPU came from a batch that does not cover the whole
    /// process list (partial NVML read or missing pages).
    pub incomplete: bool,
}

/// Groups the process records of every GPU. Records whose entity is not a
/// `gpupid:<UUID>:<PID>` spelling are returned separately.
pub(crate) fn group_by_gpu<'a>(
    session: &Session<'a>,
) -> (BTreeMap<EntityId, GpuProcesses<'a>>, Vec<Record<'a>>) {
    let mut invalid = Vec::new();
    let mut parsed: Vec<(EntityId, i32, Record<'a>)> = Vec::new();
    let mut gpu_entities: BTreeMap<&'a str, EntityId> = BTreeMap::new();
    let process_records =
        session.records_where(|key| PROCESS_GPU_METRICS.contains(&key.metric_id.as_str()));
    for record in process_records {
        match gpu_and_pid(record, &mut gpu_entities) {
            Some((gpu, pid)) => parsed.push((gpu, pid, record)),
            None => invalid.push(record),
        }
    }
    let mut newest_cycle: BTreeMap<EntityId, u64> = BTreeMap::new();
    for (gpu, _, record) in &parsed {
        let cycle = newest_cycle.entry(gpu.clone()).or_insert(0);
        *cycle = (*cycle).max(record.observation.cycle_id);
    }
    let mut groups: BTreeMap<EntityId, GpuProcesses<'a>> = BTreeMap::new();
    for (gpu, pid, record) in parsed {
        let group = groups.entry(gpu.clone()).or_insert_with(|| GpuProcesses {
            gpu_entity: gpu.clone(),
            reference_key: record.key(),
            by_pid: BTreeMap::new(),
            has_new: false,
            expiring: false,
            incomplete: false,
        });
        group.has_new |= record.is_new();
        group.expiring |= session.is_expiring(record.key());
        group.incomplete |= record.route.is_some_and(|route| route.incomplete);
        if newest_cycle.get(&gpu) != Some(&record.observation.cycle_id) {
            continue;
        }
        let records = group.by_pid.entry(pid).or_default();
        if record.key().metric_id.as_str() == ids::PROCESS_GPU_MEMORY {
            records.memory.push(record);
        } else {
            records.utilization.push(record);
        }
    }
    (groups, invalid)
}

/// GPU entity and PID of a `gpupid:<UUID>:<PID>` record. A job sees
/// thousands of these records but only a few GPUs, so each GPU entity is
/// built once and shared (`known`).
fn gpu_and_pid<'a>(
    record: Record<'a>,
    known: &mut BTreeMap<&'a str, EntityId>,
) -> Option<(EntityId, i32)> {
    let (uuid, pid) = entity::parse_gpu_pid(record.key().entity_id.as_str())?;
    if pid <= 0 {
        return None;
    }
    if let Some(gpu) = known.get(uuid) {
        return Some((gpu.clone(), pid));
    }
    let gpu = entity::gpu(uuid).ok()?;
    known.insert(uuid, gpu.clone());
    Some((gpu, pid))
}
