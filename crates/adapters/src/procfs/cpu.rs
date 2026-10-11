//! `cpu` read group: `/proc/stat` time counters for the aggregate line and
//! every logical CPU (identified by its `cpuN` label, never by line order),
//! `/proc/loadavg`, and cpufreq. `cpuinfo_max_freq` is static metadata read
//! at discovery and cached per CPU generation, then served with each read's
//! time; a missing or denied cpufreq is re-probed at most once per re-probe
//! interval. The current frequency is `cpuinfo_avg_freq` where the kernel
//! offers it, else `scaling_cur_freq` (D-67).

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{EntityId, Scope};
use dgxtop_core::metric::{Scalar, ids, native};
use dgxtop_core::naming::{entity, groups};
use dgxtop_core::observation::{Capability, EntityKind, Observation, ReadPlan, SourceDescriptor};
use dgxtop_core::quality::{Quality, RawKind, ReadStatus, Support};

use super::catalog::{
    EntityParts, Incarnation, KeyContext, LiveSource, Selected, SnapshotBuilder, SourceDraft,
    SourceSpec, source_draft,
};
use super::host::host_entity_id;
use super::locator::{
    CurrentFrequency, DRIVER, FAMILY_CPU, LoadField, ProcfsLocator, RANK_CORE, RANK_CORE_FREQUENCY,
    RANK_HOST_WIDE,
};
use super::mechanics::{ReadSnapshot, ReprobeGate, TimedRead, invariant, io_failure, timed};
use super::parse::{CpuLabel, CpuTimes, STAT_COLUMNS, StatColumn, parse_cpu_times, parse_loadavg};
use super::records::{
    ReadWindow, deferred_record, f64_field_record, failed_record, io_failed_record, ok_record,
    removed_record, u64_field_record, u64_text_record,
};
use crate::common::{SystemFiles, parse_u64};

pub(crate) const STAT_PATH: &str = "/proc/stat";
const LOADAVG_PATH: &str = "/proc/loadavg";
/// `scaling_cur_freq` semantics depend on the cpufreq driver; it is not
/// guaranteed to be an instantaneous hardware clock.
const CURRENT_FREQUENCY_NOTE: &str = "SCALING_CUR_FREQ_DRIVER_DEFINED";
/// `cpuinfo_avg_freq` averages the CPU's clock over its last moments from
/// its hardware counters.
const AVERAGE_FREQUENCY_NOTE: &str = "CPUINFO_AVG_FREQ_COUNTERS";
const CPU_NOT_ONLINE: &str = "CPU_NOT_ONLINE";

fn cpufreq_path(cpu: u32, file: &str) -> String {
    format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/{file}")
}

/// cpufreq support of one CPU incarnation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CpufreqState {
    Supported {
        generation: u32,
        maximum_khz: u64,
        current: CurrentFrequency,
    },
    Unsupported {
        generation: u32,
        status: ReadStatus,
        reason: String,
    },
}

impl CpufreqState {
    fn generation(&self) -> u32 {
        match self {
            Self::Supported { generation, .. } | Self::Unsupported { generation, .. } => {
                *generation
            }
        }
    }
}

/// Per-CPU cpufreq metadata cache and the probe gate for unsupported or
/// denied cpufreq.
#[derive(Debug, Clone)]
pub(crate) struct CpufreqCache {
    entries: BTreeMap<u32, CpufreqState>,
    gate: ReprobeGate,
}

impl CpufreqCache {
    pub(crate) fn new(unsupported_probe_ms: u64) -> Self {
        Self {
            entries: BTreeMap::new(),
            gate: ReprobeGate::new(unsupported_probe_ms),
        }
    }

    /// Cached state for `generation`, re-probing only when the generation
    /// changed or a sticky failure's re-probe is due.
    fn state(&mut self, prober: &CpufreqProber<'_>, cpu: u32, generation: u32) -> CpufreqState {
        let key = format!("cpufreq:cpu{cpu}");
        let now = prober.clock.now_boot_ns();
        if let Some(cached) = self.cached(&key, cpu, generation, now) {
            return cached;
        }
        let state = prober.probe(cpu, generation);
        match &state {
            CpufreqState::Supported { .. } => self.gate.record_success(&key),
            CpufreqState::Unsupported { status, reason, .. } => {
                self.gate.record_failure(&key, now, *status, reason);
            }
        }
        self.entries.insert(cpu, state.clone());
        state
    }

