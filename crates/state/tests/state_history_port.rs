//! The live state's side of the `HistoryIngest` port (D-79), against a
//! recording test double: what each commit, gap and maintenance step hands
//! to the history, and how the outcome reaches the published snapshot.

// Test helpers outside #[test] functions may unwrap: a broken fixture must fail loudly.
#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex, PoisonError};

use dgxtop_core::budget::ByteLedger;
use dgxtop_core::canonical::{
    AckStatus, CanonicalValue, CollectionBatch, CollectorState, CommitGuard, GapRecord, StoreAck,
};
use dgxtop_core::config::Config;
use dgxtop_core::identity::{CollectorId, MetricKey};
use dgxtop_core::metric::{CanonicalUnit, Scalar, ids};
use dgxtop_core::ports::{
    ArchiveJournal, HistoryAppends, HistoryIngest, HistoryOutcome, SnapshotReader, StoreWriter,
    TrendReader,
};
use dgxtop_core::quality::{Freshness, Namespace, Quality, SampleKind, SelectionState};
use dgxtop_core::testing::{UTC_ON_A_WHOLE_MINUTE_NS, metric_key, source_id};
use dgxtop_state::{LiveState, SharedSnapshot, StateWriter};
use uuid::Uuid;

const SECOND: u64 = 1_000_000_000;

/// What the state handed to the history, in call order.
#[derive(Debug, Clone, PartialEq)]
enum Call {
    Ingest {
        gauges: usize,
        revision: u64,
        now: u64,
    },
    Gap(String),
    Maintain(u64),
    Revision(u64),
    FinalizeGrace(u64),
    WallClock(u64, i128),
    TakeJournal,
}

#[derive(Default)]
struct Script {
    calls: Vec<Call>,
    /// Returned by every mutation.
    outcome: HistoryOutcome,
}

/// Records every call and answers with the scripted outcome.
#[derive(Clone, Default)]
struct RecordingHistory {
    script: Arc<Mutex<Script>>,
}

impl RecordingHistory {
    fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    fn answer_with(&self, outcome: HistoryOutcome) {
        self.lock().outcome = outcome;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn record(&self, call: Call) -> HistoryOutcome {
        let mut script = self.lock();
        script.calls.push(call);
        script.outcome.clone()
    }
}

impl HistoryIngest for RecordingHistory {
    fn ingest(&mut self, appends: HistoryAppends, now: u64, revision: u64) -> HistoryOutcome {
        self.record(Call::Ingest {
            gauges: appends.gauges.len(),
            revision,
            now,
        })
    }

    fn record_gap(&mut self, gap: GapRecord) -> HistoryOutcome {
        self.record(Call::Gap(gap.reason))
    }

    fn maintain(&mut self, now: u64) -> HistoryOutcome {
        self.record(Call::Maintain(now))
    }

    fn set_revision(&mut self, revision: u64) {
        self.record(Call::Revision(revision));
    }

    fn set_finalize_grace(&mut self, grace_ns: u64) {
        self.record(Call::FinalizeGrace(grace_ns));
    }

    fn observe_wall_clock(&mut self, boot_ns: u64, utc_ns: i128) {
        self.record(Call::WallClock(boot_ns, utc_ns));
    }

