//! Test fixtures: hand-built snapshots, catalogs and presentation contexts
//! made only of core types (no I/O, no clocks).

use std::collections::BTreeMap;
use std::sync::Arc;

use dgxtop_core::action::{PersistenceKind, PersistenceStatus};
use dgxtop_core::canonical::{
    CandidateDisposition, CandidateExplanation, CanonicalSample, CatalogSnapshot, DomainSnapshot,
    Explanation, ImmutableSnapshot, LastGood, ProcessGpuLink, ProcessRow, SnapshotEntry,
};
use dgxtop_core::commands::{SortKey, ViewKind};
use dgxtop_core::config::{Config, ValueOrigin};
use dgxtop_core::identity::{
    EntityId, MetricKey, OriginId, OriginIdentity, Pid, PidNamespace, ProcessKey, SessionId,
};
use dgxtop_core::metric::{NativeUnit, Scalar};
use dgxtop_core::naming::collector_id;
use dgxtop_core::observation::{Attribute, EntityKind, EntityRecord};
use dgxtop_core::quality::{Freshness, Quality, ReadStatus, SampleKind, SelectionState};
use dgxtop_core::testing::{adapter_id, boot_id, host_id, metric_key, source_id};
use dgxtop_core::view::{
    ActionsView, DiskHistory, HistoryChartKind, HistoryChartView, HistoryPointView, HistoryView,
    HostState, HostStatus, IntervalView, OverloadState, SessionState,
};

use super::cell::canonical_unit_of;
use super::{HostFacts, PresentationContext, SettingsInputs, TrendBook};

/// Boot-clock "now" of every fixture.
pub(crate) const NOW: u64 = 100_000_000_000;
const DEFAULT_SOURCE: &str = "procfs:test:channel";

pub(crate) struct SampleBuilder {
    entry: SnapshotEntry,
}

/// An unavailable sample of `metric` on `entity`/`scope`; chain `f64`/`u64`
/// to select a value.
pub(crate) fn sample(metric: &str, entity: &str, scope: &str) -> SampleBuilder {
    SampleBuilder {
        entry: SnapshotEntry {
            sample: CanonicalSample {
                schema_version: 1,
                snapshot_revision: 1,
                cycle_id: 1,
                key: metric_key(entity, metric, scope),
                source_id: None,
                source_generation: None,
                unit: canonical_unit_of(metric),
                value: None,
                sample_kind: SampleKind::Measured,
                selection_state: SelectionState::Unavailable,
                quality: Quality::Unknown,
                freshness: Freshness::Fresh,
                observed_boot_ns: None,
                committed_boot_ns: NOW,
                policy_revision: 1,
                candidate_source_ids: Vec::new(),
                reason_codes: Vec::new(),
                eligible_for_history: false,
                measurement_freshness: Freshness::Unknown,
                read_finished_boot_ns: NOW - 100_000_000,
            },
            last_good: None,
            stale_after_boot_ns: None,
        },
    }
}

impl SampleBuilder {
    pub(crate) fn f64(self, value: f64) -> Self {
        self.selected(Scalar::from_f64(value).unwrap())
    }

    pub(crate) fn u64(self, value: u64) -> Self {
        self.selected(Scalar::from_u64(value))
    }

    fn selected(mut self, value: Scalar) -> Self {
        let sample = &mut self.entry.sample;
        sample.value = Some(value);
        sample.selection_state = SelectionState::Selected;
        sample.quality = Quality::Verified;
        if sample.source_id.is_none() {
            sample.source_id = Some(source_id(DEFAULT_SOURCE));
        }
        sample.source_generation = Some(1);
        sample.eligible_for_history = true;
        self
    }

    pub(crate) fn unavailable(self) -> Self {
        self.state(SelectionState::Unavailable)
    }

    pub(crate) fn state(mut self, state: SelectionState) -> Self {
        let sample = &mut self.entry.sample;
        sample.selection_state = state;
        if !state.has_value() {
            sample.value = None;
            sample.source_id = None;
            sample.source_generation = None;
            sample.quality = Quality::Unknown;
            sample.eligible_for_history = false;
        }
        self
    }

    pub(crate) fn source(mut self, id: &str) -> Self {
        self.entry.sample.source_id = Some(source_id(id));
        self
    }

