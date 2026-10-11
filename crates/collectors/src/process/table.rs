//! Process collector (AT09).
//!
//! Host resources are sampled ONCE per `ProcessKey` per cycle from one
//! procfs record per process (`process:<ns>:<pid>:<start>`, D-79, D-86):
//! its CPU ticks as the value, the rest of its `stat` as tags
//! ([`ProcessStat`]) and the name and command line in the provenance
//! channel. CPU% = Δticks / (CLK_TCK × dt) × 100 (may exceed 100 on several
//! cores); RSS bytes and the numeric uid come from the tags. GPU usage from NVML
//! (`gpupid:<UUID>:<PID>`) is joined afterwards as `(ProcessKey, GPU)` links:
//! host resources are never repeated or multiplied per GPU, compute and
//! graphics entries of one PID on one GPU merge into one link, and a PID is
//! only linked to the process instance that already existed when the GPU
//! list was read (PID-reuse guard). Each GPU process's descendants are
//! summed from the tree of every scanned process ([`ProcessTree`]) before
//! the table is bounded with a stable priority; process metrics never
//! become history series.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use dgxtop_core::canonical::{
    CollectionBatch, CollectionPlan, CounterBase, GpuUsageKind, ProcessBase, ProcessDescendants,
    ProcessGpuLink, ProcessRow, ProcessTable, RejectReason,
};
use dgxtop_core::clock::NANOS_PER_SECOND;
use dgxtop_core::identity::{AdapterId, CollectorId, EntityId, IdentityError, ProcessKey};
use dgxtop_core::metric::{self, Domain, MetricDescriptor, Scalar, ids, native};
use dgxtop_core::naming::{self, adapters, attributes, entity, groups};
use dgxtop_core::observation::BatchStatus;
use dgxtop_core::ports::Collector;
use dgxtop_core::process_stat::{self, ProcessStat};
use dgxtop_core::text::{sanitize_command_name, sanitize_display};

use crate::process::gpu_processes::{self, PidRecords};
use crate::process::tree::{DescendantUsage, ProcessTree, TreeNode};
use crate::shared::arbitration::{self, check_namespace};
use crate::shared::context::{self, CollectorSpec, Record, Route, Session};
use crate::shared::counter::{self, CounterReading};
use crate::shared::counter_inputs;
use crate::shared::output::Subject;
use crate::shared::reasons;
use crate::shared::state_bounds::PROCESS_BASES_PER_ROW;

fn accepts(metric: &MetricDescriptor) -> bool {
    metric.domain == Domain::Process && metric.id != ids::PROCESS_CPU_PERCENT
}

/// GPU process lists are retained so a table built from a later procfs
/// batch can still join them; procfs rows are rebuilt every cycle.
fn retains(route: &Route<'_>, _metric: &MetricDescriptor) -> bool {
    route.is(adapters::NVML, groups::PROCESS)
}

static SPEC: CollectorSpec = CollectorSpec {
    id: naming::collectors::PROCESS,
    accepts,
    retains,
};

/// Process table collector.
#[derive(Debug, Clone)]
pub struct ProcessCollector {
    id: CollectorId,
}

impl ProcessCollector {
    pub fn new() -> Result<Self, IdentityError> {
        Ok(Self {
            id: CollectorId::new(naming::collectors::PROCESS)?,
        })
    }
}

impl Collector for ProcessCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn consumes(&self, adapter_id: &AdapterId, read_group: &str) -> bool {
        crate::shared::routes_to(adapter_id, read_group, SPEC.id)
    }

    fn collect(&self, plan: &CollectionPlan) -> CollectionBatch {
        context::run(plan, &SPEC, collect_processes)
    }
}

