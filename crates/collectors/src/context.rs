//! Per-job collection session shared by the six collectors.
//!
//! A session gathers the records of the batches a collector consumes (routing
//! via `naming::collectors_for`, replay groups included), merges them with the
//! candidates retained in the committed state (latest record per source),
//! resolves catalog metadata and freshness deadlines, runs arbitration and
//! assembles the `CollectionBatch`. The plan's state is never mutated: all
//! changes go to a separate `next_state`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use dgxtop_core::canonical::{
    CollectionBatch, CollectionPlan, CounterBase, CounterInterval, LastGood, PolicySnapshot,
    ProcessBase, ProcessTable, SelectionMemo,
};
use dgxtop_core::identity::{AdapterId, MetricKey, ProcessKey, SourceId};
use dgxtop_core::metric::{self, MetricDescriptor, MetricRole, Scalar};
use dgxtop_core::naming;
use dgxtop_core::observation::{
    BatchStatus, Observation, ObservationBatch, PAGE_CONTINUES, SourceDescriptor, split_page,
};
use dgxtop_core::quality::RawKind;

use crate::arbitration::{self, Candidate, CatalogMapping, Deadlines, Outcome, Request, Selection};
use crate::output::{Output, Subject};
use crate::reasons;
use crate::state_bounds;
use crate::values::ValueContext;

/// Period group of `RawKind::Static` sources: their values are refreshed by
/// discovery, so `naming::static_period_key` names their period.
pub(crate) const STATIC_PERIOD_GROUP: &str = "discovery";

/// Metric id prefix of unclassified diagnostic channels.
pub(crate) const SENSOR_PREFIX: &str = "sensor.";

/// What one collector consumes and retains.
pub(crate) struct CollectorSpec {
    pub id: &'static str,
    /// Metrics of consumed batches this collector interprets.
    pub accepts: fn(&MetricDescriptor) -> bool,
    /// Whether the latest record of a source is retained across jobs.
    pub retains: fn(&Route<'_>, &MetricDescriptor) -> bool,
}

/// Where a batch came from. Replay groups (`<live adapter>.<group>`) are
/// resolved to the live group they imitate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Route<'a> {
    pub adapter: &'a AdapterId,
    /// Read group without page suffix.
    pub read_group: &'a str,
    pub live_adapter: &'a str,
    pub live_group: &'a str,
    /// The batch is known not to cover its whole read group: a failed or
    /// partial read with an error code (e.g. `NVML_PROCESS_LIST_PARTIAL`),
    /// or a page whose following pages are not in this plan.
    pub incomplete: bool,
}

impl<'a> Route<'a> {
    pub fn of(batch: &'a ObservationBatch) -> Option<Self> {
        let (read_group, _) = split_page(batch.read_group.as_str());
        let adapter = batch.adapter_id.as_str();
        let (live_adapter, live_group) = if naming::is_replay_adapter(adapter) {
            read_group.split_once('.')?
        } else {
            (adapter, read_group)
        };
        Some(Self {
            adapter: &batch.adapter_id,
            read_group,
            live_adapter,
            live_group,
            incomplete: reports_incomplete_read(batch),
        })
    }

    pub fn is(&self, live_adapter: &str, live_group: &str) -> bool {
        self.live_adapter == live_adapter && self.live_group == live_group
    }
}

/// A failed batch, or a partial one carrying an error code other than the
/// normal page continuation, does not cover its read group.
fn reports_incomplete_read(batch: &ObservationBatch) -> bool {
    match batch.status {
        BatchStatus::Failed => true,
        BatchStatus::Partial => batch
            .error_code
            .as_deref()
            .is_some_and(|code| code != PAGE_CONTINUES),
        BatchStatus::Ok => false,
    }
}

/// One record visible to the collector in this job.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Record<'a> {
    pub observation: &'a Observation,
    /// Route of a record delivered by this plan; `None` when it is a
    /// candidate retained from the committed state.
    pub route: Option<Route<'a>>,
}

impl<'a> Record<'a> {
    pub fn is_new(&self) -> bool {
        self.route.is_some()
    }