    fn take_archive_journal(&mut self, _max_bytes: u64) -> Option<ArchiveJournal> {
        self.record(Call::TakeJournal);
        Some(ArchiveJournal {
            revision: 0,
            host_id: String::new(),
            boot_id: String::new(),
            segments: Vec::new(),
            buckets: Vec::new(),
            bridges: Vec::new(),
            gaps: Vec::new(),
            prunes: Vec::new(),
            expire: None,
            when_full: dgxtop_core::ports::ArchiveWhenFull::EvictOldest,
            permit: None,
        })
    }
}

struct Rig {
    writer: StateWriter,
    snapshots: SharedSnapshot,
    history: RecordingHistory,
    collector: CollectorId,
}

fn rig(archive_enabled: bool) -> Rig {
    let config = Config::defaults().unwrap();
    let history = RecordingHistory::default();
    let (writer, snapshots) = LiveState::new(
        &config.store,
        &config.sampling,
        ByteLedger::for_memory_limit_mib(64),
        Namespace::Live,
        archive_enabled,
        Box::new(history.clone()),
    )
    .into_parts();
    Rig {
        writer,
        snapshots,
        history,
        collector: CollectorId::new("gpu").unwrap(),
    }
}

fn temperature_key() -> MetricKey {
    metric_key("gpu:0", ids::GPU_TEMPERATURE, "gpu")
}

fn temperature(at: u64) -> CanonicalValue {
    CanonicalValue {
        key: temperature_key(),
        cycle_id: 1,
        source_id: Some(source_id("nvml:0:temp")),
        source_generation: Some(1),
        unit: CanonicalUnit::DegC,
        value: Some(Scalar::from_f64(50.0).unwrap()),
        sample_kind: SampleKind::Measured,
        selection_state: SelectionState::Selected,
        quality: Quality::Verified,
        freshness: Freshness::Fresh,
        measurement_freshness: Freshness::Unknown,
        observed_boot_ns: None,
        read_finished_boot_ns: at,
        policy_revision: 1,
        candidate_source_ids: Vec::new(),
        reason_codes: Vec::new(),
        eligible_for_history: true,
    }
}

impl Rig {
    fn commit(&mut self, epoch: u64, at: u64) -> StoreAck {
        let base = self.snapshots.latest().collector_state(&self.collector).0;
        let batch = CollectionBatch {
            batch_id: Uuid::new_v4(),
            collector_id: self.collector.clone(),
            cycle_id: 1,
            epoch,
            config_revision: 1,
            catalog_revision: 1,
            policy_revision: 1,
            state_base_revision: base,
            canonical: vec![temperature(at)],
            explanations: Vec::new(),
            counter_intervals: Vec::new(),
            inputs: Vec::new(),
            process_table: None,
            next_state: CollectorState::default(),
            failures: Vec::new(),
        };
        let guard = CommitGuard {
            expected_epoch: 1,
            collector_id: self.collector.clone(),
            state_base_revision: base,
        };
        self.writer.commit_collection(batch, &guard, at + SECOND)
    }
}

fn gap(reason: &str) -> GapRecord {
    GapRecord {
        key: Some(temperature_key()),
        adapter_id: None,
        start_boot_ns: SECOND,
        end_boot_ns: 2 * SECOND,
        reason: reason.to_owned(),
        affected_count: 1,
    }
}

#[test]
fn a_commit_reaches_the_history_before_its_revision_is_published() {
    let mut rig = rig(false);
    let ack = rig.commit(1, SECOND);
    assert_eq!(ack.status, AckStatus::Committed);
    assert_eq!(ack.committed_revision, 1);
    assert_eq!(
        rig.history.calls(),
        vec![
            Call::Ingest {
                gauges: 1,
                revision: 1,
                now: 2 * SECOND,
            },
            Call::Revision(1),
        ]
    );
}

#[test]
fn a_rejected_commit_never_reaches_the_history() {
    let mut rig = rig(false);
    let ack = rig.commit(0, SECOND);
    assert_ne!(ack.status, AckStatus::Committed);
    assert!(rig.history.calls().is_empty());
}

#[test]
fn the_history_outcome_shapes_counters_health_and_recent_gaps() {
    let mut rig = rig(false);
    rig.history.answer_with(HistoryOutcome {
        values_dropped: 2,
        intervals_dropped: 1,
        series_rejected: 1,
        evicted_records: 3,
        new_gaps: vec![gap("history_capacity")],
        gaps_added: 1,
        series_count: 7,
        truncated: true,
        archive_rows_dropped: 4,
    });
    rig.commit(1, SECOND);
    let snapshot = rig.snapshots.latest();
    assert_eq!(snapshot.health.series, 7);
    assert!(snapshot.health.truncated_history);
    assert_eq!(snapshot.health.gaps_total, 1);
    assert_eq!(snapshot.health.evicted_total, 3);
    assert_eq!(snapshot.recent_gaps.len(), 1);
    let counters = rig.snapshots.counters();
    assert_eq!(counters.history_values_dropped_total, 2);
    assert_eq!(counters.history_intervals_dropped_total, 1);
    assert_eq!(counters.series_rejected_total, 1);
    assert_eq!(counters.archive_rows_dropped_total, 4);
}

#[test]
fn a_gap_the_history_could_not_store_is_still_counted_and_listed() {
    let mut rig = rig(false);
    rig.writer.record_gap(gap("adapter_timeout"));
    assert_eq!(
        rig.history.calls()[0],
        Call::Gap("adapter_timeout".to_owned())
    );
    let snapshot = rig.snapshots.latest();
    assert_eq!(snapshot.health.gaps_total, 1);
    assert_eq!(snapshot.recent_gaps[0].reason, "adapter_timeout");
}

#[test]
fn an_empty_gap_is_refused_before_the_history() {
    let mut rig = rig(false);
    let mut empty = gap("adapter_timeout");
    empty.end_boot_ns = empty.start_boot_ns;
    rig.writer.record_gap(empty);
    assert!(rig.history.calls().is_empty());
    assert_eq!(rig.snapshots.counters().invalid_gaps_total, 1);
}

#[test]
fn maintenance_sampling_and_clock_are_forwarded() {
    let config = Config::defaults().unwrap();
    let mut rig = rig(false);
    rig.writer.maintain(10 * SECOND);
    rig.writer.set_sampling(&config.sampling);
    rig.writer
        .observe_wall_clock(30 * SECOND, UTC_ON_A_WHOLE_MINUTE_NS);
    let calls = rig.history.calls();
    assert_eq!(calls[0], Call::Maintain(10 * SECOND));
    let grace =
        dgxtop_core::sampling::SamplingTiming::from_sampling(&config.sampling).finalize_grace_ns;
    assert!(calls.contains(&Call::FinalizeGrace(grace)));
    assert_eq!(
        calls.last(),
        Some(&Call::WallClock(30 * SECOND, UTC_ON_A_WHOLE_MINUTE_NS))
    );
}

#[test]
fn journals_are_drained_only_when_the_archive_is_on() {
    let mut off = rig(false);
    assert!(off.writer.take_archive_journal(1 << 20).is_none());
    assert!(off.history.calls().is_empty());
    let mut on = rig(true);
    assert!(on.writer.take_archive_journal(1 << 20).is_some());
    assert_eq!(on.history.calls(), vec![Call::TakeJournal]);
}

/// The short trends are filled at commit (D-99): a committed read is a
/// point once its revision is published, a rejected commit, a gap or a
/// maintenance step adds none, and a sampling change restarts them.
#[test]
fn a_committed_read_is_a_trend_point() {
    let mut rig = rig(false);
    let trends = rig.writer.trends();
    let key = temperature_key();
    assert!(trends.trend(&key, 10).points.is_empty());
    rig.commit(1, SECOND);
    assert_eq!(trends.trend(&key, 10).points, vec![Some(50.0)]);
    rig.commit(0, 2 * SECOND);
    rig.writer.record_gap(gap("host_unavailable"));
    rig.writer.maintain(3 * SECOND);
    assert_eq!(trends.trend(&key, 10).points, vec![Some(50.0)]);
    rig.commit(1, 2 * SECOND);
    assert_eq!(trends.trend(&key, 10).points, vec![Some(50.0); 2]);
    let mut sampling = Config::defaults().unwrap().sampling;
    sampling.interval_ms *= 2;
    rig.writer.set_sampling(&sampling);
    assert!(trends.trend(&key, 10).points.is_empty());
}