fn collect_processes(session: &mut Session<'_>) {
    if !procfs_delivered(session) {
        return;
    }
    let clk_tck = clk_tck(session);
    let samples = host_samples(session);
    let tree = ProcessTree::of(
        samples
            .iter()
            .filter_map(|(key, sample)| tree_node(key, sample)),
    );
    let mut rows: Vec<ProcessRow> = Vec::with_capacity(samples.len());
    for (key, sample) in &samples {
        let (row, base) = host_row(session, key, sample, clk_tck);
        if let Some(base) = base {
            session.set_process(key.clone(), base);
        }
        rows.push(row);
    }
    attach_gpu_links(session, &mut rows, clk_tck);
    attach_descendants(session, &mut rows, &tree, clk_tck);
    rows.sort_by(priority);
    prune_bases(session, &rows);
    let table = bounded_table(session, rows);
    session.set_process_table(table);
}

/// A table is (re)built only when procfs process data arrived; a failed
/// read keeps the previous table instead of publishing an empty one.
fn procfs_delivered(session: &Session<'_>) -> bool {
    session.routes().iter().any(|(route, status)| {
        route.is(adapters::PROCFS, groups::PROCESS) && *status != BatchStatus::Failed
    })
}

/// `clk_tck` published by the catalog host entity; never defaulted.
fn clk_tck(session: &Session<'_>) -> Option<u64> {
    let host = EntityId::new(entity::HOST).ok()?;
    session
        .plan()
        .catalog
        .entity(&host)?
        .attribute(attributes::CLK_TCK)?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|ticks| *ticks > 0)
}

/// The newest procfs record of one process instance (D-79): its CPU ticks
/// as the value, the rest of its `stat` as tags, parsed once.
#[derive(Debug, Clone, Copy)]
struct HostSample<'a> {
    record: Record<'a>,
    stat: ProcessStat,
}

impl<'a> HostSample<'a> {
    fn of(record: Record<'a>) -> Self {
        Self {
            record,
            stat: ProcessStat::from_reason_codes(&record.observation.reason_codes),
        }
    }

    fn read_finished_boot_ns(&self) -> u64 {
        self.record.observation.read_finished_boot_ns
    }
}

/// Visible process instances of this cycle: procfs records of the policy
/// namespace, one per `ProcessKey` (the newest).
fn host_samples<'a>(session: &mut Session<'a>) -> BTreeMap<ProcessKey, HostSample<'a>> {
    let namespace = session.policy().namespace;
    let mut samples: BTreeMap<ProcessKey, HostSample<'a>> = BTreeMap::new();
    for record in session.all_records() {
        let from_procfs = record
            .route
            .is_some_and(|route| route.is(adapters::PROCFS, groups::PROCESS));
        if !from_procfs || record.key().metric_id.as_str() != ids::PROCESS_CPU_TICKS {
            continue;
        }
        if check_namespace(record.observation, namespace).is_err() {
            continue;
        }
        let key = record.key();
        let Some(process) =
            ProcessKey::from_entity_id(&key.host_id, &key.boot_id, key.entity_id.as_str())
        else {
            session.failure(
                reasons::INVALID_ENTITY,
                &format!("not a process entity: {}", key.entity_id),
                Subject::source(record.source_id()),
            );
            continue;
        };
        let newer = samples.get(&process).is_none_or(|current| {
            record.observation.read_finished_boot_ns >= current.read_finished_boot_ns()
        });
        if newer {
            samples.insert(process, HostSample::of(record));
        }
    }
    samples
}

/// The display name in a process record's channel (D-73).
fn display_name_of(channel: &str) -> &str {
    channel
        .split(naming::PROCESS_COMMAND_SEPARATOR)
        .next()
        .unwrap_or(channel)
}

/// The command line a process record's channel carries after the name
/// (D-73), cleaned and bounded; empty when there is none.
fn command_of(channel: &str) -> String {
    channel
        .split_once(naming::PROCESS_COMMAND_SEPARATOR)
        .map(|(_, command)| sanitize_display(command, naming::MAX_CHANNEL_BYTES))
        .unwrap_or_default()
}

