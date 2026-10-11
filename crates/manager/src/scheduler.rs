//! SamplingScheduler: multi-rate, absolute-deadline scheduling of adapter
//! reads and discovery (RUNTIME.md §3).
//!
//! * Every read group follows its period class (`fast` → `sampling.interval_ms`,
//!   `capacity` → `capacity_interval_ms`, `slow_health` →
//!   `slow_health_interval_ms`). `effective = max(configured, manifest
//!   minimum) × overload multiplier` where the multiplier only applies to
//!   throttleable fast groups and is capped at 60 s.
//! * Slots are absolute on the boot clock: `phase + k × period` with a stable
//!   per-adapter phase (`clock::stable_phase` of the adapter id). Adapters are
//!   spread over the period while the groups of one adapter share its slot, so
//!   a collector fed by several groups of one host runs once per cycle; a
//!   period that is a multiple of another lands on the shorter one's slots.
//!   Missed slots are never replayed (`clock::next_due_after`), so a clock
//!   jump schedules exactly the next future slot.
//! * One request per host. Due groups wait for the host and are dispatched by
//!   earliest due time with round-robin ties; a slot that arrives while the same
//!   group is still in flight or still waiting is recorded as `skipped_due` and
//!   never becomes a second request.
//! * Paged groups continue with the next page of the same cycle immediately.
//! * Discovery runs at start, every `discovery_interval_ms`, paged, and early
//!   (rate limited) after host recovery or when unknown sources appear. An
//!   adapter whose discovery reports `unavailable`/`permission_denied` is not
//!   read and is re-probed every `unsupported_probe_ms`.
//! * Replay rule: a replay adapter (`naming::is_replay_adapter`) schedules each
//!   group `<live adapter>.<group>` its catalog reports a source for, using the
//!   period class and paging of the live group it mirrors. An adapter unknown
//!   to `naming::READ_GROUPS` schedules the groups of its catalog as paged fast
//!   groups with a 1000 ms cadence floor (unknown hardware cadence).
//! * Replies are matched by request id; replies of an older epoch or host
//!   generation are reported stale and never reach a collector.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::clock::{
    NANOS_PER_MILLI, NANOS_PER_SECOND, ms_to_ns, next_due_after, stable_phase,
};
use dgxtop_core::config::Config;
use dgxtop_core::identity::{AdapterId, ReadGroup};
use dgxtop_core::naming::{self, PeriodClass};
use dgxtop_core::observation::{AdapterManifest, DiscoverRequest, ReadPlan, paged_group};
use dgxtop_core::ports::{AdapterJob, Dispatch, DispatchError};
use dgxtop_core::quality::Availability;
use dgxtop_core::view::IntervalView;
use uuid::Uuid;

/// Grace after a request deadline before the scheduler stops waiting for a
/// reply the runtime never delivered (the runtime normally reports `TimedOut`).
const REPLY_GRACE_NS: u64 = 250 * NANOS_PER_MILLI;
/// Retry delay after the adapter port answered `Busy`/`Capacity`.
const DISPATCH_RETRY_NS: u64 = 20 * NANOS_PER_MILLI;
/// Minimum spacing of early (recovery / unknown source) discovery requests.
const EARLY_DISCOVERY_GAP_NS: u64 = 2 * NANOS_PER_SECOND;
/// Upper bound of pages one read cycle or discovery round may span.
pub(crate) const MAX_PAGES_PER_CYCLE: u32 = 64;
/// Longest effective period after overload multiplication.
const MAX_EFFECTIVE_MS: u32 = 60_000;
/// Cadence floor for adapters whose hardware cadence is unknown.
const UNKNOWN_CADENCE_FLOOR_MS: u32 = 1_000;
/// Discovery may take longer than one read.
const MIN_DISCOVERY_DEADLINE_MS: u32 = 1_000;
/// `IntervalView::read_group` of the discovery entry.
const DISCOVERY_GROUP: &str = "discovery";

/// The scheduling-relevant part of the effective configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SchedulerSettings {
    interval_ms: u32,
    capacity_interval_ms: u32,
    slow_health_interval_ms: u32,
    discovery_interval_ms: u32,
    unsupported_probe_ms: u32,
    read_deadline_ms: u32,
    gpu_enabled: bool,
}

impl SchedulerSettings {
    fn from_config(config: &Config) -> Self {
        let sampling = &config.sampling;
        Self {
            interval_ms: sampling.interval_ms,
            capacity_interval_ms: sampling.capacity_interval_ms,
            slow_health_interval_ms: sampling.slow_health_interval_ms,
            discovery_interval_ms: sampling.discovery_interval_ms,
            unsupported_probe_ms: sampling.unsupported_probe_ms,
            read_deadline_ms: sampling.read_deadline_ms,
            gpu_enabled: sampling.gpu_enabled,
        }
    }