    /// The cached state while it is valid: supported for this generation,
    /// or unsupported with its re-probe not yet due.
    fn cached(&mut self, key: &str, cpu: u32, generation: u32, now: u64) -> Option<CpufreqState> {
        let state = self.entries.get(&cpu)?;
        if state.generation() != generation {
            // A re-created CPU (driver restart, hotplug) is probed at once.
            self.gate.forget(key);
            return None;
        }
        match state {
            CpufreqState::Supported { .. } => Some(state.clone()),
            CpufreqState::Unsupported { .. } => self.gate.deferred(key, now).map(|_| state.clone()),
        }
    }

    fn retain(&mut self, online: &BTreeSet<u32>) {
        self.entries.retain(|cpu, _| online.contains(cpu));
        self.gate.retain(|key| {
            key.strip_prefix("cpufreq:cpu")
                .and_then(|cpu| cpu.parse::<u32>().ok())
                .is_some_and(|cpu| online.contains(&cpu))
        });
    }
}

struct CpufreqProber<'a> {
    files: &'a dyn SystemFiles,
    clock: &'a dyn TimeProvider,
}

impl CpufreqProber<'_> {
    /// The averaged frequency where the kernel offers it (D-67).
    fn current_frequency(&self, cpu: u32) -> CurrentFrequency {
        let average = cpufreq_path(cpu, CurrentFrequency::Average.file());
        if self.files.exists(&average) {
            CurrentFrequency::Average
        } else {
            CurrentFrequency::Scaling
        }
    }

    fn probe(&self, cpu: u32, generation: u32) -> CpufreqState {
        let path = cpufreq_path(cpu, "cpuinfo_max_freq");
        let read = timed(self.clock, || self.files.read_to_string(&path));
        let unsupported = |status: ReadStatus, reason: String| CpufreqState::Unsupported {
            generation,
            status,
            reason,
        };
        match &read.result {
            Ok(text) => match parse_u64(text) {
                Some(maximum_khz) => CpufreqState::Supported {
                    generation,
                    maximum_khz,
                    current: self.current_frequency(cpu),
                },
                None => unsupported(ReadStatus::Unsupported, "CPUFREQ_MAX_UNPARSABLE".to_owned()),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                unsupported(ReadStatus::Unsupported, "CPUFREQ_NOT_PRESENT".to_owned())
            }
            Err(error) => {
                let (status, reason) = io_failure(error);
                unsupported(status, reason)
            }
        }
    }
}

/// Inputs of CPU discovery besides the snapshot builder.
pub(crate) struct CpuDiscovery<'a> {
    pub keys: &'a KeyContext,
    pub files: &'a dyn SystemFiles,
    pub clock: &'a dyn TimeProvider,
}

/// Adds the aggregate CPU and every online logical CPU; on an unreadable
/// `/proc/stat` the previous CPU entities are carried over.
pub(crate) fn discover(
    builder: &mut SnapshotBuilder<'_, ProcfsLocator>,
    discovery: &CpuDiscovery<'_>,
    cache: &mut CpufreqCache,
) -> Result<(), io::Error> {
    let text = match discovery.files.read_to_string(STAT_PATH) {
        Ok(text) => text,
        Err(error) => {
            builder.carry_over_family(FAMILY_CPU);
            builder.error(format!("PROC_STAT_UNREADABLE:{}", io_failure(&error).1));
            return Err(error);
        }
    };
    let prober = CpufreqProber {
        files: discovery.files,
        clock: discovery.clock,
    };
    let mut online = BTreeSet::new();
    let mut supported_cores = 0usize;
    for line in parse_cpu_times(&text) {
        match line.label {
            CpuLabel::Aggregate => add_aggregate(builder, discovery.keys),
            CpuLabel::Core(cpu) => {
                online.insert(cpu);
                let supported = add_core(builder, discovery.keys, &prober, cache, cpu);
                supported_cores += usize::from(supported);
            }
        }
    }
    cache.retain(&online);
    builder.capability(frequency_summary(supported_cores, online.len()));
    Ok(())
}

fn aggregate_entity_id() -> EntityId {
    invariant(EntityId::new(entity::CPU_AGGREGATE), entity::CPU_AGGREGATE)
}

fn add_aggregate(builder: &mut SnapshotBuilder<'_, ProcfsLocator>, keys: &KeyContext) {
    let entity_id = aggregate_entity_id();
    let Some(incarnation) = builder.incarnation(&entity_id, entity::CPU_AGGREGATE) else {
        return;
    };
    let scope = invariant(Scope::new(entity::SCOPE_HOST), entity::SCOPE_HOST);
    let sources = stat_sources(builder, keys, &entity_id, &scope, CpuLabel::Aggregate);
    let parts = EntityParts {
        entity_id,
        kind: EntityKind::Host,
        parent_id: Some(host_entity_id()),
        display_name: "All CPUs".to_owned(),
        attributes: Vec::new(),
    };
    builder.add(FAMILY_CPU, parts, incarnation, sources);
}