/// One host row plus the next CPU tick base of the process.
fn host_row(
    session: &Session<'_>,
    key: &ProcessKey,
    sample: &HostSample<'_>,
    clk_tck: Option<u64>,
) -> (ProcessRow, Option<ProcessBase>) {
    let record = sample.record;
    let (cpu_percent, cpu_reason, base) = cpu_percent(session, key, record, clk_tck);
    let admitted = admits_tags(session, record);
    let (rss_bytes, rss_reason) = tagged_rss(session, sample, admitted);
    let channel = &record.observation.provenance.channel;
    let row = ProcessRow {
        key: key.clone(),
        name: sanitize_command_name(display_name_of(channel)),
        command: command_of(channel),
        uid: sample.stat.uid.filter(|_| admitted.is_ok()),
        state: sample.stat.state,
        cpu_percent,
        rss_bytes,
        missing_reason: cpu_reason.or(rss_reason).map(str::to_owned),
        gpu: Vec::new(),
        descendants: None,
        sample_kind: record.observation.sample_kind,
        read_finished_boot_ns: sample.read_finished_boot_ns(),
    };
    (row, base)
}

/// Whether the tags of a compact record may be used: its source is
/// admitted. Its read status speaks of the ticks only, so failed ticks do
/// not discard the RSS and owner read from the same `stat`.
fn admits_tags(session: &Session<'_>, record: Record<'_>) -> Result<(), &'static str> {
    let candidate = session.candidate(record);
    match arbitration::check_admission(&candidate, record.key(), session.policy()) {
        Ok(()) | Err(RejectReason::ReadFailed { .. }) => Ok(()),
        Err(reason) => Err(reasons::reject_code(
            &reason,
            record.observation.native_unit.as_str(),
        )),
    }
}

/// The RSS a compact record's tags carry, already in bytes; the adapter's
/// reason when it could not read it.
fn tagged_rss(
    session: &Session<'_>,
    sample: &HostSample<'_>,
    admitted: Result<(), &'static str>,
) -> (Option<u64>, Option<&'static str>) {
    if let Err(code) = admitted {
        return (None, Some(code));
    }
    if !counter_inputs::is_fresh(session, sample.record) {
        return (None, Some(reasons::STALE));
    }
    match sample.stat.rss_bytes {
        Some(bytes) => (Some(bytes), None),
        None => (
            None,
            Some(
                process_stat::rss_failure(&sample.record.observation.reason_codes)
                    .unwrap_or(reasons::MISSING_INPUT),
            ),
        ),
    }
}

/// CPU% of one instance. The first sample of a `ProcessKey` (and therefore of
/// a reused PID with a new start time) has no base: warmup, never another
/// incarnation's ticks.
fn cpu_percent(
    session: &Session<'_>,
    key: &ProcessKey,
    record: Record<'_>,
    clk_tck: Option<u64>,
) -> (Option<f64>, Option<&'static str>, Option<ProcessBase>) {
    let ticks = match counter_inputs::validate(session, record, None) {
        Ok((ticks, _)) => ticks,
        Err(code) => return (None, Some(code), None),
    };
    let unit = record.observation.native_unit.as_str();
    if unit != native::TICKS && unit != native::COUNT {
        return (None, Some(reasons::UNKNOWN_UNIT), None);
    }
    let reading = CounterReading {
        value: ticks,
        boot_id: &key.boot_id,
        generation: record.observation.provenance.generation,
        sample_boot_ns: record.sample_time_boot_ns(),
        modulus: None,
    };
    let previous = session.committed_process(key).map(|base| CounterBase {
        value: base.ticks,
        boot_id: key.boot_id.clone(),
        generation: reading.generation,
        sample_boot_ns: base.sample_boot_ns,
    });
    let step = counter::advance(previous.as_ref(), &reading, session.stale_window_ns(record));
    let base = Some(ProcessBase {
        ticks,
        sample_boot_ns: reading.sample_boot_ns,
        descendant_ticks: None,
    });
    let delta = match step.outcome {
        Ok(delta) => delta,
        Err(gap) => return (None, Some(gap.reason_code()), base),
    };
    let Some(clk_tck) = clk_tck else {
        return (None, Some(reasons::CLK_TCK_UNKNOWN), base);
    };
    if !counter_inputs::is_fresh(session, record) {
        return (None, Some(reasons::STALE), base);
    }
    #[allow(clippy::cast_precision_loss)]
    let cpu_seconds = delta.delta as f64 / clk_tck as f64;
    match counter::rate_of(cpu_seconds, delta.elapsed_ns()) {
        Some(share) => (Some(share * 100.0), None, base),
        None => (None, Some(reasons::VALUE_OVERFLOW), base),
    }
}