    pub(crate) fn reason(mut self, code: &str) -> Self {
        self.entry.sample.reason_codes.push(code.to_owned());
        self
    }

    pub(crate) fn read_at(mut self, boot_ns: u64) -> Self {
        self.entry.sample.read_finished_boot_ns = boot_ns;
        self
    }

    pub(crate) fn stale_after(mut self, boot_ns: u64) -> Self {
        self.entry.stale_after_boot_ns = Some(boot_ns);
        self
    }

    pub(crate) fn last_good(mut self, value: f64, sample_boot_ns: u64) -> Self {
        self.entry.last_good = Some(LastGood {
            value: Scalar::from_f64(value).unwrap(),
            unit: self.entry.sample.unit,
            sample_boot_ns,
            source_id: source_id(DEFAULT_SOURCE),
        });
        self
    }

    pub(crate) fn revision(mut self, revision: u64) -> Self {
        self.entry.sample.snapshot_revision = revision;
        self
    }

    pub(crate) fn build(self) -> SnapshotEntry {
        self.entry
    }
}

pub(crate) struct CandidateBuilder {
    candidate: CandidateExplanation,
}

/// A healthy standby candidate without a value.
pub(crate) fn candidate(source: &str, adapter: &str) -> CandidateBuilder {
    CandidateBuilder {
        candidate: CandidateExplanation {
            source_id: source_id(source),
            adapter_id: adapter_id(adapter),
            origin: OriginIdentity::unknown(),
            profile_rank: 0,
            native_value: None,
            native_unit: NativeUnit::new("degC").unwrap(),
            value: None,
            conversion_id: None,
            read_status: ReadStatus::Ok,
            quality: Quality::Verified,
            sample_kind: SampleKind::Measured,
            freshness: Freshness::Fresh,
            measurement_freshness: Freshness::Unknown,
            read_finished_boot_ns: NOW - 100_000_000,
            observed_boot_ns: None,
            disposition: CandidateDisposition::Standby,
            suspect_reasons: Vec::new(),
        },
    }
}

impl CandidateBuilder {
    pub(crate) fn value(mut self, value: f64) -> Self {
        let scalar = Scalar::from_f64(value).unwrap();
        self.candidate.value = Some(scalar);
        self.candidate.native_value = Some(scalar);
        self
    }

    pub(crate) fn native(mut self, value: Scalar, unit: &str) -> Self {
        self.candidate.native_value = Some(value);
        self.candidate.native_unit = NativeUnit::new(unit).unwrap();
        self
    }

    pub(crate) fn disposition(mut self, disposition: CandidateDisposition) -> Self {
        self.candidate.disposition = disposition;
        self
    }

    pub(crate) fn freshness(mut self, freshness: Freshness) -> Self {
        self.candidate.freshness = freshness;
        self
    }

    pub(crate) fn quality(mut self, quality: Quality) -> Self {
        self.candidate.quality = quality;
        self
    }

    pub(crate) fn rank(mut self, rank: u16) -> Self {
        self.candidate.profile_rank = rank;
        self
    }

    pub(crate) fn origin(mut self, origin: &str) -> Self {
        self.candidate.origin = OriginIdentity::confirmed(OriginId::new(origin).unwrap());
        self
    }

    pub(crate) fn suspect(mut self, reason: &str) -> Self {
        self.candidate.suspect_reasons.push(reason.to_owned());
        self
    }

    pub(crate) fn build(self) -> CandidateExplanation {
        self.candidate
    }
}

/// Explanation whose selection state follows the candidates' dispositions.
pub(crate) fn explanation_for(
    key: &MetricKey,
    candidates: Vec<CandidateExplanation>,
) -> Explanation {
    let selected_source = candidates
        .iter()
        .find(|candidate| candidate.disposition == CandidateDisposition::Selected)
        .map(|candidate| candidate.source_id.clone());
    let conflicting = candidates
        .iter()
        .any(|candidate| candidate.disposition == CandidateDisposition::Conflicting);
    let selection_state = match (&selected_source, conflicting) {
        (Some(_), _) => SelectionState::Selected,
        (None, true) => SelectionState::Conflict,
        (None, false) => SelectionState::Unavailable,
    };
    Explanation {
        key: key.clone(),
        policy_revision: 1,
        selection_state,
        selected_source,
        pinned_source: None,
        candidates,
        conflict_spread: None,
        tolerance: None,
        switch: None,
        reason_codes: Vec::new(),
    }
}