    fn configured_ms(&self, class: PeriodClass) -> u32 {
        match class {
            PeriodClass::Fast => self.interval_ms,
            PeriodClass::Capacity => self.capacity_interval_ms,
            PeriodClass::SlowHealth => self.slow_health_interval_ms,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdapterKind {
    Live,
    Replay,
    Custom,
}

impl AdapterKind {
    fn of(adapter: &str) -> Self {
        if naming::is_replay_adapter(adapter) {
            Self::Replay
        } else if naming::read_groups_of(adapter).next().is_some() {
            Self::Live
        } else {
            Self::Custom
        }
    }
}

/// Inputs of a period computation.
#[derive(Debug, Clone, Copy)]
struct PeriodInputs<'a> {
    settings: &'a SchedulerSettings,
    minimum_ms: u32,
    kind: AdapterKind,
    multiplier: u32,
}

/// `(configured, effective, reason)` of one read group.
fn compute_period(class: PeriodClass, inputs: PeriodInputs<'_>) -> (u32, u32, String) {
    let configured = inputs.settings.configured_ms(class);
    let mut reasons = Vec::new();
    let mut effective = configured;
    if inputs.kind == AdapterKind::Custom && effective < UNKNOWN_CADENCE_FLOOR_MS {
        effective = UNKNOWN_CADENCE_FLOOR_MS;
        reasons.push(format!(
            "unknown hardware cadence floor {UNKNOWN_CADENCE_FLOOR_MS} ms"
        ));
    }
    if inputs.minimum_ms > effective {
        effective = inputs.minimum_ms;
        reasons.push(format!("adapter minimum {} ms", inputs.minimum_ms));
    }
    if class == PeriodClass::Fast && inputs.multiplier > 1 {
        let scaled = effective.saturating_mul(inputs.multiplier);
        let capped = scaled.min(MAX_EFFECTIVE_MS).max(effective);
        reasons.push(if capped < scaled {
            format!(
                "overload x{}, capped at {MAX_EFFECTIVE_MS} ms",
                inputs.multiplier
            )
        } else {
            format!("overload x{}", inputs.multiplier)
        });
        effective = capped;
    }
    let reason = if reasons.is_empty() {
        "configured".to_owned()
    } else {
        reasons.join("; ")
    };
    (configured, effective, reason)
}

#[derive(Debug, Clone)]
struct GroupSchedule {
    name: String,
    class: PeriodClass,
    paged: bool,
    configured_ms: u32,
    effective_ms: u32,
    reason: String,
    phase_ns: u64,
    next_due_ns: u64,
    pending_since_ns: Option<u64>,
    skipped_due: u64,
}

impl GroupSchedule {
    fn new(
        adapter: &str,
        spec: (String, PeriodClass, bool),
        inputs: PeriodInputs<'_>,
        now_ns: u64,
    ) -> Self {
        let (name, class, paged) = spec;
        let mut group = Self {
            name,
            class,
            paged,
            configured_ms: 0,
            effective_ms: 0,
            reason: String::new(),
            phase_ns: 0,
            next_due_ns: 0,
            pending_since_ns: None,
            skipped_due: 0,
        };
        group.replan(adapter, inputs, now_ns);
        group
    }

    fn replan(&mut self, adapter: &str, inputs: PeriodInputs<'_>, now_ns: u64) {
        let (configured, effective, reason) = compute_period(self.class, inputs);
        self.configured_ms = configured;
        self.effective_ms = effective;
        self.reason = reason;
        let period_ns = ms_to_ns(u64::from(effective));
        self.phase_ns = stable_phase(adapter, period_ns);
        self.next_due_ns = next_due_after(self.phase_ns, period_ns, now_ns);
    }

    fn period_ns(&self) -> u64 {
        ms_to_ns(u64::from(self.effective_ms))
    }
}

#[derive(Debug, Clone)]
struct DiscoverySchedule {
    configured_ms: u32,
    effective_ms: u32,
    phase_ns: u64,
    next_due_ns: u64,
    pending_since_ns: Option<u64>,
    last_request_ns: Option<u64>,
}

impl DiscoverySchedule {
    fn new(adapter: &str, settings: &SchedulerSettings, unsupported: bool, now_ns: u64) -> Self {
        let mut discovery = Self {
            configured_ms: 0,
            effective_ms: 0,
            phase_ns: 0,
            next_due_ns: 0,
            // Discovery runs at start: the first round is due immediately.
            pending_since_ns: Some(now_ns),
            last_request_ns: None,
        };
        discovery.replan(adapter, settings, unsupported, now_ns);
        discovery
    }

    fn replan(
        &mut self,
        adapter: &str,
        settings: &SchedulerSettings,
        unsupported: bool,
        now_ns: u64,
    ) {
        self.configured_ms = settings.discovery_interval_ms;
        self.effective_ms = if unsupported {
            settings.unsupported_probe_ms
        } else {
            settings.discovery_interval_ms
        };
        let period_ns = ms_to_ns(u64::from(self.effective_ms));
        self.phase_ns = stable_phase(&naming::static_period_key(adapter), period_ns);
        self.next_due_ns = next_due_after(self.phase_ns, period_ns, now_ns);
    }
}

/// What an outstanding request asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestKind {
    Read {
        group: String,
        cycle_id: u64,
        page: u32,
    },
    Discover {
        page: u32,
    },
}

#[derive(Debug, Clone)]
struct Inflight {
    request_id: Uuid,
    kind: RequestKind,
    epoch: u64,
    host_generation: u32,
    deadline_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Work {
    Read(usize),
    Discover,
}

/// Per-request constants shared by every dispatch of one tick.
#[derive(Debug, Clone, Copy)]
struct RequestContext<'a> {
    epoch: u64,
    config_revision: u32,
    catalog_revision: u32,
    settings: &'a SchedulerSettings,
}

#[derive(Debug)]
struct HostSchedule {
    adapter_id: AdapterId,
    kind: AdapterKind,
    minimum_ms: u32,
    enabled: bool,
    groups: Vec<GroupSchedule>,
    discovery: DiscoverySchedule,
    inflight: Option<Inflight>,
    retry_at_ns: Option<u64>,
    next_cycle_id: u64,
    rr_cursor: usize,
    generation: Option<u32>,
    dispatchable: bool,
    unsupported: bool,
    discovered: bool,
    catalog_groups: BTreeSet<String>,
    catalog_generation: u32,
}

impl HostSchedule {
    /// A read request is in flight: more groups of its cycle may arrive.
    fn reads(&self) -> bool {
        self.inflight
            .as_ref()
            .is_some_and(|inflight| matches!(inflight.kind, RequestKind::Read { .. }))
    }

    fn new(manifest: &AdapterManifest, settings: &SchedulerSettings, now_ns: u64) -> Self {
        let adapter = manifest.adapter_id.as_str();
        let mut host = Self {
            adapter_id: manifest.adapter_id.clone(),
            kind: AdapterKind::of(adapter),
            minimum_ms: manifest.minimum_sample_ms,
            enabled: true,
            groups: Vec::new(),
            discovery: DiscoverySchedule::new(adapter, settings, false, now_ns),
            inflight: None,
            retry_at_ns: None,
            next_cycle_id: 1,
            rr_cursor: 0,
            generation: None,
            dispatchable: false,
            unsupported: false,
            discovered: false,
            catalog_groups: BTreeSet::new(),
            catalog_generation: 0,
        };
        host.rebuild_groups(settings, 1, now_ns);
        host
    }