// ----- descendants ----------------------------------------------------------

/// A process of the tree, from its compact record; one whose parent is
/// unknown is left out.
fn tree_node(key: &ProcessKey, sample: &HostSample<'_>) -> Option<(i64, TreeNode)> {
    let own_ticks = sample.record.observation.value.and_then(Scalar::as_u64);
    let node = TreeNode {
        parent: sample.stat.ppid?,
        own_ticks,
        reaped_ticks: sample.stat.reaped_ticks,
        rss_bytes: sample.stat.rss_bytes,
    };
    Some((i64::from(key.pid.get()), node))
}

/// What each GPU process's descendants use, their CPU since the last
/// table counted from the base its row keeps (D-77, D-79). Only GPU
/// processes: the process history follows no other.
fn attach_descendants(
    session: &mut Session<'_>,
    rows: &mut [ProcessRow],
    tree: &ProcessTree,
    clk_tck: Option<u64>,
) {
    for row in rows.iter_mut().filter(|row| !row.gpu.is_empty()) {
        let Some(usage) = tree.usage_of(i64::from(row.key.pid.get())) else {
            continue;
        };
        let now = usage
            .cpu_ticks
            .map(|ticks| (ticks, row.read_finished_boot_ns));
        let before = session
            .committed_process(&row.key)
            .and_then(|base| base.descendant_ticks);
        row.descendants = Some(descendants_of(usage, before, now, clk_tck));
        // An unknown total keeps the last known one as the next base.
        if let Some(base) = session.next_processes_mut().get_mut(&row.key) {
            base.descendant_ticks = now.or(before);
        }
    }
}

/// The descendants' count and memory as read, and their CPU since
/// `before` as percent of one core; `now` is `None` when their time is
/// unknown. Their time drops when one leaves the tree unreaped (a daemon,
/// an orphan): that table gives no rate, and the next one counts from it.
fn descendants_of(
    usage: DescendantUsage,
    before: Option<(u64, u64)>,
    now: Option<(u64, u64)>,
    clk_tck: Option<u64>,
) -> ProcessDescendants {
    #[allow(clippy::cast_precision_loss)]
    let cpu_percent = before
        .zip(now)
        .and_then(|((before_ticks, before_at), (ticks, at))| {
            let spent = ticks.checked_sub(before_ticks)?;
            let elapsed = at.checked_sub(before_at).filter(|elapsed| *elapsed > 0)?;
            let per_second = clk_tck.filter(|ticks| *ticks > 0)?;
            Some(
                spent as f64 * 100.0 * NANOS_PER_SECOND as f64 / per_second as f64 / elapsed as f64,
            )
        });
    ProcessDescendants {
        processes: usage.processes,
        cpu_percent,
        rss_bytes: usage.rss_bytes,
    }
}

// ----- GPU links ------------------------------------------------------------