/// Adds `cpu.core.N`; returns whether cpufreq is supported for it.
fn add_core(
    builder: &mut SnapshotBuilder<'_, ProcfsLocator>,
    keys: &KeyContext,
    prober: &CpufreqProber<'_>,
    cache: &mut CpufreqCache,
    cpu: u32,
) -> bool {
    let (Ok(entity_id), Ok(scope)) = (entity::cpu_core(cpu), entity::scope_core(cpu)) else {
        builder.error("CPU_IDENTITY_INVALID");
        return false;
    };
    let Some(incarnation) = builder.incarnation(&entity_id, &format!("cpu{cpu}")) else {
        return false;
    };
    let mut sources = stat_sources(builder, keys, &entity_id, &scope, CpuLabel::Core(cpu));
    let state = cache.state(prober, cpu, incarnation.generation);
    let supported = match &state {
        CpufreqState::Supported {
            maximum_khz,
            current,
            ..
        } => {
            sources.extend(frequency_sources(
                builder,
                keys,
                &entity_id,
                &scope,
                cpu,
                (*current, *maximum_khz),
            ));
            true
        }
        CpufreqState::Unsupported { reason, .. } => {
            for metric in [ids::CPU_FREQUENCY_CURRENT, ids::CPU_FREQUENCY_MAXIMUM] {
                builder.capability(core_capability(
                    metric,
                    &entity_id,
                    Support::Unsupported,
                    reason,
                ));
            }
            false
        }
    };
    add_core_entity(builder, entity_id, cpu, incarnation, sources);
    supported
}

fn add_core_entity(
    builder: &mut SnapshotBuilder<'_, ProcfsLocator>,
    entity_id: EntityId,
    cpu: u32,
    incarnation: Incarnation,
    sources: Vec<SourceDraft<ProcfsLocator>>,
) {
    let parts = EntityParts {
        entity_id,
        kind: EntityKind::CpuCore,
        parent_id: Some(aggregate_entity_id()),
        display_name: format!("CPU {cpu}"),
        attributes: Vec::new(),
    };
    builder.add(FAMILY_CPU, parts, incarnation, sources);
}

fn stat_sources(
    builder: &SnapshotBuilder<'_, ProcfsLocator>,
    keys: &KeyContext,
    entity_id: &EntityId,
    scope: &Scope,
    cpu: CpuLabel,
) -> Vec<SourceDraft<ProcfsLocator>> {
    let (device, rank) = match cpu {
        CpuLabel::Aggregate => ("cpu".to_owned(), RANK_HOST_WIDE),
        CpuLabel::Core(index) => (format!("cpu{index}"), RANK_CORE),
    };
    STAT_COLUMNS
        .into_iter()
        .filter_map(|column| {
            let origin = format!("procfs:{STAT_PATH}:{device}.{}", column.channel());
            let spec = SourceSpec {
                device_id: &device,
                channel: column.channel(),
                entity: entity_id,
                scope,
                metric_id: column.metric_id(),
                group: groups::CPU,
                native_unit: native::TICKS,
                raw_kind: RawKind::Counter,
                driver: DRIVER,
                device_path: STAT_PATH,
                origin: &origin,
                update_interval_ms: None,
                label: None,
                rank,
            };
            source_draft(
                builder.config(),
                keys,
                &spec,
                ProcfsLocator::Stat { cpu, column },
            )
        })
        .collect()
}

fn frequency_sources(
    builder: &SnapshotBuilder<'_, ProcfsLocator>,
    keys: &KeyContext,
    entity_id: &EntityId,
    scope: &Scope,
    cpu: u32,
    (current, maximum_khz): (CurrentFrequency, u64),
) -> Vec<SourceDraft<ProcfsLocator>> {
    let device = format!("cpu{cpu}");
    let channels = [
        (
            current.file(),
            ids::CPU_FREQUENCY_CURRENT,
            RawKind::Gauge,
            ProcfsLocator::FrequencyCurrent { cpu, current },
        ),
        (
            "cpuinfo_max_freq",
            ids::CPU_FREQUENCY_MAXIMUM,
            RawKind::Static,
            ProcfsLocator::FrequencyMaximum { khz: maximum_khz },
        ),
    ];
    channels
        .into_iter()
        .filter_map(|(channel, metric_id, raw_kind, locator)| {
            let path = cpufreq_path(cpu, channel);
            let origin = format!("procfs:{path}");
            let spec = SourceSpec {
                device_id: &device,
                channel,
                entity: entity_id,
                scope,
                metric_id,
                group: groups::CPU,
                native_unit: native::KHZ,
                raw_kind,
                driver: DRIVER,
                device_path: &path,
                origin: &origin,
                update_interval_ms: None,
                label: None,
                rank: RANK_CORE_FREQUENCY,
            };
            source_draft(builder.config(), keys, &spec, locator)
        })
        .collect()
}