    pub fn source_id(&self) -> &'a SourceId {
        &self.observation.provenance.source_id
    }

    pub fn key(&self) -> &'a MetricKey {
        &self.observation.key
    }

    /// Device time when known, else read time.
    pub fn sample_time_boot_ns(&self) -> u64 {
        self.observation
            .device_sample_boot_ns
            .unwrap_or(self.observation.read_finished_boot_ns)
    }

    fn supersedes(&self, other: &Record<'_>) -> bool {
        let rank = |record: &Record<'_>| {
            (
                record.observation.read_finished_boot_ns,
                record.is_new(),
                record.observation.cycle_id,
            )
        };
        rank(self) > rank(other)
    }
}

type RecordsByKey<'a> = BTreeMap<&'a MetricKey, BTreeMap<&'a SourceId, Record<'a>>>;

/// One collection job in progress.
pub(crate) struct Session<'a> {
    plan: &'a CollectionPlan,
    spec: &'static CollectorSpec,
    records: RecordsByKey<'a>,
    routes: Vec<(Route<'a>, BatchStatus)>,
    expiring: BTreeSet<&'a MetricKey>,
    touched: BTreeSet<MetricKey>,
    next_state: dgxtop_core::canonical::CollectorState,
    output: Output,
}

/// Runs `body` in a session for `plan`, or returns an empty batch with a
/// failure when the plan is addressed to another collector.
pub(crate) fn run(
    plan: &CollectionPlan,
    spec: &'static CollectorSpec,
    body: impl FnOnce(&mut Session<'_>),
) -> CollectionBatch {
    let mut session = Session::begin(plan, spec);
    if plan.collector_id.as_str() == spec.id {
        body(&mut session);
    } else {
        let message = format!(
            "plan for collector {} delivered to {}",
            plan.collector_id, spec.id
        );
        session.output = Output::default();
        session.next_state = (*plan.state).clone();
        session.failure(reasons::PLAN_COLLECTOR_MISMATCH, &message, Subject::none());
    }
    session.finish()
}

impl<'a> Session<'a> {
    fn begin(plan: &'a CollectionPlan, spec: &'static CollectorSpec) -> Self {
        let mut session = Self {
            plan,
            spec,
            records: BTreeMap::new(),
            routes: Vec::new(),
            expiring: BTreeSet::new(),
            touched: BTreeSet::new(),
            next_state: (*plan.state).clone(),
            output: Output::default(),
        };
        session.gather_batches();
        session.merge_retained();
        session.refresh_retention();
        session
    }

    fn gather_batches(&mut self) {
        let plan = self.plan;
        let final_pages: BTreeSet<(&str, &str, u64)> = plan
            .inputs
            .iter()
            .map(|ingress| ingress.batch())
            .filter(|batch| !batch.continues())
            .map(|batch| {
                let (group, _) = split_page(batch.read_group.as_str());
                (batch.adapter_id.as_str(), group, batch.cycle_id)
            })
            .collect();
        for ingress in &plan.inputs {
            let batch: &'a ObservationBatch = ingress.batch();
            let Some(mut route) = Route::of(batch) else {
                continue;
            };
            if !naming::collectors_for(batch.adapter_id.as_str(), route.read_group)
                .contains(&self.spec.id)
            {
                continue;
            }
            // Pages arrive in order: a continuing page without its final page
            // in this plan means the rest of the snapshot is still missing.
            if batch.continues()
                && !final_pages.contains(&(
                    batch.adapter_id.as_str(),
                    route.read_group,
                    batch.cycle_id,
                ))
            {
                route.incomplete = true;
            }
            self.routes.push((route, batch.status));
            self.report_batch_status(batch);
            for observation in &batch.records {
                self.offer_new(observation, route);
            }
        }
    }

    /// Failed and partial reads are reported, never turned into values.
    fn report_batch_status(&mut self, batch: &ObservationBatch) {
        let code = match batch.status {
            BatchStatus::Failed => reasons::BATCH_FAILED,
            BatchStatus::Partial if reports_incomplete_read(batch) => reasons::BATCH_PARTIAL,
            BatchStatus::Ok | BatchStatus::Partial => return,
        };
        let message = format!(
            "{}/{}: {}",
            batch.adapter_id,
            batch.read_group,
            batch.error_code.as_deref().unwrap_or("no error code")
        );
        self.failure(code, &message, Subject::none());
    }

    fn offer_new(&mut self, observation: &'a Observation, route: Route<'a>) {
        let metric_id = observation.key.metric_id.as_str();
        match metric::descriptor(metric_id) {
            None => self.failure(
                reasons::UNKNOWN_METRIC,
                &format!("unknown metric id {metric_id}"),
                Subject::source(&observation.provenance.source_id),
            ),
            Some(descriptor) if !(self.spec.accepts)(descriptor) => self.failure(
                reasons::METRIC_NOT_HANDLED,
                &format!(
                    "{metric_id} is not interpreted by collector {}",
                    self.spec.id
                ),
                Subject::source(&observation.provenance.source_id),
            ),
            Some(_) => self.offer(Record {
                observation,
                route: Some(route),
            }),
        }
    }

    fn merge_retained(&mut self) {
        let plan = self.plan;
        for observation in plan.state.candidates.values().map(Arc::as_ref) {
            let accepted = metric::descriptor(observation.key.metric_id.as_str())
                .is_some_and(|descriptor| (self.spec.accepts)(descriptor));
            if accepted {
                self.offer(Record {
                    observation,
                    route: None,
                });
            }
        }
    }

    /// Newest reading per source and key wins (a late batch never replaces
    /// a newer retained reading).
    fn offer(&mut self, record: Record<'a>) {
        let sources = self.records.entry(record.key()).or_default();
        match sources.get(record.source_id()) {
            Some(existing) if !record.supersedes(existing) => {}
            _ => {
                sources.insert(record.source_id(), record);
            }
        }
    }

    /// Retains new records the collector keeps; drops retained ones whose
    /// read deadline passed (their keys are re-published once as expired).
    fn refresh_retention(&mut self) {
        let now = self.plan.now_boot_ns;
        let mut retain: Vec<Record<'a>> = Vec::new();
        let mut expire: Vec<Record<'a>> = Vec::new();
        for record in self.records.values().flat_map(BTreeMap::values) {
            let expired = self.deadlines(*record).read_boot_ns <= now;
            match (record.route, expired) {
                (Some(route), false) if self.retains(&route, record.observation) => {
                    retain.push(*record);
                }
                (_, true) => expire.push(*record),
                _ => {}
            }
        }
        for record in retain {
            let slot = self.retention_key(record.source_id(), record.key());
            self.next_state
                .candidates
                .insert(slot, Arc::new(record.observation.clone()));
        }
        for record in expire {
            let slot = self.retention_key(record.source_id(), record.key());
            let same_entry = self
                .next_state
                .candidates
                .get(&slot)
                .is_some_and(|retained| retained.key == record.observation.key);
            if same_entry {
                self.next_state.candidates.remove(&slot);
            }
            if !record.is_new() {
                self.expiring.insert(record.key());
            }
        }
    }

    /// Slot of a retained candidate in `CollectorState::candidates`: the
    /// source id when the catalog maps the source to exactly this key, else
    /// `<source>#<entity>`, so a source multiplexing dynamic keys (NVML
    /// per-GPU process lists) or a collector-derived candidate keeps one
    /// entry per series instead of overwriting itself.
    pub fn retention_key(&self, source: &SourceId, key: &MetricKey) -> SourceId {
        let maps_exactly = self
            .source_descriptor(source)
            .and_then(|descriptor| descriptor.metric.as_ref())
            == Some(key);
        if maps_exactly {
            return source.clone();
        }
        SourceId::new(format!("{source}#{}", key.entity_id)).unwrap_or_else(|_| source.clone())
    }

    fn retains(&self, route: &Route<'_>, observation: &Observation) -> bool {
        metric::descriptor(observation.key.metric_id.as_str())
            .is_some_and(|descriptor| (self.spec.retains)(route, descriptor))
    }

    // ----- plan access -------------------------------------------------

    pub fn plan(&self) -> &'a CollectionPlan {
        self.plan
    }

    pub fn now(&self) -> u64 {
        self.plan.now_boot_ns
    }

    pub fn policy(&self) -> &'a PolicySnapshot {
        let plan = self.plan;
        &plan.policy
    }

    pub fn context(&self) -> ValueContext {
        ValueContext {
            cycle_id: self.plan.cycle_id,
            policy_revision: self.plan.policy.revision,
            namespace: self.plan.policy.namespace,
        }
    }

    /// Batches of consumed routes delivered by this plan.
    pub fn routes(&self) -> &[(Route<'a>, BatchStatus)] {
        &self.routes
    }

    // ----- records ----------------------------------------------------------

    /// Every key with at least one record.
    pub fn keys(&self) -> Vec<&'a MetricKey> {
        self.records.keys().copied().collect()
    }

    /// Records of `key`, one per source.
    pub fn records(&self, key: &MetricKey) -> Vec<Record<'a>> {
        self.records
            .get(key)
            .map(|sources| sources.values().copied().collect())
            .unwrap_or_default()
    }

    /// Records (new and retained) of the keys `wanted` accepts, in key order,
    /// without a lookup per key.
    pub fn records_where<'s>(
        &'s self,
        wanted: impl Fn(&MetricKey) -> bool + 's,
    ) -> impl Iterator<Item = Record<'a>> + 's {
        self.records
            .iter()
            .filter(move |(key, _)| wanted(key))
            .flat_map(|(_, sources)| sources.values().copied())
    }

    /// Every record of this job (new and retained).
    pub fn all_records(&self) -> Vec<Record<'a>> {
        self.records
            .values()
            .flat_map(BTreeMap::values)
            .copied()
            .collect()
    }

    /// Whether `key` has a record delivered by this plan.
    pub fn has_new(&self, key: &MetricKey) -> bool {
        self.records
            .get(key)
            .is_some_and(|sources| sources.values().any(Record::is_new))
    }

    /// Whether a retained candidate of `key` reached its read deadline now.
    pub fn is_expiring(&self, key: &MetricKey) -> bool {
        self.expiring.contains(key)
    }

    /// Keys with new evidence: a record of this plan, or a retained candidate
    /// expiring now.
    pub fn pending_keys(&self, mut wanted: impl FnMut(&MetricKey) -> bool) -> Vec<&'a MetricKey> {
        self.records
            .iter()
            .filter(|(key, sources)| {
                wanted(key)
                    && (sources.values().any(Record::is_new) || self.expiring.contains(*key))
            })
            .map(|(key, _)| *key)
            .collect()
    }

    // ----- catalog and freshness -------------------------------------------

    pub fn source_descriptor(&self, source: &SourceId) -> Option<&'a SourceDescriptor> {
        let plan = self.plan;
        plan.catalog.source(source).map(|descriptor| &**descriptor)
    }

    /// Catalog raw kind of the source, else the metric's kind.
    pub fn raw_kind(&self, observation: &Observation) -> RawKind {
        self.source_descriptor(&observation.provenance.source_id)
            .map(|descriptor| descriptor.raw_kind)
            .or_else(|| {
                metric::descriptor(observation.key.metric_id.as_str()).map(|metric| metric.kind)
            })
            .unwrap_or(RawKind::Gauge)
    }

    /// Static metadata values (limits, maxima, totals) are refreshed by
    /// discovery, whichever group delivers them; either the catalog source
    /// or the metric catalog may declare a value static.
    fn is_static(&self, observation: &Observation) -> bool {
        let declared = self
            .source_descriptor(&observation.provenance.source_id)
            .is_some_and(|descriptor| descriptor.raw_kind == RawKind::Static);
        declared
            || metric::descriptor(observation.key.metric_id.as_str())
                .is_some_and(|metric| metric.kind == RawKind::Static)
    }

    pub fn deadlines(&self, record: Record<'a>) -> Deadlines {
        let observation = record.observation;
        let (adapter, group) = self.period_group(record);
        let interval = self.update_interval(observation);
        let policy = self.policy();
        Deadlines {
            read_boot_ns: policy.fresh_deadline(
                adapter,
                group,
                interval,
                observation.read_finished_boot_ns,
            ),
            measurement_boot_ns: observation
                .device_sample_boot_ns
                .map(|device| policy.fresh_deadline(adapter, group, interval, device)),
        }
    }

    /// Longest interval a counter delta of this source may span.
    pub fn stale_window_ns(&self, record: Record<'a>) -> u64 {
        let (adapter, group) = self.period_group(record);
        self.policy()
            .fresh_deadline(adapter, group, self.update_interval(record.observation), 0)
    }

    fn period_group(&self, record: Record<'a>) -> (&'a AdapterId, &'a str) {
        let observation = record.observation;
        let adapter = &observation.provenance.adapter_id;
        if self.is_static(observation) {
            return (adapter, STATIC_PERIOD_GROUP);
        }
        if let Some(route) = record.route {
            return (route.adapter, route.read_group);
        }
        let group = self
            .source_descriptor(&observation.provenance.source_id)
            .map_or("", |descriptor| descriptor.read_group.as_str());
        (adapter, group)
    }

    fn update_interval(&self, observation: &Observation) -> Option<u32> {
        observation.source_update_interval_ms.or_else(|| {
            self.source_descriptor(&observation.provenance.source_id)
                .and_then(|descriptor| descriptor.update_interval_ms)
        })
    }

    /// Arbitration candidate for a record, with catalog defaults for sources
    /// that are not catalogued (rank 0, kind from the metric descriptor).
    pub fn candidate(&self, record: Record<'a>) -> Candidate<'a> {
        let observation = record.observation;
        let descriptor = self.source_descriptor(&observation.provenance.source_id);
        Candidate {
            observation,
            profile_rank: descriptor.map_or(0, |descriptor| descriptor.profile_rank),
            raw_kind: self.raw_kind(observation),
            mapping: mapping_of(descriptor, &observation.key),
            deadlines: self.deadlines(record),
            input_source_ids: Vec::new(),
            previous_value: self.previous_value(record),
        }
    }

    pub fn candidates(&self, key: &MetricKey) -> Vec<Candidate<'a>> {
        self.records(key)
            .into_iter()
            .map(|record| self.candidate(record))
            .collect()
    }

    /// Value of the committed earlier reading of the same source, if older.
    fn previous_value(&self, record: Record<'a>) -> Option<Scalar> {
        if !record.is_new() {
            return None;
        }
        let slot = self.retention_key(record.source_id(), record.key());
        let previous = self.plan.state.candidates.get(&slot)?;
        let current = record.observation;
        let earlier = previous.key == current.key
            && previous.provenance.generation == current.provenance.generation
            && previous.read_finished_boot_ns < current.read_finished_boot_ns;
        if earlier { previous.value } else { None }
    }

    // ----- arbitration and publication ---------------------------------

    /// Arbitrates `key` over `candidates`; records the memo and returns the
    /// outcome for the caller to publish. `None` for an unknown metric id.
    pub fn arbitrate(
        &mut self,
        key: &MetricKey,
        candidates: Vec<Candidate<'_>>,
        notes: Vec<String>,
    ) -> Option<Outcome> {
        let descriptor = metric::descriptor(key.metric_id.as_str())?;
        let plan = self.plan;
        let memo = plan.state.selections.get(key);
        let candidate_count = candidates.len();
        let outcome = arbitration::arbitrate(Request {
            key,
            descriptor,
            cycle_id: plan.cycle_id,
            now_boot_ns: plan.now_boot_ns,
            policy: &plan.policy,
            memo,
            candidates,
            max_candidates: usize::try_from(plan.limits.max_candidates_per_series)
                .unwrap_or(arbitration::MAX_CANDIDATES_PER_SERIES),
            notes,
        });
        self.touched.insert(key.clone());
        self.remember_memo(key, outcome.memo.clone(), candidate_count, memo.is_some());
        if !outcome.dropped_candidates.is_empty() {
            let message = format!(
                "{}: {} candidate(s) beyond the per-series limit",
                key.series_selector(),
                outcome.dropped_candidates.len()
            );
            self.failure(
                reasons::CANDIDATE_LIMIT_EXCEEDED,
                &message,
                Subject::key(key),
            );
        }
        Some(outcome)
    }

    /// Memos are kept only where hysteresis can matter: several candidates,
    /// a pin, or an existing memo.
    fn remember_memo(
        &mut self,
        key: &MetricKey,
        memo: Option<SelectionMemo>,
        candidate_count: usize,
        had_memo: bool,
    ) {
        let pinned = self.plan.policy.pinned_sources.contains_key(key);
        match memo {
            Some(memo) if candidate_count >= 2 || had_memo || pinned => {
                self.next_state.selections.insert(key.clone(), memo);
            }
            _ => {
                self.next_state.selections.remove(key);
            }
        }
    }

    /// Publishes an arbitration outcome. Canonical metrics get a value,
    /// explanation and events; `sensor.*` keys only an explanation
    /// (diagnostics); raw inputs nothing. Returns the selection.
    pub fn publish(&mut self, outcome: Outcome) -> Option<Selection> {
        let Outcome {
            canonical,
            explanation,
            events,
            selection,
            has_namespace_evidence,
            ..
        } = outcome;
        let metric_id = canonical.key.metric_id.as_str();
        let diagnostic = metric_id.starts_with(SENSOR_PREFIX);
        let canonical_metric = !diagnostic
            && metric::descriptor(metric_id)
                .is_some_and(|descriptor| descriptor.role == MetricRole::Canonical);
        if diagnostic || canonical_metric {
            self.output.explanations.push(explanation);
        }
        if canonical_metric && has_namespace_evidence {
            self.output.events.extend(events);
            if !self.already_committed(&canonical) {
                self.publish_value(canonical);
            }
        }
        selection
    }

    /// Arbitrates and publishes in one step.
    pub fn arbitrate_and_publish(
        &mut self,
        key: &MetricKey,
        candidates: Vec<Candidate<'_>>,
        notes: Vec<String>,
    ) -> Option<Selection> {
        let outcome = self.arbitrate(key, candidates, notes)?;
        self.publish(outcome)
    }

    /// Re-arbitrating a retained reading must not publish the same sample
    /// twice: suppressed when the committed selection still holds this source
    /// and its last committed sample is this one. A value-less round clears
    /// the committed selection, so a value returning after a conflict or an
    /// unavailable round is always published again.
    fn already_committed(&self, value: &dgxtop_core::canonical::CanonicalValue) -> bool {
        let state = &self.plan.state;
        let Some(source) = value.source_id.as_ref() else {
            return false;
        };
        let still_selected = state
            .selections
            .get(&value.key)
            .and_then(|memo| memo.selected.as_ref())
            == Some(source);
        let same_sample = state.last_good.get(&value.key).is_some_and(|last| {
            last.source_id == *source && last.sample_boot_ns == value.sample_time_boot_ns()
        });
        value.selection_state.has_value() && still_selected && same_sample
    }

    /// Validates and records a canonical value; a value-bearing one becomes
    /// the key's `last_good` annotation.
    ///
    /// Live and replay data never mix: a value whose kind belongs to another
    /// namespace than the policy (e.g. derived from replayed inputs under a
    /// live policy) is never published.
    pub fn publish_value(&mut self, value: dgxtop_core::canonical::CanonicalValue) {
        if value.sample_kind.namespace() != self.plan.policy.namespace {
            let message = format!(
                "{}: {:?} value outside the {:?} namespace",
                value.key.series_selector(),
                value.sample_kind,
                self.plan.policy.namespace
            );
            self.failure(
                reasons::NAMESPACE_MISMATCH,
                &message,
                Subject::key(&value.key),
            );
            return;
        }
        let last_good = match (&value.value, &value.source_id) {
            (Some(scalar), Some(source_id)) if value.selection_state.has_value() => {
                Some(LastGood {
                    value: *scalar,
                    unit: value.unit,
                    sample_boot_ns: value.sample_time_boot_ns(),
                    source_id: source_id.clone(),
                })
            }
            _ => None,
        };
        let key = value.key.clone();
        if self.output.push_canonical(value)
            && let Some(last_good) = last_good
        {
            self.next_state.last_good.insert(key, last_good);
        }
    }

    // ----- state ------------------------------------------------------------

    pub fn committed_counter(&self, source: &SourceId) -> Option<&'a CounterBase> {
        let plan = self.plan;
        plan.state.counters.get(source)
    }

    pub fn set_counter(&mut self, source: SourceId, base: CounterBase) {
        self.next_state.counters.insert(source, base);
    }

    pub fn committed_process(&self, key: &ProcessKey) -> Option<&'a ProcessBase> {
        let plan = self.plan;
        plan.state.processes.get(key)
    }

    pub fn set_process(&mut self, key: ProcessKey, base: ProcessBase) {
        self.next_state.processes.insert(key, base);
    }

    /// Process bases of the next state (for collector-specific pruning).
    pub fn next_processes_mut(&mut self) -> &mut BTreeMap<ProcessKey, ProcessBase> {
        &mut self.next_state.processes
    }

    /// Retains a collector-built observation (e.g. a derived power candidate)
    /// as the latest candidate of its series.
    pub fn retain(&mut self, observation: Observation) {
        let slot = self.retention_key(&observation.provenance.source_id, &observation.key);
        self.next_state
            .candidates
            .insert(slot, Arc::new(observation));
    }

    /// Drops the retained candidate of `source` for `key`.
    pub fn forget(&mut self, source: &SourceId, key: &MetricKey) {
        let slot = self.retention_key(source, key);
        self.next_state.candidates.remove(&slot);
    }

    // ----- outputs ----------------------------------------------------------

    pub fn interval(&mut self, interval: CounterInterval) {
        self.output.intervals.push(interval);
    }

    pub fn failure(&mut self, code: &str, message: &str, subject: Subject) {
        self.output.failure(code, message, subject);
    }

    pub fn set_process_table(&mut self, table: ProcessTable) {
        self.output.process_table = Some(table);
    }

    fn finish(mut self) -> CollectionBatch {
        let plan = self.plan;
        state_bounds::enforce(&mut self.next_state, &plan.limits, &self.touched);
        self.next_state.revision = plan.state_revision.saturating_add(1);
        let mut output = self.output;
        output
            .canonical
            .sort_by(|left, right| left.key.cmp(&right.key));
        output
            .explanations
            .sort_by(|left, right| left.key.cmp(&right.key));
        output.intervals.sort_by(|left, right| {
            (&left.key, left.start_boot_ns, &left.source_id).cmp(&(
                &right.key,
                right.start_boot_ns,
                &right.source_id,
            ))
        });
        CollectionBatch {
            batch_id: plan.batch_id,
            collector_id: plan.collector_id.clone(),
            cycle_id: plan.cycle_id,
            epoch: plan.epoch,
            config_revision: plan.config_revision,
            catalog_revision: plan.catalog.revision,
            policy_revision: plan.policy.revision,
            state_base_revision: plan.state_revision,
            canonical: output.canonical,
            explanations: output.explanations,
            counter_intervals: output.intervals,
            inputs: plan.inputs.clone(),
            process_table: output.process_table,
            next_state: self.next_state,
            failures: output.failures,
            events: output.events,
        }
    }
}