fn attach_gpu_links(session: &mut Session<'_>, rows: &mut [ProcessRow], clk_tck: Option<u64>) {
    let (groups, invalid) = gpu_processes::group_by_gpu(session);
    for record in invalid {
        session.failure(
            reasons::INVALID_ENTITY,
            &format!("not a gpupid entity: {}", record.key().entity_id),
            Subject::source(record.source_id()),
        );
    }
    if groups.values().all(|group| group.by_pid.is_empty()) {
        return;
    }
    let Some(clk_tck) = clk_tck else {
        // Without CLK_TCK the PID-reuse guard cannot run: no unsafe joins.
        session.failure(
            reasons::CLK_TCK_UNKNOWN,
            "GPU process links skipped: clk_tck unknown, PID reuse cannot be excluded",
            Subject::none(),
        );
        return;
    };
    for group in groups.values().filter(|group| group.incomplete) {
        session.failure(
            reasons::GPU_PROCESS_LIST_INCOMPLETE,
            &format!(
                "{}: links joined from an incomplete process list",
                group.gpu_entity
            ),
            Subject::none(),
        );
    }
    let mut by_pid: BTreeMap<i32, Vec<usize>> = BTreeMap::new();
    for (index, row) in rows.iter().enumerate() {
        by_pid.entry(row.key.pid.get()).or_default().push(index);
    }
    for group in groups.values() {
        for (pid, records) in &group.by_pid {
            let Some(candidates) = by_pid.get(pid) else {
                continue;
            };
            let Some(row) = guarded_owner(rows, candidates, records, clk_tck) else {
                continue;
            };
            let link = gpu_link(session, &group.gpu_entity, records);
            rows[row].gpu.push(link);
        }
    }
    for row in rows.iter_mut() {
        row.gpu
            .sort_by(|left, right| left.gpu_entity_id.cmp(&right.gpu_entity_id));
    }
}

/// The instance with this PID that had started when the GPU list was read
/// (latest such start wins); `None` when every candidate started later.
fn guarded_owner(
    rows: &[ProcessRow],
    candidates: &[usize],
    records: &PidRecords<'_>,
    clk_tck: u64,
) -> Option<usize> {
    let gpu_read = records
        .all()
        .map(|record| record.observation.read_finished_boot_ns)
        .max()?;
    candidates
        .iter()
        .copied()
        .filter(|index| {
            start_boot_ns(&rows[*index].key, clk_tck).is_some_and(|start| start <= gpu_read)
        })
        .max_by_key(|index| (rows[*index].key.start_time_ticks, rows[*index].key.clone()))
}

/// Process start on the boot clock: `start_time_ticks / CLK_TCK` seconds.
fn start_boot_ns(key: &ProcessKey, clk_tck: u64) -> Option<u64> {
    let nanos =
        u128::from(key.start_time_ticks) * u128::from(NANOS_PER_SECOND) / u128::from(clk_tck);
    u64::try_from(nanos).ok()
}

/// One merged link per `(ProcessKey, GPU)`.
fn gpu_link(session: &Session<'_>, gpu: &EntityId, records: &PidRecords<'_>) -> ProcessGpuLink {
    ProcessGpuLink {
        gpu_entity_id: gpu.clone(),
        usage: usage_of(records),
        utilization_percent: largest_valid(
            session,
            &records.utilization,
            ids::PROCESS_GPU_UTILIZATION,
        )
        .map(Scalar::to_f64_lossy),
        memory_bytes: largest_valid(session, &records.memory, ids::PROCESS_GPU_MEMORY)
            .and_then(Scalar::as_u64),
    }
}

/// Largest eligible reading among duplicate entries (compute + graphics
/// lists report the same context); unknown entries are skipped, never 0.
fn largest_valid(session: &Session<'_>, records: &[Record<'_>], metric_id: &str) -> Option<Scalar> {
    let descriptor = metric::descriptor(metric_id)?;
    records
        .iter()
        .filter_map(|record| {
            let candidate = session.candidate(*record);
            arbitration::assess(
                &candidate,
                record.key(),
                descriptor,
                session.policy(),
                session.now(),
            )
            .verdict
            .ok()
        })
        .max_by(|left, right| left.to_f64_lossy().total_cmp(&right.to_f64_lossy()))
}

