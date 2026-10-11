use std::collections::BTreeMap;
use std::sync::Arc;

use dgxtop_core::action::Signal;
use dgxtop_core::canonical::{ProcessRow, ProcessTable};
use dgxtop_core::identity::EntityId;
use dgxtop_core::ports::{ProcessHistoryCompletion, ProcessHistoryReader, ProcessMemoryReader};
use dgxtop_core::process_history::{BarSize, EndCause, ProcessHistoryLoad, RunEnd, RunMetric};
use dgxtop_core::process_model::RunRequest;
use dgxtop_core::process_probe::{ProbeLiveness, ProbeReading, ProbeReport};
use dgxtop_core::testing::{
    CapturingDispatch, FakeClock, IdleProbeDeriver, MemoryLogger, boot_id, gpu_process,
    process_table, test_process_key,
};

use super::*;

const SECOND: u64 = 1_000_000_000;
const GIB: u64 = 1 << 30;
const GPU: &str = "gpu:GPU-aaaa";

struct Rig {
    actor: ProcessHistory,
    published: PublishedProcessHistory,
    clock: Arc<FakeClock>,
    probe: CapturingDispatch<ProcessProbeJob>,
    store: CapturingDispatch<ProcessHistoryJob>,
}

fn rig() -> Rig {
    let clock = FakeClock::new(0);
    let probe = CapturingDispatch::new();
    let store = CapturingDispatch::new();
    let (actor, published) = ProcessHistory::new(
        ProcessHistorySettings {
            boot_id: boot_id("boot-test"),
            utc_offset_s: 0,
        },
        ProcessHistoryPorts {
            probe: Some(Box::new(probe.clone())),
            probe_deriver: Arc::new(IdleProbeDeriver),
            store: Some(Box::new(store.clone())),
            clock: Arc::clone(&clock) as Arc<dyn TimeProvider>,
            logger: Arc::new(MemoryLogger::default()),
        },
    );
    Rig {
        actor,
        published,
        clock,
        probe,
        store,
    }
}

fn row(pid: i64, gpu_bytes: u64) -> ProcessRow {
    let mut row = gpu_process(pid, "acme_llm.train", &[(GPU, Some(gpu_bytes))]);
    row.cpu_percent = Some(50.0);
    row
}

fn table_at(second: u64, rows: Vec<ProcessRow>) -> ProcessTable {
    process_table(second, second * SECOND, rows)
}

/// The clock moves to `second` and the table read then is handed over.
fn table(rig: &mut Rig, second: u64, rows: Vec<ProcessRow>) {
    let ahead = (second * SECOND).saturating_sub(rig.clock.now_boot_ns());
    rig.clock.advance_ns(ahead);
    rig.actor
        .handle(ProcessHistoryInput::Table(Box::new(ProcessTableSample {
            table: Arc::new(table_at(second, rows)),
            clk_tck: Some(100),
            gpu_labels: BTreeMap::from([(EntityId::new(GPU).unwrap(), "GPU 0".to_owned())]),
            host_network: [None, None],
            process_period_ms: 1_000,
        })));
}

fn loaded(rig: &mut Rig, load: ProcessHistoryLoad) {
    rig.actor.handle(ProcessHistoryInput::Store(
        ProcessHistoryCompletion::Loaded(Ok(load)),
    ));
}

/// The first input asks the stores for what they kept; the tables become
/// runs with their figures and the processes holding memory get colors.
#[test]
fn tables_become_runs_and_colors_in_the_published_model() {
    let mut rig = rig();
    assert_eq!(rig.published.latest().revision, 1, "an empty model at once");
    table(&mut rig, 1, vec![row(9, 4 * GIB)]);
    assert!(matches!(rig.store.take()[..], [ProcessHistoryJob::Load]));
    for second in 2..10 {
        table(&mut rig, second, vec![row(9, 4 * GIB)]);
    }
    let model = rig.published.latest();
    assert_eq!(model.runs.len(), 1);
    let run = &model.runs[0];
    assert!(run.live);
    assert_eq!(run.gpus, vec!["GPU 0".to_owned()]);
    assert_eq!(run.whole.value(RunMetric::Cpu).count, 9);
    assert_eq!(run.whole.value(RunMetric::Cpu).mean(), Some(50.0));
    assert!(model.kept_on_disk);
    assert_eq!(model.tracked.len(), 1);
    assert_eq!(
        model.slot_of(&test_process_key(9)),
        Some(model.tracked[0].slot)
    );
    assert_eq!(model.tracked[0].current.len(), 1);
}

/// A tick changes nothing shown, so nothing is published again.
#[test]
fn an_input_that_changes_nothing_publishes_nothing() {
    let mut rig = rig();
    table(&mut rig, 1, vec![row(9, GIB)]);
    let revision = rig.published.latest().revision;
    rig.probe.take();
    rig.actor.handle(ProcessHistoryInput::Tick);
    assert_eq!(rig.published.latest().revision, revision);
}