    fn inputs<'a>(&self, settings: &'a SchedulerSettings, multiplier: u32) -> PeriodInputs<'a> {
        PeriodInputs {
            settings,
            minimum_ms: self.minimum_ms,
            kind: self.kind,
            multiplier,
        }
    }

    /// The `(name, class, paged)` groups this host should schedule now.
    fn wanted_groups(&self, settings: &SchedulerSettings) -> Vec<(String, PeriodClass, bool)> {
        let adapter = self.adapter_id.as_str();
        match self.kind {
            AdapterKind::Live => naming::read_groups_of(adapter)
                .map(|spec| (spec.group.to_owned(), spec.period, spec.paged))
                .collect(),
            AdapterKind::Replay => self
                .catalog_groups
                .iter()
                .filter_map(|group| mirrored_spec(group, settings.gpu_enabled))
                .collect(),
            AdapterKind::Custom => self
                .catalog_groups
                .iter()
                .map(|group| (group.clone(), PeriodClass::Fast, true))
                .collect(),
        }
    }

    fn rebuild_groups(&mut self, settings: &SchedulerSettings, multiplier: u32, now_ns: u64) {
        self.enabled = !(self.kind == AdapterKind::Live
            && self.adapter_id.as_str() == naming::adapters::NVML
            && !settings.gpu_enabled);
        let inputs = self.inputs(settings, multiplier);
        let adapter = self.adapter_id.to_string();
        let mut previous: BTreeMap<String, GroupSchedule> = self
            .groups
            .drain(..)
            .map(|group| (group.name.clone(), group))
            .collect();
        let wanted = self.wanted_groups(settings);
        self.groups = wanted
            .into_iter()
            .map(|spec| match previous.remove(&spec.0) {
                Some(existing) => existing,
                None => GroupSchedule::new(&adapter, spec, inputs, now_ns),
            })
            .collect();
        self.rr_cursor %= self.groups.len() + 1;
    }

    fn replan(
        &mut self,
        settings: &SchedulerSettings,
        multiplier: u32,
        now_ns: u64,
        new_epoch: bool,
    ) {
        self.rebuild_groups(settings, multiplier, now_ns);
        let inputs = self.inputs(settings, multiplier);
        let adapter = self.adapter_id.to_string();
        for group in &mut self.groups {
            group.replan(&adapter, inputs, now_ns);
            if new_epoch {
                group.pending_since_ns = None;
            }
        }
        let pending = self.discovery.pending_since_ns;
        self.discovery
            .replan(&adapter, settings, self.unsupported, now_ns);
        self.discovery.pending_since_ns = pending;
    }

    fn reads_active(&self) -> bool {
        self.generation.is_some() && self.discovered && !self.unsupported
    }

    fn expire(&mut self, now_ns: u64, report: &mut TickReport) {
        let expired = self
            .inflight
            .as_ref()
            .is_some_and(|inflight| now_ns >= inflight.deadline_ns.saturating_add(REPLY_GRACE_NS));
        if expired && let Some(inflight) = self.inflight.take() {
            report.expired.push(ExpiredRequest {
                adapter_id: self.adapter_id.clone(),
                request_id: inflight.request_id,
                kind: inflight.kind,
                deadline_ns: inflight.deadline_ns,
            });
        }
    }

    fn mark_due(&mut self, now_ns: u64, report: &mut TickReport) {
        let reads_active = self.reads_active();
        let busy_group = match &self.inflight {
            Some(Inflight {
                kind: RequestKind::Read { group, .. },
                ..
            }) => Some(group.clone()),
            _ => None,
        };
        for group in &mut self.groups {
            if now_ns < group.next_due_ns {
                continue;
            }
            let slot_ns = group.next_due_ns;
            group.next_due_ns = next_due_after(group.phase_ns, group.period_ns(), now_ns);
            if !reads_active {
                continue;
            }
            if !self.dispatchable {
                report.missed.push(MissedSlot {
                    adapter_id: self.adapter_id.clone(),
                    group: group.name.clone(),
                    slot_ns,
                    next_ns: group.next_due_ns,
                });
            } else if busy_group.as_deref() == Some(group.name.as_str())
                || group.pending_since_ns.is_some()
            {
                group.skipped_due = group.skipped_due.saturating_add(1);
                report.skipped = report.skipped.saturating_add(1);
            } else {
                group.pending_since_ns = Some(slot_ns);
            }
        }
        if now_ns >= self.discovery.next_due_ns {
            let slot_ns = self.discovery.next_due_ns;
            let period_ns = ms_to_ns(u64::from(self.discovery.effective_ms));
            self.discovery.next_due_ns = next_due_after(self.discovery.phase_ns, period_ns, now_ns);
            let discovering = matches!(
                self.inflight,
                Some(Inflight {
                    kind: RequestKind::Discover { .. },
                    ..
                })
            );
            if !discovering && self.discovery.pending_since_ns.is_none() {
                self.discovery.pending_since_ns = Some(slot_ns);
            }
        }
    }

    /// Earliest pending work, round-robin among equal due times.
    fn pick(&self, defer_metadata: bool) -> Option<Work> {
        let slots = self.groups.len() + 1;
        let cursor = self.rr_cursor % slots;
        let rank = |index: usize| (index + slots - cursor) % slots;
        let mut best: Option<((u64, usize), Work)> = None;
        let mut consider = |since: u64, index: usize, work: Work| {
            let key = (since, rank(index));
            if best.as_ref().is_none_or(|(current, _)| key < *current) {
                best = Some((key, work));
            }
        };
        if self.reads_active() {
            for (index, group) in self.groups.iter().enumerate() {
                if let Some(since) = group.pending_since_ns {
                    consider(since, index, Work::Read(index));
                }
            }
        }
        // Metadata is deferred under overload once a first catalog exists.
        let discovery_allowed = !(defer_metadata && self.discovered);
        if discovery_allowed && let Some(since) = self.discovery.pending_since_ns {
            consider(since, self.groups.len(), Work::Discover);
        }
        best.map(|(_, work)| work)
    }

    fn dispatch(
        &mut self,
        now_ns: u64,
        request: RequestContext<'_>,
        defer_metadata: bool,
        port: &dyn Dispatch<AdapterJob>,
        report: &mut TickReport,
    ) {
        // A passed retry instant is cleared first: left behind, it would be
        // an overdue deadline that keeps waking the control loop.
        if let Some(retry_at) = self.retry_at_ns {
            if now_ns < retry_at {
                return;
            }
            self.retry_at_ns = None;
        }
        let Some(generation) = self.generation else {
            return;
        };
        if self.inflight.is_some() || !self.dispatchable || !self.enabled {
            return;
        }
        let Some(work) = self.pick(defer_metadata) else {
            return;
        };
        let result = match work {
            Work::Read(index) => self.dispatch_read(index, generation, now_ns, request, port),
            Work::Discover => self.dispatch_discovery(0, generation, now_ns, request, port),
        };
        match result {
            Ok(()) => {
                report.dispatched = report.dispatched.saturating_add(1);
                self.retry_at_ns = None;
                let index = match work {
                    Work::Read(index) => index,
                    Work::Discover => self.groups.len(),
                };
                self.rr_cursor = (index + 1) % (self.groups.len() + 1);
            }
            Err(error) => {
                report.deferred.record(&error);
                if error == DispatchError::Closed {
                    self.dispatchable = false;
                } else {
                    self.retry_at_ns = Some(now_ns.saturating_add(DISPATCH_RETRY_NS));
                }
            }
        }
    }

    fn dispatch_read(
        &mut self,
        index: usize,
        generation: u32,
        now_ns: u64,
        request: RequestContext<'_>,
        port: &dyn Dispatch<AdapterJob>,
    ) -> Result<(), DispatchError> {
        let Some(group) = self.groups.get(index) else {
            return Ok(());
        };
        let name = group.name.clone();
        let cycle_id = self.next_cycle_id;
        let Some(plan) = self.read_plan(&name, cycle_id, 0, generation, now_ns, request) else {
            // A group name that cannot be a ReadGroup is never schedulable.
            if let Some(group) = self.groups.get_mut(index) {
                group.pending_since_ns = None;
            }
            return Err(DispatchError::Closed);
        };
        let inflight = Inflight {
            request_id: plan.request_id,
            kind: RequestKind::Read {
                group: name,
                cycle_id,
                page: 0,
            },
            epoch: request.epoch,
            host_generation: generation,
            deadline_ns: plan.deadline_boot_ns,
        };
        port.try_dispatch(AdapterJob::Read(plan))?;
        self.next_cycle_id = self.next_cycle_id.saturating_add(1);
        if let Some(group) = self.groups.get_mut(index) {
            group.pending_since_ns = None;
        }
        self.inflight = Some(inflight);
        Ok(())
    }

    fn dispatch_discovery(
        &mut self,
        page: u32,
        generation: u32,
        now_ns: u64,
        request: RequestContext<'_>,
        port: &dyn Dispatch<AdapterJob>,
    ) -> Result<(), DispatchError> {
        let deadline_ms = request
            .settings
            .read_deadline_ms
            .max(MIN_DISCOVERY_DEADLINE_MS);
        let discover = DiscoverRequest {
            request_id: Uuid::new_v4(),
            adapter_id: self.adapter_id.clone(),
            host_generation: generation,
            catalog_revision: request.catalog_revision.max(1),
            config_revision: request.config_revision.max(1),
            deadline_boot_ns: now_ns.saturating_add(ms_to_ns(u64::from(deadline_ms))),
            previous_generation: self.catalog_generation,
            page,
        };
        let inflight = Inflight {
            request_id: discover.request_id,
            kind: RequestKind::Discover { page },
            epoch: request.epoch,
            host_generation: generation,
            deadline_ns: discover.deadline_boot_ns,
        };
        port.try_dispatch(AdapterJob::Discover(discover))?;
        self.discovery.pending_since_ns = None;
        self.discovery.last_request_ns = Some(now_ns);
        self.inflight = Some(inflight);
        Ok(())
    }

    fn read_plan(
        &self,
        group: &str,
        cycle_id: u64,
        page: u32,
        generation: u32,
        now_ns: u64,
        request: RequestContext<'_>,
    ) -> Option<ReadPlan> {
        let read_group = ReadGroup::new(paged_group(group, page)).ok()?;
        let deadline_ns =
            now_ns.saturating_add(ms_to_ns(u64::from(request.settings.read_deadline_ms)));
        Some(ReadPlan {
            request_id: Uuid::new_v4(),
            adapter_id: self.adapter_id.clone(),
            host_generation: generation,
            catalog_revision: request.catalog_revision.max(1),
            config_revision: request.config_revision.max(1),
            cycle_id,
            read_group,
            // Empty means "the whole group": adapters page through
            // PAGE_CONTINUES, so no request ever names more than 256 sources.
            source_slots: Vec::new(),
            deadline_boot_ns: deadline_ns,
            epoch: request.epoch,
            page,
        })
    }

    fn request_early_discovery(&mut self, now_ns: u64) -> bool {
        let discovering = matches!(
            self.inflight,
            Some(Inflight {
                kind: RequestKind::Discover { .. },
                ..
            })
        );
        let rate_limited = self
            .discovery
            .last_request_ns
            .is_some_and(|last| now_ns < last.saturating_add(EARLY_DISCOVERY_GAP_NS));
        if !self.enabled || discovering || self.discovery.pending_since_ns.is_some() || rate_limited
        {
            return false;
        }
        self.discovery.pending_since_ns = Some(now_ns);
        true
    }

    fn deadlines(&self) -> impl Iterator<Item = u64> + '_ {
        let groups = self.groups.iter().map(|group| group.next_due_ns);
        let inflight = self
            .inflight
            .as_ref()
            .map(|inflight| inflight.deadline_ns.saturating_add(REPLY_GRACE_NS));
        groups
            .chain(std::iter::once(self.discovery.next_due_ns))
            .chain(inflight)
            .chain(self.retry_at_ns)
    }
}

