use std::sync::Arc;

use dgxtop_core::action::Signal;
use dgxtop_core::process_model::{ProcessHistoryModel, WriteProgress};
use dgxtop_core::testing::{
    CapturingDispatch, StaticProcessHistory, process_table, test_process_key,
};
use dgxtop_core::tiers::BarSize;

use super::*;

struct Rig {
    link: ProcessHistoryLink,
    inputs: CapturingDispatch<ProcessHistoryInput>,
    history: Arc<StaticProcessHistory>,
}

fn rig() -> Rig {
    let inputs = CapturingDispatch::new();
    let history = StaticProcessHistory::new(ProcessHistoryModel::default());
    let link = ProcessHistoryLink::new(
        Some(ProcessHistoryHandles {
            inputs: Box::new(inputs.clone()),
            runs: Arc::clone(&history) as Arc<dyn ProcessHistoryReader>,
            memory: Arc::clone(&history) as Arc<dyn ProcessMemoryReader>,
        }),
        Diagnostics::new(),
    );
    Rig {
        link,
        inputs,
        history,
    }
}

fn sample(table: ProcessTable) -> ProcessTableSample {
    ProcessTableSample {
        table: Arc::new(table),
        clk_tck: Some(100),
        gpu_labels: BTreeMap::new(),
        host_network: [None, None],
        process_period_ms: 1_000,
    }
}

fn session() -> SessionId {
    SessionId::new("session-test").unwrap()
}

fn request(bar: BarSize) -> RunRequest {
    RunRequest {
        key: test_process_key(9),
        bar,
        columns: 80,
    }
}

/// A table is handed over once; one refused for a full queue goes with
/// the next commit.
#[test]
fn a_table_goes_once_and_a_refused_one_with_the_next_commit() {
    let mut rig = rig();
    let table = process_table(3, 5_000, Vec::new());
    assert!(!rig.link.has_table(&table));
    rig.inputs.set_busy(true);
    rig.link.send_table(sample(table.clone()));
    assert!(!rig.link.has_table(&table), "refused: not handed over");
    rig.inputs.set_busy(false);
    rig.link.send_table(sample(table.clone()));
    assert!(rig.link.has_table(&table));
    assert!(matches!(
        rig.inputs.take()[..],
        [ProcessHistoryInput::Table(_)]
    ));
    assert!(!rig.link.has_table(&process_table(4, 5_000, Vec::new())));
}

/// A run is asked for until the model answers it, at most once a second.
#[test]
fn a_shown_run_is_asked_for_until_answered() {
    let mut rig = rig();
    rig.link.show(&session(), Some(request(BarSize::Auto)), 0);
    rig.link
        .show(&session(), Some(request(BarSize::Auto)), 500_000_000);
    assert_eq!(rig.inputs.take().len(), 1, "once a second");
    rig.link
        .show(&session(), Some(request(BarSize::OneHour)), 600_000_000);
    assert_eq!(rig.inputs.take().len(), 1, "another size at once");
    rig.link
        .show(&session(), Some(request(BarSize::OneHour)), 1_700_000_000);
    assert_eq!(rig.inputs.take().len(), 1, "again a second later");
    rig.link.show(&session(), None, 1_800_000_000);
    rig.link.show(&session(), None, 1_900_000_000);
    assert!(rig.inputs.is_empty(), "nothing shown, nothing asked");
}

/// A flush refused for a full queue is sent again while the shutdown
/// waits, so the last writes still reach the stores.
#[test]
fn a_refused_flush_is_sent_again_while_the_shutdown_waits() {
    let mut rig = rig();
    rig.inputs.set_busy(true);
    rig.link.flush();
    assert!(rig.inputs.take().is_empty());
    assert!(!rig.link.settled(), "a flush is owed");
    rig.inputs.set_busy(false);
    assert!(!rig.link.settled(), "sent now, not answered yet");
    assert!(matches!(
        rig.inputs.take()[..],
        [ProcessHistoryInput::Flush]
    ));
    rig.history.publish(ProcessHistoryModel {
        revision: 2,
        writes: WriteProgress {
            flushes: 1,
            pending: 0,
        },
        ..ProcessHistoryModel::default()
    });
    assert!(rig.link.settled());
    assert!(rig.inputs.take().is_empty(), "once");
}

/// The shutdown waits for the flush to be handled and its writes answered;
/// without a process history nothing is waited for.
#[test]
fn the_shutdown_waits_for_the_flush_it_asked_for() {
    let mut rig = rig();
    assert!(rig.link.settled(), "no flush asked for");
    rig.link.flush();
    assert!(matches!(
        rig.inputs.take()[..],
        [ProcessHistoryInput::Flush]
    ));
    assert!(!rig.link.settled());
    rig.history.publish(ProcessHistoryModel {
        writes: WriteProgress {
            flushes: 1,
            pending: 1,
        },
        ..ProcessHistoryModel::default()
    });
    assert!(!rig.link.settled(), "a write unanswered");
    rig.history.publish(ProcessHistoryModel {
        revision: 2,
        writes: WriteProgress {
            flushes: 1,
            pending: 0,
        },
        ..ProcessHistoryModel::default()
    });
    assert!(rig.link.settled());
    let mut none = ProcessHistoryLink::new(None, Diagnostics::new());
    none.flush();
    assert!(none.settled());
    assert!(none.model().runs.is_empty());
}

#[test]
fn signals_ticks_and_new_models_reach_their_side() {
    let mut rig = rig();
    rig.link.note_signal(&test_process_key(9), Signal::Sigterm);
    rig.link.tick();
    let inputs = rig.inputs.take();
    assert!(matches!(
        inputs[0],
        ProcessHistoryInput::Signal {
            signal: Signal::Sigterm,
            ..
        }
    ));
    assert!(matches!(inputs[1], ProcessHistoryInput::Tick));
    assert!(!rig.link.take_changed(), "the first model is revision 0");
    rig.history.publish(ProcessHistoryModel {
        revision: 1,
        ..ProcessHistoryModel::default()
    });
    assert!(rig.link.take_changed());
    assert!(!rig.link.take_changed(), "once");
}