fn mapping_of(descriptor: Option<&SourceDescriptor>, key: &MetricKey) -> CatalogMapping {
    match descriptor {
        None => CatalogMapping::NotCatalogued,
        Some(descriptor) => match &descriptor.metric {
            Some(mapped) if mapped == key => CatalogMapping::Matches,
            Some(_) => CatalogMapping::KeyMismatch,
            None if carries_dynamic_key(key) => CatalogMapping::NotCatalogued,
            None => CatalogMapping::UnprovenPurpose,
        },
    }
}

/// Per-process series (`process:…`, `gpupid:…`) are keyed by each record:
/// their sources are catalogued once (per GPU / per adapter) without a
/// fixed key. Every other source without a mapped key has no proven purpose.
fn carries_dynamic_key(key: &MetricKey) -> bool {
    metric::descriptor(key.metric_id.as_str())
        .is_some_and(|descriptor| descriptor.domain == dgxtop_core::metric::Domain::Process)
}

/// Whether `observation` is a collector-built candidate carrying `formula`.
pub(crate) fn carries_formula(observation: &Observation, formula: &str) -> bool {
    observation.reason_codes.iter().any(|code| code == formula)
}

#[cfg(test)]
mod tests {
    use super::*;

    use dgxtop_core::naming::{period_key, static_period_key};

    #[test]
    fn static_period_group_matches_naming_contract() {
        assert_eq!(
            period_key("nvml", STATIC_PERIOD_GROUP),
            static_period_key("nvml")
        );
    }
}