pub(crate) fn entity_record(
    id: &str,
    kind: EntityKind,
    display_name: &str,
    attributes: &[(&str, &str)],
) -> EntityRecord {
    EntityRecord {
        entity_id: EntityId::new(id).unwrap(),
        kind,
        parent_id: None,
        generation: 1,
        discovered_boot_ns: 0,
        removed_boot_ns: None,
        display_name: display_name.to_owned(),
        attributes: attributes
            .iter()
            .map(|(key, value)| Attribute {
                key: (*key).to_owned(),
                value: (*value).to_owned(),
            })
            .collect(),
    }
}

pub(crate) fn process_key(pid: i64, start_time_ticks: u64) -> ProcessKey {
    ProcessKey {
        host_id: host_id("test-host"),
        boot_id: boot_id("fake-boot"),
        pid_namespace: PidNamespace::new("pid:[1]").unwrap(),
        pid: Pid::new(pid).unwrap(),
        start_time_ticks,
    }
}

pub(crate) fn process_row(pid: i64, name: &str) -> ProcessRow {
    ProcessRow {
        key: process_key(pid, 1000),
        name: name.to_owned(),
        uid: Some(1000),
        state: Some('S'),
        cpu_percent: None,
        rss_bytes: None,
        missing_reason: None,
        gpu: Vec::new(),
        sample_kind: SampleKind::Measured,
        read_finished_boot_ns: NOW - 100_000_000,
    }
}

pub(crate) fn gpu_link(gpu: &str, utilization: Option<f64>, memory: Option<u64>) -> ProcessGpuLink {
    ProcessGpuLink {
        gpu_entity_id: EntityId::new(gpu).unwrap(),
        usage: dgxtop_core::canonical::GpuUsageKind::Compute,
        utilization_percent: utilization,
        memory_bytes: memory,
    }
}

pub(crate) fn host_status(adapter: &str, state: HostState, detail: Option<&str>) -> HostStatus {
    HostStatus {
        adapter_id: adapter.to_owned(),
        state,
        generation: 1,
        pid: None,
        last_success_boot_ns: None,
        timeouts_total: 0,
        consecutive_failures: 0,
        restarts_total: 0,
        backoff_until_boot_ns: None,
        detail: detail.map(str::to_owned),
    }
}

pub(crate) fn empty_actions() -> ActionsView {
    ActionsView {
        confirmation: None,
        recent: Vec::new(),
        audit: PersistenceStatus::healthy(PersistenceKind::Audit),
        actions_available: true,
        unavailable_reason: None,
        unknown_outcomes: 0,
    }
}

pub(crate) fn empty_history() -> HistoryView {
    HistoryView {
        window_label: "5m".to_owned(),
        requested_span_ns: 300_000_000_000,
        start_utc_ns: 0,
        end_utc_ns: 300_000_000_000,
        charts: Vec::new(),
        summaries: Vec::new(),
        disk: DiskHistory::Off,
        error: None,
    }
}

/// A CPU history chart with `points`.
pub(crate) fn history_chart(points: Vec<HistoryPointView>) -> HistoryChartView {
    HistoryChartView {
        kind: HistoryChartKind::CpuUtilization,
        subject: String::new(),
        series: dgxtop_core::testing::metric_key("cpu", "cpu.utilization.total", "host"),
        unit: dgxtop_core::metric::CanonicalUnit::Percent,
        scale_max: Some(100.0),
        points,
        mean: None,
        peak: None,
        coverage_ratio: Some(1.0),
        covered_ns: None,
        truncated: false,
        display_downsampled: false,
        pending: false,
        error: None,
    }
}