/// The live group a replay group mirrors: `<live adapter>.<group>`.
fn mirrored_spec(group: &str, gpu_enabled: bool) -> Option<(String, PeriodClass, bool)> {
    let (live_adapter, live_group) = group.split_once('.')?;
    if live_adapter == naming::adapters::NVML && !gpu_enabled {
        return None;
    }
    naming::READ_GROUPS
        .iter()
        .find(|spec| spec.adapter == live_adapter && spec.group == live_group)
        .map(|spec| (group.to_owned(), spec.period, spec.paged))
}

/// Outcome of matching an adapter reply against the outstanding request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReplyMatch {
    Read(ReadReply),
    Discover { page: u32 },
    Stale(StaleReason),
}

/// The read cycle a reply belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadReply {
    pub(crate) group: String,
    pub(crate) cycle_id: u64,
    pub(crate) page: u32,
    pub(crate) epoch: u64,
}

/// Why a reply is not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StaleReason {
    UnknownAdapter,
    NoRequest,
    OtherRequest,
    OldGeneration,
    OldEpoch,
}

impl StaleReason {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::UnknownAdapter => "unknown_adapter",
            Self::NoRequest => "no_request",
            Self::OtherRequest => "other_request",
            Self::OldGeneration => "old_generation",
            Self::OldEpoch => "old_epoch",
        }
    }
}

/// Why the next page of a cycle/round could not be requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContinueError {
    /// The group is not paged, yet the adapter announced more pages.
    NotPaged,
    PageLimit,
    Stopped,
    HostUnavailable,
    InvalidGroup,
    Dispatch(DispatchError),
}

impl ContinueError {
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::NotPaged => "unexpected_page",
            Self::PageLimit => "page_limit",
            Self::Stopped => "stopped",
            Self::HostUnavailable => "host_unavailable",
            Self::InvalidGroup => "invalid_group",
            Self::Dispatch(_) => "page_dispatch_failed",
        }
    }
}

/// A request whose reply never arrived before deadline + grace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExpiredRequest {
    pub(crate) adapter_id: AdapterId,
    pub(crate) request_id: Uuid,
    pub(crate) kind: RequestKind,
    pub(crate) deadline_ns: u64,
}

/// A slot that could not be served because the host was unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MissedSlot {
    pub(crate) adapter_id: AdapterId,
    pub(crate) group: String,
    pub(crate) slot_ns: u64,
    pub(crate) next_ns: u64,
}

/// A request lost because its host restarted (new generation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LostRequest {
    pub(crate) request_id: Uuid,
    pub(crate) kind: RequestKind,
}

/// What one scheduler tick did.
#[derive(Debug, Default)]
pub(crate) struct TickReport {
    pub(crate) expired: Vec<ExpiredRequest>,
    pub(crate) missed: Vec<MissedSlot>,
    pub(crate) skipped: u64,
    pub(crate) dispatched: u32,
    pub(crate) deferred: DeferredDispatches,
}

/// Dispatches a worker refused in one tick, by reason. Refused work stays
/// pending and is retried, so a refusal is a deferral, never a dropped item;
/// a slot the deferral makes a group miss is recorded as a gap (D-37).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeferredDispatches {
    pub(crate) worker_busy: u64,
    pub(crate) capacity: u64,
    pub(crate) closed: u64,
}

impl DeferredDispatches {
    fn record(&mut self, error: &DispatchError) {
        let count = match error {
            DispatchError::Busy => &mut self.worker_busy,
            DispatchError::Capacity(_) => &mut self.capacity,
            DispatchError::Closed => &mut self.closed,
        };
        *count = count.saturating_add(1);
    }

    /// The non-zero counts with their bounded diagnostics labels.
    pub(crate) fn by_reason(&self) -> impl Iterator<Item = (&'static str, u64)> {
        [
            ("worker_busy", self.worker_busy),
            ("capacity", self.capacity),
            ("closed", self.closed),
        ]
        .into_iter()
        .filter(|(_, count)| *count > 0)
    }
}

/// Inputs of a tick that come from other services.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DispatchContext {
    pub(crate) catalog_revision: u32,
    pub(crate) defer_metadata: bool,
}

pub(crate) struct SamplingScheduler {
    port: Box<dyn Dispatch<AdapterJob>>,
    settings: SchedulerSettings,
    epoch: u64,
    config_revision: u32,
    multiplier: u32,
    hosts: Vec<HostSchedule>,
    stopped: bool,
}

impl SamplingScheduler {
    /// Adapters with a read request in flight: more groups of their current
    /// cycle may still arrive.
    pub(crate) fn busy_adapters(&self) -> std::collections::BTreeSet<AdapterId> {
        self.hosts
            .iter()
            .filter(|host| host.reads())
            .map(|host| host.adapter_id.clone())
            .collect()
    }

    /// Whether some adapter has a read in flight.
    pub(crate) fn has_read_in_flight(&self) -> bool {
        self.hosts.iter().any(HostSchedule::reads)
    }