/// NVML marks each association with a `gpu_usage:*` reason code; compute
/// and graphics entries of one PID on one GPU merge into one link.
fn usage_of(records: &PidRecords<'_>) -> GpuUsageKind {
    let mut compute = false;
    let mut graphics = false;
    for code in records
        .all()
        .flat_map(|record| &record.observation.reason_codes)
    {
        match code.as_str() {
            reasons::GPU_USAGE_COMPUTE => compute = true,
            reasons::GPU_USAGE_GRAPHICS => graphics = true,
            reasons::GPU_USAGE_COMPUTE_AND_GRAPHICS => {
                compute = true;
                graphics = true;
            }
            _ => {}
        }
    }
    match (compute, graphics) {
        (true, true) => GpuUsageKind::ComputeAndGraphics,
        (true, false) => GpuUsageKind::Compute,
        (false, true) => GpuUsageKind::Graphics,
        (false, false) => GpuUsageKind::Unknown,
    }
}

// ----- ordering and bounds --------------------------------------------------

/// GPU-associated first, then CPU% and RSS descending (unknown last), then
/// `ProcessKey`: stable and independent of input order.
fn priority(left: &ProcessRow, right: &ProcessRow) -> Ordering {
    (!right.gpu.is_empty())
        .cmp(&!left.gpu.is_empty())
        .then_with(|| descending(left.cpu_percent, right.cpu_percent, f64::total_cmp))
        .then_with(|| descending(left.rss_bytes, right.rss_bytes, |a, b| a.cmp(b)))
        .then_with(|| left.key.cmp(&right.key))
}

fn descending<T: Copy>(
    left: Option<T>,
    right: Option<T>,
    compare: impl Fn(&T, &T) -> Ordering,
) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => compare(&right, &left),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Keeps at most `PROCESS_BASES_PER_ROW × max_processes` bases: entries not
/// seen this cycle go first (oldest first), then the lowest-priority rows.
fn prune_bases(session: &mut Session<'_>, rows: &[ProcessRow]) {
    let cap = usize::try_from(session.plan().limits.max_processes)
        .unwrap_or(usize::MAX)
        .saturating_mul(PROCESS_BASES_PER_ROW);
    let rank: BTreeMap<&ProcessKey, usize> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| (&row.key, index))
        .collect();
    let bases = session.next_processes_mut();
    let excess = bases.len().saturating_sub(cap);
    if excess == 0 {
        return;
    }
    let mut order: Vec<(bool, u64, usize, ProcessKey)> = bases
        .iter()
        .map(|(key, base)| match rank.get(key) {
            None => (false, base.sample_boot_ns, 0, key.clone()),
            Some(index) => (true, 0, usize::MAX - index, key.clone()),
        })
        .collect();
    order.sort();
    for (_, _, _, key) in order.into_iter().take(excess) {
        bases.remove(&key);
    }
}

fn bounded_table(session: &Session<'_>, mut rows: Vec<ProcessRow>) -> ProcessTable {
    let limits = session.plan().limits;
    let max_rows = usize::try_from(limits.max_processes).unwrap_or(usize::MAX);
    let max_links = usize::try_from(limits.max_process_gpu_links).unwrap_or(usize::MAX);
    let total_seen = u32::try_from(rows.len()).unwrap_or(u32::MAX);
    let truncated = rows.len() > max_rows;
    rows.truncate(max_rows);
    let mut links = 0usize;
    let mut gpu_links_truncated = false;
    for row in &mut rows {
        let room = max_links.saturating_sub(links);
        if row.gpu.len() > room {
            row.gpu.truncate(room);
            gpu_links_truncated = true;
        }
        links += row.gpu.len();
    }
    ProcessTable {
        read_finished_boot_ns: rows
            .iter()
            .map(|row| row.read_finished_boot_ns)
            .max()
            .unwrap_or(0),
        rows,
        truncated,
        gpu_links_truncated,
        total_seen,
        cycle_id: session.plan().cycle_id,
    }
}

#[cfg(test)]
mod tests;