/// One probe at a time, here too: a tick lets the next one leave once the
/// report came back.
#[test]
fn probes_follow_their_reports() {
    let mut rig = rig();
    table(&mut rig, 1, vec![row(9, GIB)]);
    assert_eq!(rig.probe.take().len(), 1);
    rig.actor.handle(ProcessHistoryInput::Tick);
    assert!(rig.probe.is_empty(), "the first is in flight");
    rig.actor
        .handle(ProcessHistoryInput::Probe(ProbeReport::default()));
    rig.actor.handle(ProcessHistoryInput::Tick);
    assert_eq!(rig.probe.take().len(), 1);
}

/// dgxtop's signal explains the end the probe finds.
#[test]
fn a_noted_signal_explains_the_end() {
    let mut rig = rig();
    table(&mut rig, 1, vec![row(9, GIB)]);
    rig.actor.handle(ProcessHistoryInput::Signal {
        key: test_process_key(9),
        signal: Signal::Sigterm,
    });
    rig.actor.handle(ProcessHistoryInput::Probe(ProbeReport {
        readings: vec![ProbeReading {
            liveness: ProbeLiveness::Gone,
            ..ProbeReading::gone(test_process_key(9))
        }],
        oom_kills: None,
        read_boot_ns: 2 * SECOND,
    }));
    let model = rig.published.latest();
    assert_eq!(
        model.runs[0].end.map(|end| end.cause),
        Some(EndCause::SignaledByDgxtop(Signal::Sigterm))
    );
}

/// A shown run gets its detail with bars of the size asked for; a run
/// whose buckets are not held is read from the store once and loading
/// until they come.
#[test]
fn shown_runs_get_their_details_and_ended_ones_are_read_once() {
    let mut rig = rig();
    for second in 1..40 {
        table(&mut rig, second, vec![row(9, 4 * GIB)]);
    }
    let asked = RunRequest {
        key: test_process_key(9),
        bar: BarSize::OneMinute,
        columns: 30,
    };
    rig.actor.handle(ProcessHistoryInput::Show(asked.clone()));
    let model = rig.published.latest();
    let detail = model.detail_for(&asked).unwrap();
    assert!(model.answers(&asked));
    let bars = detail.bars.as_ref().unwrap();
    assert_eq!(bars.bar, BarSize::OneMinute);
    assert_eq!(bars.window.1 - bars.window.0, 30 * 60 * 1_000_000_000);
    assert!(!detail.loading);
    // An ended run of an earlier start, restored without its buckets.
    let mut info = detail.info.clone();
    info.key = test_process_key(5);
    info.end = Some(RunEnd {
        utc_ns: info.last_seen_utc_ns,
        cause: EndCause::Exit(0),
    });
    loaded(
        &mut rig,
        ProcessHistoryLoad {
            runs: vec![info],
            ..ProcessHistoryLoad::default()
        },
    );
    rig.store.take();
    let ended = RunRequest {
        key: test_process_key(5),
        ..asked
    };
    rig.actor.handle(ProcessHistoryInput::Show(ended.clone()));
    rig.actor.handle(ProcessHistoryInput::Show(ended.clone()));
    let reads = rig.store.take();
    assert_eq!(reads.len(), 1, "read once: {reads:?}");
    assert!(matches!(reads[0], ProcessHistoryJob::ReadRun(_)));
    assert!(rig.published.latest().detail_for(&ended).unwrap().loading);
}

/// The flush sends the journal and the short-term file; the model says
/// when both were answered.
#[test]
fn a_flush_is_settled_once_its_writes_are_answered() {
    let mut rig = rig();
    table(&mut rig, 1, vec![row(9, GIB)]);
    loaded(&mut rig, ProcessHistoryLoad::default());
    rig.store.take();
    rig.actor.handle(ProcessHistoryInput::Flush);
    let jobs = rig.store.take();
    assert!(matches!(
        jobs[..],
        [
            ProcessHistoryJob::Append(_),
            ProcessHistoryJob::SaveRecent(_)
        ]
    ));
    let writes = rig.published.latest().writes;
    assert_eq!((writes.flushes, writes.pending), (1, 2));
    assert!(!writes.settled_after(1));
    for _ in 0..2 {
        rig.actor
            .handle(ProcessHistoryInput::Store(ProcessHistoryCompletion::Saved(
                Ok(()),
            )));
    }
    assert!(rig.published.latest().writes.settled_after(1));
}

/// The memory record answers a chart's parts for the exact window asked.
#[test]
fn the_memory_record_answers_the_parts_of_a_window() {
    let mut rig = rig();
    for second in [10, 15, 20] {
        table(&mut rig, second, vec![row(9, 2 * GIB)]);
    }
    let gpus = std::collections::BTreeSet::from([EntityId::new(GPU).unwrap()]);
    let parts = rig.published.parts(&gpus, (10 * SECOND, 20 * SECOND), 2);
    assert_eq!(parts.len(), 1);
    #[allow(clippy::cast_precision_loss)]
    let held = (2 * GIB) as f64;
    assert_eq!(parts[0].means, vec![Some(held), Some(held)]);
    let other = std::collections::BTreeSet::from([EntityId::new("gpu:GPU-bbbb").unwrap()]);
    assert!(
        rig.published
            .parts(&other, (10 * SECOND, 20 * SECOND), 2)
            .is_empty()
    );
}