    pub(crate) fn new(
        port: Box<dyn Dispatch<AdapterJob>>,
        manifests: &[AdapterManifest],
        config: &Config,
        now_ns: u64,
    ) -> Self {
        let settings = SchedulerSettings::from_config(config);
        let hosts = manifests
            .iter()
            .map(|manifest| HostSchedule::new(manifest, &settings, now_ns))
            .collect();
        Self {
            port,
            settings,
            epoch: 1,
            config_revision: config.revision,
            multiplier: 1,
            hosts,
            stopped: false,
        }
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Records a configuration change that does not affect scheduling.
    pub(crate) fn set_config_revision(&mut self, revision: u32) {
        self.config_revision = revision;
    }

    /// Applies new sampling settings under a new epoch; replies to requests of
    /// the previous epoch become stale. Returns the new epoch.
    pub(crate) fn reconfigure(&mut self, config: &Config, now_ns: u64) -> u64 {
        self.settings = SchedulerSettings::from_config(config);
        self.config_revision = config.revision;
        self.epoch = self.epoch.saturating_add(1);
        for host in &mut self.hosts {
            host.replan(&self.settings, self.multiplier, now_ns, true);
        }
        self.epoch
    }

    /// Applies an overload multiplier to throttleable fast groups.
    pub(crate) fn set_multiplier(&mut self, multiplier: u32, now_ns: u64) -> bool {
        let multiplier = multiplier.max(1);
        if multiplier == self.multiplier {
            return false;
        }
        self.multiplier = multiplier;
        for host in &mut self.hosts {
            host.replan(&self.settings, multiplier, now_ns, false);
        }
        true
    }

    /// Stops all further dispatching (shutdown).
    pub(crate) fn stop(&mut self) {
        self.stopped = true;
    }

    /// Host generation/state report. Returns the request lost to a restart.
    pub(crate) fn on_host_status(
        &mut self,
        adapter_id: &AdapterId,
        generation: u32,
        dispatchable: bool,
        now_ns: u64,
    ) -> Option<LostRequest> {
        let host = self.host_mut(adapter_id)?;
        let previous = host.generation;
        let was_available = previous.is_some() && host.dispatchable;
        host.generation = Some(generation);
        host.dispatchable = dispatchable;
        host.retry_at_ns = None;
        let restarted = previous.is_some_and(|old| old != generation);
        let lost = if restarted {
            host.inflight.take().map(|inflight| LostRequest {
                request_id: inflight.request_id,
                kind: inflight.kind,
            })
        } else {
            None
        };
        // Half-open recovery or a new incarnation: refresh the catalog early.
        if previous.is_some() && dispatchable && (restarted || !was_available) {
            host.request_early_discovery(now_ns);
        }
        lost
    }

    /// Matches a reply; the host becomes free whenever the id matches.
    pub(crate) fn match_reply(
        &mut self,
        adapter_id: &AdapterId,
        request_id: Uuid,
        host_generation: u32,
    ) -> ReplyMatch {
        let epoch = self.epoch;
        let Some(host) = self.host_mut(adapter_id) else {
            return ReplyMatch::Stale(StaleReason::UnknownAdapter);
        };
        let Some(current) = host.inflight.as_ref() else {
            return ReplyMatch::Stale(StaleReason::NoRequest);
        };
        if current.request_id != request_id {
            return ReplyMatch::Stale(StaleReason::OtherRequest);
        }
        let Some(inflight) = host.inflight.take() else {
            return ReplyMatch::Stale(StaleReason::NoRequest);
        };
        host.retry_at_ns = None;
        if inflight.host_generation != host_generation || host.generation != Some(host_generation) {
            return ReplyMatch::Stale(StaleReason::OldGeneration);
        }
        if inflight.epoch != epoch {
            return ReplyMatch::Stale(StaleReason::OldEpoch);
        }
        match inflight.kind {
            RequestKind::Read {
                group,
                cycle_id,
                page,
            } => ReplyMatch::Read(ReadReply {
                group,
                cycle_id,
                page,
                epoch: inflight.epoch,
            }),
            RequestKind::Discover { page } => ReplyMatch::Discover { page },
        }
    }

    /// Requests the next page of a paged read cycle right away.
    pub(crate) fn continue_read(
        &mut self,
        adapter_id: &AdapterId,
        reply: &ReadReply,
        now_ns: u64,
        catalog_revision: u32,
    ) -> Result<(), ContinueError> {
        let next_page = reply.page.saturating_add(1);
        if next_page >= MAX_PAGES_PER_CYCLE {
            return Err(ContinueError::PageLimit);
        }
        if self.stopped {
            return Err(ContinueError::Stopped);
        }
        let request = RequestContext {
            epoch: self.epoch,
            config_revision: self.config_revision,
            catalog_revision,
            settings: &self.settings,
        };
        let host = self
            .hosts
            .iter_mut()
            .find(|host| &host.adapter_id == adapter_id)
            .ok_or(ContinueError::HostUnavailable)?;
        let paged = host
            .groups
            .iter()
            .any(|group| group.name == reply.group && group.paged);
        if !paged {
            return Err(ContinueError::NotPaged);
        }
        let generation = host
            .generation
            .filter(|_| host.dispatchable && host.enabled && host.inflight.is_none())
            .ok_or(ContinueError::HostUnavailable)?;
        let plan = host
            .read_plan(
                &reply.group,
                reply.cycle_id,
                next_page,
                generation,
                now_ns,
                request,
            )
            .ok_or(ContinueError::InvalidGroup)?;
        let inflight = Inflight {
            request_id: plan.request_id,
            kind: RequestKind::Read {
                group: reply.group.clone(),
                cycle_id: reply.cycle_id,
                page: next_page,
            },
            epoch: request.epoch,
            host_generation: generation,
            deadline_ns: plan.deadline_boot_ns,
        };
        self.port
            .try_dispatch(AdapterJob::Read(plan))
            .map_err(ContinueError::Dispatch)?;
        host.inflight = Some(inflight);
        Ok(())
    }

    /// Requests the next page of a discovery round right away.
    pub(crate) fn continue_discovery(
        &mut self,
        adapter_id: &AdapterId,
        page: u32,
        now_ns: u64,
        catalog_revision: u32,
    ) -> Result<(), ContinueError> {
        if page >= MAX_PAGES_PER_CYCLE {
            return Err(ContinueError::PageLimit);
        }
        if self.stopped {
            return Err(ContinueError::Stopped);
        }
        let request = RequestContext {
            epoch: self.epoch,
            config_revision: self.config_revision,
            catalog_revision,
            settings: &self.settings,
        };
        let host = self
            .hosts
            .iter_mut()
            .find(|host| &host.adapter_id == adapter_id)
            .ok_or(ContinueError::HostUnavailable)?;
        let generation = host
            .generation
            .filter(|_| host.dispatchable && host.enabled && host.inflight.is_none())
            .ok_or(ContinueError::HostUnavailable)?;
        host.dispatch_discovery(page, generation, now_ns, request, self.port.as_ref())
            .map_err(ContinueError::Dispatch)
    }

    /// A discovery round finished (complete page or last page).
    pub(crate) fn discovery_finished(
        &mut self,
        adapter_id: &AdapterId,
        availability: Option<Availability>,
        catalog_groups: BTreeSet<String>,
        catalog_generation: u32,
        now_ns: u64,
    ) {
        let settings = self.settings.clone();
        let multiplier = self.multiplier;
        let Some(host) = self.host_mut(adapter_id) else {
            return;
        };
        let unsupported = matches!(
            availability,
            Some(Availability::Unavailable | Availability::PermissionDenied)
        );
        host.discovered = true;
        host.catalog_generation = catalog_generation;
        let groups_changed = host.catalog_groups != catalog_groups;
        host.catalog_groups = catalog_groups;
        if host.unsupported != unsupported {
            host.unsupported = unsupported;
            let pending = host.discovery.pending_since_ns;
            host.discovery
                .replan(adapter_id.as_str(), &settings, unsupported, now_ns);
            host.discovery.pending_since_ns = pending;
        }
        if groups_changed && host.kind != AdapterKind::Live {
            host.rebuild_groups(&settings, multiplier, now_ns);
        }
    }

    /// Early (rate limited) discovery, e.g. for unknown sources.
    pub(crate) fn request_early_discovery(&mut self, adapter_id: &AdapterId, now_ns: u64) -> bool {
        self.host_mut(adapter_id)
            .is_some_and(|host| host.request_early_discovery(now_ns))
    }

    pub(crate) fn tick(&mut self, now_ns: u64, context: DispatchContext) -> TickReport {
        let mut report = TickReport::default();
        if self.stopped {
            return report;
        }
        let request = RequestContext {
            epoch: self.epoch,
            config_revision: self.config_revision,
            catalog_revision: context.catalog_revision,
            settings: &self.settings,
        };
        for host in self.hosts.iter_mut().filter(|host| host.enabled) {
            host.expire(now_ns, &mut report);
            host.mark_due(now_ns, &mut report);
            host.dispatch(
                now_ns,
                request,
                context.defer_metadata,
                self.port.as_ref(),
                &mut report,
            );
        }
        report
    }

    pub(crate) fn next_deadline(&self) -> Option<u64> {
        if self.stopped {
            return None;
        }
        self.hosts
            .iter()
            .filter(|host| host.enabled)
            .flat_map(HostSchedule::deadlines)
            .min()
    }

    /// Configured/effective period of every scheduled group (+ discovery).
    pub(crate) fn intervals(&self) -> Vec<IntervalView> {
        let mut views = Vec::new();
        for host in self.hosts.iter().filter(|host| host.enabled) {
            let adapter = host.adapter_id.to_string();
            if !host.unsupported {
                views.extend(host.groups.iter().map(|group| IntervalView {
                    adapter_id: adapter.clone(),
                    read_group: group.name.clone(),
                    configured_ms: group.configured_ms,
                    effective_ms: group.effective_ms,
                    reason: if host.discovered {
                        group.reason.clone()
                    } else {
                        format!("{}; awaiting discovery", group.reason)
                    },
                }));
            }
            views.push(IntervalView {
                adapter_id: adapter,
                read_group: DISCOVERY_GROUP.to_owned(),
                configured_ms: host.discovery.configured_ms,
                effective_ms: host.discovery.effective_ms,
                reason: if host.unsupported {
                    format!(
                        "adapter unavailable; probing every {} ms",
                        host.discovery.effective_ms
                    )
                } else {
                    "metadata refresh".to_owned()
                },
            });
        }
        views
    }

    /// `PolicySnapshot::effective_periods_ms` content.
    pub(crate) fn effective_periods(&self) -> BTreeMap<String, u32> {
        let mut periods = BTreeMap::new();
        for host in self.hosts.iter().filter(|host| host.enabled) {
            let adapter = host.adapter_id.as_str();
            for group in &host.groups {
                periods.insert(naming::period_key(adapter, &group.name), group.effective_ms);
            }
            periods.insert(
                naming::static_period_key(adapter),
                host.discovery.effective_ms,
            );
        }
        periods
    }

    /// Effective period of the fastest scheduled fast group.
    pub(crate) fn effective_fast_interval_ms(&self) -> u32 {
        self.hosts
            .iter()
            .filter(|host| host.enabled && !host.unsupported)
            .flat_map(|host| host.groups.iter())
            .filter(|group| group.class == PeriodClass::Fast)
            .map(|group| group.effective_ms)
            .min()
            .unwrap_or_else(|| {
                let inputs = PeriodInputs {
                    settings: &self.settings,
                    minimum_ms: 0,
                    kind: AdapterKind::Live,
                    multiplier: self.multiplier,
                };
                compute_period(PeriodClass::Fast, inputs).1
            })
    }

    /// Effective period of one group (used for input expiry).
    pub(crate) fn group_effective_ms(&self, adapter_id: &AdapterId, group: &str) -> Option<u32> {
        self.hosts
            .iter()
            .find(|host| &host.adapter_id == adapter_id)
            .and_then(|host| host.groups.iter().find(|candidate| candidate.name == group))
            .map(|group| group.effective_ms)
    }

    /// Whether a group is paged (tests).
    #[cfg(test)]
    pub(crate) fn is_paged(&self, adapter_id: &AdapterId, group: &str) -> bool {
        self.hosts
            .iter()
            .find(|host| &host.adapter_id == adapter_id)
            .and_then(|host| host.groups.iter().find(|candidate| candidate.name == group))
            .is_some_and(|group| group.paged)
    }

    /// Total `skipped_due` over every group.
    #[cfg(test)]
    pub(crate) fn skipped_due_total(&self) -> u64 {
        self.hosts
            .iter()
            .flat_map(|host| host.groups.iter())
            .map(|group| group.skipped_due)
            .sum()
    }

    fn host_mut(&mut self, adapter_id: &AdapterId) -> Option<&mut HostSchedule> {
        self.hosts
            .iter_mut()
            .find(|host| &host.adapter_id == adapter_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::manifest;
    use dgxtop_core::testing::{CapturingDispatch, adapter_id};

    const START_NS: u64 = 1_000 * NANOS_PER_SECOND;
    const MS: u64 = NANOS_PER_MILLI;

    struct Bench {
        scheduler: SamplingScheduler,
        port: CapturingDispatch<AdapterJob>,
        now: u64,
    }

    fn context() -> DispatchContext {
        DispatchContext {
            catalog_revision: 1,
            defer_metadata: false,
        }
    }

    fn bench(manifests: &[AdapterManifest], config: &Config) -> Bench {
        let port = CapturingDispatch::new();
        let scheduler = SamplingScheduler::new(Box::new(port.clone()), manifests, config, START_NS);
        Bench {
            scheduler,
            port,
            now: START_NS,
        }
    }

    #[derive(Debug, Default)]
    struct Counts {
        reads: BTreeMap<(String, String), u32>,
        discoveries: BTreeMap<String, u32>,
        pages: Vec<u32>,
    }

    impl Bench {
        fn host_up(&mut self, adapter: &str) {
            self.scheduler
                .on_host_status(&adapter_id(adapter), 1, true, self.now);
        }

        /// Runs the scheduler in `step_ms` steps for `duration_ms`, replying to
        /// every request immediately when `reply` is set.
        fn drive(&mut self, duration_ms: u64, step_ms: u64, reply: bool) -> Counts {
            let mut counts = Counts::default();
            let end = self.now + duration_ms * MS;
            while self.now < end {
                self.scheduler.tick(self.now, context());
                for job in self.port.take() {
                    self.observe(job, reply, &mut counts);
                }
                self.now += step_ms * MS;
            }
            counts
        }

        fn observe(&mut self, job: AdapterJob, reply: bool, counts: &mut Counts) {
            match job {
                AdapterJob::Read(plan) => {
                    let (group, page) =
                        dgxtop_core::observation::split_page(plan.read_group.as_str());
                    *counts
                        .reads
                        .entry((plan.adapter_id.to_string(), group.to_owned()))
                        .or_default() += 1;
                    counts.pages.push(page);
                    if reply {
                        let matched =
                            self.scheduler
                                .match_reply(&plan.adapter_id, plan.request_id, 1);
                        assert!(matches!(matched, ReplyMatch::Read(_)));
                    }
                }
                AdapterJob::Discover(request) => {
                    *counts
                        .discoveries
                        .entry(request.adapter_id.to_string())
                        .or_default() += 1;
                    if reply {
                        let matched =
                            self.scheduler
                                .match_reply(&request.adapter_id, request.request_id, 1);
                        assert!(matches!(matched, ReplyMatch::Discover { .. }));
                        self.scheduler.discovery_finished(
                            &request.adapter_id,
                            Some(Availability::Available),
                            BTreeSet::new(),
                            1,
                            self.now,
                        );
                    }
                }
            }
        }
    }

    fn reads(counts: &Counts, adapter: &str, group: &str) -> u32 {
        counts
            .reads
            .get(&(adapter.to_owned(), group.to_owned()))
            .copied()
            .unwrap_or(0)
    }

    #[test]
    fn cadences_follow_period_classes_and_discovery_interval() {
        let config = Config::defaults().unwrap();
        let mut bench = bench(
            &[
                manifest("procfs", 100),
                manifest("filesystem", 1000),
                manifest("nvml", 1000),
            ],
            &config,
        );
        for adapter in ["procfs", "filesystem", "nvml"] {
            bench.host_up(adapter);
        }
        let counts = bench.drive(30_000, 10, true);
        // 1 s fast groups, 5 s capacity/slow health, 10 s discovery (+ start).
        for group in ["cpu", "memory", "disk", "process"] {
            let count = reads(&counts, "procfs", group);
            assert!((29..=30).contains(&count), "procfs/{group}: {count}");
        }
        let fast = reads(&counts, "nvml", "fast");
        assert!((29..=30).contains(&fast), "nvml/fast: {fast}");
        let slow = reads(&counts, "nvml", "slow_health");
        assert!((5..=6).contains(&slow), "nvml/slow_health: {slow}");
        let capacity = reads(&counts, "filesystem", "capacity");
        assert!(
            (5..=6).contains(&capacity),
            "filesystem/capacity: {capacity}"
        );
        for adapter in ["procfs", "filesystem", "nvml"] {
            let discoveries = counts.discoveries[adapter];
            assert!((3..=4).contains(&discoveries), "{adapter}: {discoveries}");
        }
        assert_eq!(bench.scheduler.skipped_due_total(), 0);
    }

    #[test]
    fn unsupported_adapter_is_never_read_and_probed_every_60_s() {
        let config = Config::defaults().unwrap();
        let mut bench = bench(&[manifest("nvml", 1000)], &config);
        bench.host_up("nvml");
        let nvml = adapter_id("nvml");
        let mut discoveries = 0;
        let mut reads = 0;
        let end = bench.now + 180_000 * MS;
        while bench.now < end {
            bench.scheduler.tick(bench.now, context());
            for job in bench.port.take() {
                match job {
                    AdapterJob::Discover(request) => {
                        discoveries += 1;
                        bench.scheduler.match_reply(&nvml, request.request_id, 1);
                        bench.scheduler.discovery_finished(
                            &nvml,
                            Some(Availability::Unavailable),
                            BTreeSet::new(),
                            1,
                            bench.now,
                        );
                    }
                    AdapterJob::Read(_) => reads += 1,
                }
            }
            bench.now += 100 * MS;
        }
        assert_eq!(reads, 0);
        // Start + one probe per 60 s over 180 s.
        assert!((3..=4).contains(&discoveries), "probes: {discoveries}");
        let intervals = bench.scheduler.intervals();
        assert_eq!(intervals.len(), 1);
        assert_eq!(intervals[0].effective_ms, 60_000);
        assert!(intervals[0].reason.contains("probing every 60000 ms"));
    }

    #[test]
    fn clock_jump_schedules_only_the_next_future_slot() {
        let config = Config::defaults().unwrap();
        let mut bench = bench(&[manifest("procfs", 100)], &config);
        bench.host_up("procfs");
        bench.drive(2_000, 10, true);
        // Jump one hour ahead: each group fires at most once, not 3600 times.
        bench.now += 3_600_000 * MS;
        let counts = bench.drive(10, 10, true);
        for group in ["cpu", "memory", "disk", "process"] {
            assert!(reads(&counts, "procfs", group) <= 1, "{group}");
        }
        let total: u32 = counts.reads.values().sum();
        assert!((1..=4).contains(&total), "total {total}");
        let next = bench.scheduler.next_deadline().unwrap();
        assert!(next > bench.now - 10 * MS && next <= bench.now + 1_000 * MS);
    }

    #[test]
    fn busy_host_records_skipped_due_and_never_sends_a_second_request() {
        let config = Config::defaults().unwrap();
        let mut bench = bench(&[manifest("network", 100)], &config);
        bench.host_up("network");
        bench.drive(1, 1, true); // discovery round
        // Never reply to reads: the first read stays in flight.
        let mut config_long_deadline = config.clone();
        config_long_deadline.sampling.read_deadline_ms = 10_000;
        bench
            .scheduler
            .reconfigure(&config_long_deadline, bench.now);
        let counts = bench.drive(5_000, 10, false);
        assert_eq!(reads(&counts, "network", "fast"), 1);
        assert!(bench.scheduler.skipped_due_total() >= 3);
    }

    #[test]
    fn paged_group_requests_the_next_page_of_the_same_cycle_immediately() {
        let config = Config::defaults().unwrap();
        let mut bench = bench(&[manifest("network", 100)], &config);
        bench.host_up("network");
        bench.drive(1, 1, true);
        let network = adapter_id("network");
        // Wait for the first read without replying.
        let mut first = None;
        while first.is_none() {
            bench.scheduler.tick(bench.now, context());
            first = bench.port.take().into_iter().find_map(|job| match job {
                AdapterJob::Read(plan) => Some(plan),
                AdapterJob::Discover(_) => None,
            });
            bench.now += 10 * MS;
        }
        let first = first.unwrap();
        assert_eq!(first.page, 0);
        let ReplyMatch::Read(reply) = bench.scheduler.match_reply(&network, first.request_id, 1)
        else {
            panic!("read reply expected");
        };
        assert!(bench.scheduler.is_paged(&network, &reply.group));
        bench
            .scheduler
            .continue_read(&network, &reply, bench.now, 1)
            .unwrap();
        let jobs = bench.port.take();
        let AdapterJob::Read(second) = &jobs[0] else {
            panic!("page request expected");
        };
        assert_eq!(second.page, 1);
        assert_eq!(second.cycle_id, first.cycle_id);
        assert_eq!(
            second.read_group.as_str(),
            format!("{}:1", reply.group).as_str()
        );
        // The page limit is enforced.
        let limit = ReadReply {
            page: MAX_PAGES_PER_CYCLE - 1,
            ..reply
        };
        bench.scheduler.match_reply(&network, second.request_id, 1);
        assert_eq!(
            bench
                .scheduler
                .continue_read(&network, &limit, bench.now, 1),
            Err(ContinueError::PageLimit)
        );
    }

    #[test]
    fn replies_of_old_epochs_and_generations_are_stale() {
        let config = Config::defaults().unwrap();
        let mut bench = bench(&[manifest("procfs", 100)], &config);
        bench.host_up("procfs");
        bench.drive(1, 1, true);
        let procfs = adapter_id("procfs");
        let mut plan = None;
        while plan.is_none() {
            bench.scheduler.tick(bench.now, context());
            plan = bench.port.take().into_iter().find_map(|job| match job {
                AdapterJob::Read(plan) => Some(plan),
                AdapterJob::Discover(_) => None,
            });
            bench.now += 10 * MS;
        }
        let plan = plan.unwrap();
        let mut faster = config.clone();
        faster.sampling.interval_ms = 500;
        assert_eq!(bench.scheduler.reconfigure(&faster, bench.now), 2);
        assert_eq!(
            bench.scheduler.match_reply(&procfs, plan.request_id, 1),
            ReplyMatch::Stale(StaleReason::OldEpoch)
        );
        // A duplicate of the same reply finds no request any more.
        assert_eq!(
            bench.scheduler.match_reply(&procfs, plan.request_id, 1),
            ReplyMatch::Stale(StaleReason::NoRequest)
        );
        // Generation: a restart drops the in-flight request.
        let mut next = None;
        while next.is_none() {
            bench.scheduler.tick(bench.now, context());
            next = bench.port.take().into_iter().find_map(|job| match job {
                AdapterJob::Read(plan) => Some(plan),
                AdapterJob::Discover(_) => None,
            });
            bench.now += 10 * MS;
        }
        let next = next.unwrap();
        let lost = bench
            .scheduler
            .on_host_status(&procfs, 2, true, bench.now)
            .unwrap();
        assert_eq!(lost.request_id, next.request_id);
        assert_eq!(
            bench.scheduler.match_reply(&procfs, next.request_id, 1),
            ReplyMatch::Stale(StaleReason::NoRequest)
        );
        assert_eq!(
            bench
                .scheduler
                .match_reply(&adapter_id("ghost"), next.request_id, 1),
            ReplyMatch::Stale(StaleReason::UnknownAdapter)
        );
    }

    #[test]
    fn effective_period_respects_minimum_multiplier_and_cap() {
        let mut config = Config::defaults().unwrap();
        config.sampling.interval_ms = 200;
        let settings = SchedulerSettings::from_config(&config);
        let inputs = PeriodInputs {
            settings: &settings,
            minimum_ms: 1000,
            kind: AdapterKind::Live,
            multiplier: 1,
        };
        let (configured, effective, reason) = compute_period(PeriodClass::Fast, inputs);
        assert_eq!((configured, effective), (200, 1000));
        assert!(reason.contains("adapter minimum 1000 ms"));
        let throttled = PeriodInputs {
            multiplier: 4,
            ..inputs
        };
        assert_eq!(compute_period(PeriodClass::Fast, throttled).1, 4000);
        // Slow health is critical: never multiplied.
        assert_eq!(compute_period(PeriodClass::SlowHealth, throttled).1, 5000);
        config.sampling.interval_ms = 30_000;
        let slow = SchedulerSettings::from_config(&config);
        let capped = PeriodInputs {
            settings: &slow,
            minimum_ms: 100,
            kind: AdapterKind::Live,
            multiplier: 4,
        };
        let (_, effective, reason) = compute_period(PeriodClass::Fast, capped);
        assert_eq!(effective, 60_000);
        assert!(reason.contains("capped"));
        let custom = PeriodInputs {
            settings: &settings,
            minimum_ms: 100,
            kind: AdapterKind::Custom,
            multiplier: 1,
        };
        assert_eq!(compute_period(PeriodClass::Fast, custom).1, 1000);
    }

    #[test]
    fn gpu_disabled_skips_nvml_and_replay_mirrors_live_groups() {
        let mut config = Config::defaults().unwrap();
        config.sampling.gpu_enabled = false;
        let mut bench = bench(&[manifest("nvml", 1000), manifest("replay", 100)], &config);
        bench.host_up("nvml");
        bench.host_up("replay");
        let replay = adapter_id("replay");
        let counts = bench.drive(1, 1, false);
        assert_eq!(counts.discoveries.get("nvml"), None, "nvml host disabled");
        assert_eq!(counts.discoveries.get("replay"), Some(&1));
        // Replay discovery reports mirrored groups (one unknown spelling).
        let request = bench
            .scheduler
            .hosts
            .iter()
            .find(|host| host.adapter_id == replay)
            .and_then(|host| host.inflight.as_ref())
            .map(|inflight| inflight.request_id)
            .unwrap();
        bench.scheduler.match_reply(&replay, request, 1);
        bench.scheduler.discovery_finished(
            &replay,
            Some(Availability::Available),
            BTreeSet::from([
                "hwmon.fast".to_owned(),
                "nvml.fast".to_owned(),
                "filesystem.capacity".to_owned(),
                "bogus".to_owned(),
            ]),
            1,
            bench.now,
        );
        let groups: Vec<_> = bench
            .scheduler
            .intervals()
            .into_iter()
            .filter(|view| view.adapter_id == "replay" && view.read_group != DISCOVERY_GROUP)
            .map(|view| (view.read_group, view.effective_ms))
            .collect();
        assert_eq!(
            groups,
            vec![
                ("filesystem.capacity".to_owned(), 5000),
                ("hwmon.fast".to_owned(), 1000),
            ]
        );
        let counts = bench.drive(3_000, 10, true);
        assert!(reads(&counts, "replay", "hwmon.fast") >= 2);
        assert!(counts.reads.keys().all(|(adapter, _)| adapter == "replay"));
    }

    #[test]
    fn round_robin_keeps_every_group_served_on_one_host() {
        let config = Config::defaults().unwrap();
        let mut bench = bench(&[manifest("procfs", 100)], &config);
        bench.host_up("procfs");
        bench.drive(1, 1, true);
        // Replies arrive slowly (host busy 300 ms per request): all four fast
        // groups still get served each second instead of one starving.
        let procfs = adapter_id("procfs");
        let mut served: BTreeMap<String, u32> = BTreeMap::new();
        let mut outstanding: Option<(Uuid, u64)> = None;
        let end = bench.now + 10_000 * MS;
        while bench.now < end {
            if let Some((request_id, reply_at)) = outstanding
                && bench.now >= reply_at
            {
                bench.scheduler.match_reply(&procfs, request_id, 1);
                outstanding = None;
            }
            bench.scheduler.tick(bench.now, context());
            for job in bench.port.take() {
                if let AdapterJob::Read(plan) = job {
                    *served.entry(plan.read_group.to_string()).or_default() += 1;
                    outstanding = Some((plan.request_id, bench.now + 200 * MS));
                }
            }
            bench.now += 10 * MS;
        }
        assert_eq!(served.len(), 4, "{served:?}");
        assert!(served.values().all(|count| *count >= 8), "{served:?}");
    }

    /// Smallest arc of the `period_ns` circle that holds every instant.
    fn circular_spread_ns(instants: &[u64], period_ns: u64) -> u64 {
        let mut offsets: Vec<u64> = instants.iter().map(|ns| ns % period_ns).collect();
        offsets.sort_unstable();
        let wrap_gap = offsets[0] + period_ns - offsets[offsets.len() - 1];
        let largest_gap = offsets
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .fold(wrap_gap, u64::max);
        period_ns - largest_gap
    }

    #[test]
    fn groups_of_one_adapter_share_its_phase_and_adapters_stay_spread() {
        let config = Config::defaults().unwrap();
        let mut bench = bench(&[manifest("procfs", 100), manifest("nvml", 100)], &config);
        for adapter in ["procfs", "nvml"] {
            bench.host_up(adapter);
        }
        let mut instants: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        let mut counts = Counts::default();
        let end = bench.now + 10_000 * MS;
        while bench.now < end {
            bench.scheduler.tick(bench.now, context());
            for job in bench.port.take() {
                if let AdapterJob::Read(plan) = &job {
                    instants
                        .entry(plan.adapter_id.to_string())
                        .or_default()
                        .push(bench.now);
                }
                bench.observe(job, true, &mut counts);
            }
            bench.now += MS;
        }
        // One shared slot per adapter (RUNTIME.md §3): its due groups go out
        // back to back, one per 1 ms tick, so a collector fed by several
        // groups of one host sees one burst per cycle. 5 s slow_health slots
        // land on 1 s fast slots.
        let second = NANOS_PER_SECOND;
        assert_eq!(reads(&counts, "nvml", "slow_health"), 2, "{counts:?}");
        for adapter in ["procfs", "nvml"] {
            let spread = circular_spread_ns(&instants[adapter], second);
            assert!(spread <= 5 * MS, "{adapter}: spread {spread} ns");
        }
        let mut both = instants["procfs"].clone();
        both.extend(&instants["nvml"]);
        let spread = circular_spread_ns(&both, second);
        assert!(spread > 50 * MS, "adapters share a phase: {spread} ns");
    }

    #[test]
    fn early_discovery_is_rate_limited_and_recovery_triggers_it() {
        let config = Config::defaults().unwrap();
        let mut bench = bench(&[manifest("hwmon", 100)], &config);
        bench.host_up("hwmon");
        bench.drive(1, 1, true);
        let hwmon = adapter_id("hwmon");
        assert!(!bench.scheduler.request_early_discovery(&hwmon, bench.now));
        bench.now += 2_000 * MS;
        assert!(bench.scheduler.request_early_discovery(&hwmon, bench.now));
        assert!(!bench.scheduler.request_early_discovery(&hwmon, bench.now));
        bench.drive(10, 10, true);
        // Circuit opens, then half-open recovery requests discovery early.
        bench.scheduler.on_host_status(&hwmon, 1, false, bench.now);
        bench.now += 5_000 * MS;
        bench.scheduler.on_host_status(&hwmon, 1, true, bench.now);
        // One job per 1 ms tick: hwmon's due read groups (`fast`, `disk`)
        // may go out before the discovery.
        let counts = bench.drive(10, 1, true);
        assert_eq!(counts.discoveries.get("hwmon"), Some(&1));
    }

    #[test]
    fn refused_dispatches_are_deferrals_labelled_by_reason() {
        let mut deferred = DeferredDispatches::default();
        assert_eq!(
            deferred.by_reason().count(),
            0,
            "nothing refused, no labels"
        );
        deferred.record(&DispatchError::Busy);
        deferred.record(&DispatchError::Busy);
        deferred.record(&DispatchError::Closed);
        deferred.record(&DispatchError::Capacity(
            dgxtop_core::budget::CapacityError::ItemsExhausted {
                queue: "ingress",
                limit: 4,
            },
        ));
        assert_eq!(
            deferred.by_reason().collect::<Vec<_>>(),
            vec![("worker_busy", 2), ("capacity", 1), ("closed", 1)]
        );
    }

    #[test]
    fn a_busy_worker_defers_the_due_read_until_the_retry_instant() {
        let config = Config::defaults().unwrap();
        let mut bench = bench(&[manifest("network", 100)], &config);
        bench.host_up("network");
        bench.drive(1, 1, true);
        bench.port.set_busy(true);
        let mut deferred = DeferredDispatches::default();
        while deferred.worker_busy == 0 {
            bench.now += 10 * MS;
            deferred = bench.scheduler.tick(bench.now, context()).deferred;
        }
        bench.port.set_busy(false);
        let early = bench.scheduler.tick(bench.now + MS, context());
        assert_eq!(early.dispatched, 0, "no retry before the retry instant");
        let retry = bench
            .scheduler
            .tick(bench.now + DISPATCH_RETRY_NS, context());
        assert_eq!(retry.dispatched, 1, "the kept work goes out on retry");
        assert_eq!(retry.deferred, DeferredDispatches::default());
    }

    #[test]
    fn unavailable_host_reports_missed_slots_and_expired_requests() {
        let config = Config::defaults().unwrap();
        let mut bench = bench(&[manifest("thermal", 100)], &config);
        bench.host_up("thermal");
        bench.drive(1, 1, true);
        let thermal = adapter_id("thermal");
        // A read that never gets an answer expires after deadline + grace.
        let mut expired = Vec::new();
        let end = bench.now + 2_000 * MS;
        while bench.now < end {
            expired.extend(bench.scheduler.tick(bench.now, context()).expired);
            bench.port.take();
            bench.now += 10 * MS;
        }
        assert!(!expired.is_empty());
        assert!(matches!(expired[0].kind, RequestKind::Read { .. }));
        bench
            .scheduler
            .on_host_status(&thermal, 1, false, bench.now);
        let mut missed = Vec::new();
        let end = bench.now + 3_000 * MS;
        while bench.now < end {
            missed.extend(bench.scheduler.tick(bench.now, context()).missed);
            bench.now += 10 * MS;
        }
        assert!((2..=3).contains(&missed.len()), "{}", missed.len());
        assert!(bench.port.is_empty());
        bench.scheduler.stop();
        assert!(bench.scheduler.next_deadline().is_none());
    }
}