/// Owns everything a [`PresentationContext`] borrows.
pub(crate) struct Fixture {
    pub(crate) snapshot: ImmutableSnapshot,
    pub(crate) catalog: CatalogSnapshot,
    pub(crate) session: SessionState,
    pub(crate) viewport: (u16, u16),
    pub(crate) saved: Config,
    pub(crate) effective: Config,
    pub(crate) origins: BTreeMap<String, ValueOrigin>,
    pub(crate) restart_pending: Vec<String>,
    pub(crate) pending: Option<String>,
    pub(crate) last_result: Option<String>,
    pub(crate) config_status: PersistenceStatus,
    pub(crate) archive_status: Option<PersistenceStatus>,
    pub(crate) hosts: Vec<HostStatus>,
    pub(crate) intervals: Vec<IntervalView>,
    pub(crate) overload: OverloadState,
    pub(crate) actions: Arc<ActionsView>,
    pub(crate) history: Arc<HistoryView>,
    pub(crate) trends: TrendBook,
    pub(crate) facts: HostFacts,
    pub(crate) enabled_modules: Vec<String>,
    pub(crate) now_boot_ns: u64,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let config = Config::defaults().unwrap();
        let catalog = CatalogSnapshot {
            host_id: Some(host_id("test-host")),
            boot_id: Some(boot_id("fake-boot")),
            ..CatalogSnapshot::default()
        };
        let enabled_modules = config.viewer.enabled_modules.clone();
        Self {
            snapshot: ImmutableSnapshot {
                revision: 7,
                committed_boot_ns: NOW,
                ..ImmutableSnapshot::default()
            },
            catalog,
            session: SessionState {
                session_id: SessionId::new("session-1").unwrap(),
                view: ViewKind::Overview,
                selected_entity_id: None,
                selected_process: None,
                filter: String::new(),
                sort: SortKey::GpuMemory,
                ascending: false,
            },
            viewport: (160, 50),
            saved: config.clone(),
            effective: config,
            origins: BTreeMap::new(),
            restart_pending: Vec::new(),
            pending: None,
            last_result: None,
            config_status: PersistenceStatus::healthy(PersistenceKind::Config),
            archive_status: None,
            hosts: Vec::new(),
            intervals: Vec::new(),
            overload: OverloadState::Normal,
            actions: Arc::new(empty_actions()),
            history: Arc::new(empty_history()),
            trends: TrendBook::new(120, 1000),
            facts: HostFacts {
                host_name: "dgx-test".to_owned(),
                version: "0.2.0".to_owned(),
                ..HostFacts::default()
            },
            enabled_modules,
            now_boot_ns: NOW,
        }
    }

    pub(crate) fn context(&self) -> PresentationContext<'_> {
        PresentationContext {
            frame_revision: 3,
            now_boot_ns: self.now_boot_ns,
            snapshot: &self.snapshot,
            catalog: &self.catalog,
            session: &self.session,
            viewport: self.viewport,
            settings: SettingsInputs {
                saved: &self.saved,
                effective: &self.effective,
                origins: &self.origins,
                restart_pending: &self.restart_pending,
                pending: self.pending.clone(),
                last_result: self.last_result.clone(),
                status: &self.config_status,
            },
            hosts: &self.hosts,
            intervals: &self.intervals,
            overload: self.overload,
            actions: Arc::clone(&self.actions),
            history: Arc::clone(&self.history),
            trends: &self.trends,
            facts: &self.facts,
            enabled_modules: &self.enabled_modules,
            archive: self.archive_status.as_ref(),
        }
    }

    pub(crate) fn add(&mut self, collector: &str, sample: SampleBuilder) {
        let entry = sample.build();
        self.domain(collector)
            .samples
            .insert(entry.sample.key.clone(), Arc::new(entry));
    }

    pub(crate) fn explain(&mut self, collector: &str, explanation: Explanation) {
        self.domain(collector)
            .explanations
            .insert(explanation.key.clone(), Arc::new(explanation));
    }

    pub(crate) fn add_entity(&mut self, record: EntityRecord) {
        self.catalog
            .entities
            .insert(record.entity_id.clone(), Arc::new(record));
    }

    pub(crate) fn set_processes(&mut self, rows: Vec<ProcessRow>) {
        let table = dgxtop_core::canonical::ProcessTable {
            total_seen: u32::try_from(rows.len()).unwrap(),
            rows,
            ..Default::default()
        };
        self.snapshot.process_table = Arc::new(table);
    }

    fn domain(&mut self, collector: &str) -> &mut DomainSnapshot {
        let id = collector_id(collector).unwrap();
        Arc::make_mut(self.snapshot.domains.entry(id).or_default())
    }
}