fn core_capability(
    metric: &str,
    entity_id: &EntityId,
    support: Support,
    reason: &str,
) -> Capability {
    Capability {
        capability_id: metric.to_owned(),
        entity_id: Some(entity_id.clone()),
        support,
        reason: Some(reason.to_owned()),
        min_interval_ms: None,
    }
}

fn frequency_summary(supported: usize, online: usize) -> Capability {
    let (support, reason) = if supported > 0 {
        (
            Support::Supported,
            format!("CPUFREQ_CORES:{supported}/{online}"),
        )
    } else {
        (Support::Unsupported, "CPUFREQ_NOT_PRESENT".to_owned())
    };
    Capability {
        capability_id: ids::CPU_FREQUENCY_CURRENT.to_owned(),
        entity_id: None,
        support,
        reason: Some(reason),
        min_interval_ms: None,
    }
}

/// Everything the `cpu` group read needs.
pub(crate) struct CpuRead<'a> {
    pub plan: &'a ReadPlan,
    pub files: &'a dyn SystemFiles,
    pub clock: &'a dyn TimeProvider,
}

/// Reads only the files the selected sources need.
pub(crate) fn read(
    context: &CpuRead<'_>,
    selection: &[Selected<'_, ProcfsLocator>],
    gate: &mut ReprobeGate,
) -> ReadSnapshot {
    let started_boot_ns = context.clock.now_boot_ns();
    let shared = SharedCpuFiles::read(context, selection);
    let mut records = Vec::with_capacity(selection.len());
    for selected in selection {
        let record = match selected {
            Selected::Removed(descriptor) => {
                removed_record(context.plan, descriptor, context.clock.now_boot_ns())
            }
            Selected::Live(source) => shared.record(context, source, gate),
        };
        records.extend(record);
    }
    ReadSnapshot {
        records,
        started_boot_ns,
        finished_boot_ns: context.clock.now_boot_ns(),
        final_error: None,
    }
}

/// `/proc/stat` and `/proc/loadavg`, each read at most once per cycle and
/// shared by every source that needs them.
struct SharedCpuFiles {
    stat: Option<TimedRead<String>>,
    stat_lines: BTreeMap<CpuLabel, CpuTimes>,
    load: Option<TimedRead<String>>,
}

impl SharedCpuFiles {
    fn read(context: &CpuRead<'_>, selection: &[Selected<'_, ProcfsLocator>]) -> Self {
        let needs = |predicate: fn(&ProcfsLocator) -> bool| {
            selection.iter().any(
                |selected| matches!(selected, Selected::Live(source) if predicate(&source.locator)),
            )
        };
        let stat = needs(|locator| matches!(locator, ProcfsLocator::Stat { .. }))
            .then(|| timed(context.clock, || context.files.read_to_string(STAT_PATH)));
        let load = needs(|locator| matches!(locator, ProcfsLocator::Load(_)))
            .then(|| timed(context.clock, || context.files.read_to_string(LOADAVG_PATH)));
        let stat_lines = stat
            .as_ref()
            .and_then(|read| read.result.as_ref().ok())
            .map(|text| {
                parse_cpu_times(text)
                    .into_iter()
                    .map(|line| (line.label, line))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            stat,
            stat_lines,
            load,
        }
    }

    fn record(
        &self,
        context: &CpuRead<'_>,
        source: &LiveSource<ProcfsLocator>,
        gate: &mut ReprobeGate,
    ) -> Option<Observation> {
        let descriptor = &source.descriptor;
        match &source.locator {
            ProcfsLocator::Stat { cpu, column } => {
                self.stat_record(context.plan, descriptor, *cpu, *column)
            }
            ProcfsLocator::Load(field) => {
                load_record(context.plan, descriptor, self.load.as_ref(), *field)
            }
            ProcfsLocator::FrequencyCurrent { cpu, current } => {
                current_frequency_record(context, descriptor, (*cpu, *current), gate)
            }
            ProcfsLocator::FrequencyMaximum { khz } => static_record(context, descriptor, *khz),
            ProcfsLocator::Memory(_) | ProcfsLocator::Disk { .. } => None,
        }
    }

    fn stat_record(
        &self,
        plan: &ReadPlan,
        descriptor: &SourceDescriptor,
        cpu: CpuLabel,
        column: StatColumn,
    ) -> Option<Observation> {
        let read = self.stat.as_ref()?;
        let window = ReadWindow::from(read);
        if let Err(error) = &read.result {
            return io_failed_record(plan, descriptor, error, window);
        }
        match self.stat_lines.get(&cpu) {
            Some(times) => u64_field_record(plan, descriptor, times.column(column), window),
            None => failed_record(
                plan,
                descriptor,
                ReadStatus::Removed,
                CPU_NOT_ONLINE,
                window,
            ),
        }
    }
}

fn load_record(
    plan: &ReadPlan,
    descriptor: &SourceDescriptor,
    load: Option<&TimedRead<String>>,
    field: LoadField,
) -> Option<Observation> {
    let read = load?;
    let window = ReadWindow::from(read);
    let text = match &read.result {
        Ok(text) => text,
        Err(error) => return io_failed_record(plan, descriptor, error, window),
    };
    let average = parse_loadavg(text);
    match field {
        LoadField::One => f64_field_record(plan, descriptor, average.one, window),
        LoadField::Five => f64_field_record(plan, descriptor, average.five, window),
        LoadField::Fifteen => f64_field_record(plan, descriptor, average.fifteen, window),
        LoadField::Running => u64_field_record(plan, descriptor, average.running, window),
        LoadField::Total => u64_field_record(plan, descriptor, average.total, window),
    }
}

/// The current frequency of `cpu` from `current`. A CPU idle beyond its
/// counters' last sample has no averaged frequency (`EAGAIN`): no record
/// that cycle, so its series goes stale rather than failing the batch.
fn current_frequency_record(
    context: &CpuRead<'_>,
    descriptor: &SourceDescriptor,
    (cpu, current): (u32, CurrentFrequency),
    gate: &mut ReprobeGate,
) -> Option<Observation> {
    let key = descriptor.source_id.as_str();
    let now = context.clock.now_boot_ns();
    if let Some(failure) = gate.deferred(key, now) {
        return deferred_record(context.plan, descriptor, &failure, now);
    }
    let path = cpufreq_path(cpu, current.file());
    let read = timed(context.clock, || context.files.read_to_string(&path));
    let window = ReadWindow::from(&read);
    match &read.result {
        Ok(text) => {
            gate.record_success(key);
            let note = match current {
                CurrentFrequency::Average => AVERAGE_FREQUENCY_NOTE,
                CurrentFrequency::Scaling => CURRENT_FREQUENCY_NOTE,
            };
            let notes = vec![note.to_owned()];
            u64_text_record(
                context.plan,
                descriptor,
                text,
                Quality::Usable,
                window,
                notes,
            )
        }
        Err(error)
            if current == CurrentFrequency::Average
                && error.kind() == io::ErrorKind::WouldBlock =>
        {
            gate.record_success(key);
            None
        }
        Err(error) => {
            let (status, reason) = io_failure(error);
            gate.record_failure(key, read.finished_boot_ns, status, &reason);
            failed_record(context.plan, descriptor, status, &reason, window)
        }
    }
}

/// The cached maximum, served with this read's time as NVML serves its
/// metadata cache: the collector holds a static value fresh for three
/// discovery periods, while the file is read again only for a new CPU
/// generation (RUNTIME.md §3). The discovery read time made it stale after
/// half a minute.
fn static_record(
    context: &CpuRead<'_>,
    descriptor: &SourceDescriptor,
    value: u64,
) -> Option<Observation> {
    let served_boot_ns = context.clock.now_boot_ns();
    let window = ReadWindow {
        started_boot_ns: served_boot_ns,
        finished_boot_ns: served_boot_ns,
    };
    ok_record(
        context.plan,
        descriptor,
        Scalar::from_u64(value),
        Quality::Verified,
        window,
        Vec::new(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_time_identities_are_valid() {
        assert_eq!(aggregate_entity_id().as_str(), "cpu");
        assert_eq!(
            cpufreq_path(3, "scaling_cur_freq"),
            "/sys/devices/system/cpu/cpu3/cpufreq/scaling_cur_freq"
        );
    }
}
