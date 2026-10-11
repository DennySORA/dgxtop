//! The Manager and the history's process actor (D-79): what it hands the
//! actor, and what it waits for.

use dgxtop_core::ports::ProcessHistoryInput;
use dgxtop_core::process_model::{ProcessHistoryModel, RunRequest, WriteProgress};
use dgxtop_core::testing::{gpu_process, process_table};
use dgxtop_core::tiers::BarSize;

use super::*;

fn harness() -> Harness {
    ready_harness(HarnessOptions {
        process_history: true,
        ..HarnessOptions::default()
    })
}

/// Every commit hands over the snapshot's table: the first goes to the
/// actor with what its sample takes, the commits after it only tick.
#[test]
fn a_committed_table_goes_to_the_process_actor_once() {
    let mut harness = harness();
    let plans = jobs_of_every_collector(&mut harness);
    harness.ports.snapshots.update(|snapshot| {
        snapshot.process_table = Arc::new(process_table(
            5,
            7_000,
            vec![gpu_process(9, "train", &[("gpu:GPU-aaaa", Some(1 << 30))])],
        ));
    });
    harness.ports.process_inputs.take();
    for (revision, plan) in (2..).zip(&plans) {
        commit(&mut harness, plan, revision);
    }
    let inputs = harness.ports.process_inputs.take();
    let tables: Vec<_> = inputs
        .iter()
        .filter_map(|input| match input {
            ProcessHistoryInput::Table(sample) => Some(sample),
            _ => None,
        })
        .collect();
    assert_eq!(tables.len(), 1, "{inputs:?}");
    assert_eq!(tables[0].table().cycle_id, 5);
    assert_eq!(tables[0].table().rows.len(), 1);
    assert!(tables[0].process_period_ms > 0);
    // Handed over as committed, with the catalog of the moment (D-99).
    let committed = dgxtop_core::ports::SnapshotReader::latest(harness.ports.snapshots.as_ref());
    assert!(Arc::ptr_eq(
        &tables[0].snapshot.process_table,
        &committed.process_table
    ));
    assert_eq!(
        *tables[0].catalog,
        *harness.manager.core().catalog.snapshot()
    );
    let ticks = inputs
        .iter()
        .filter(|input| matches!(input, ProcessHistoryInput::Tick))
        .count();
    assert!(ticks >= plans.len() - 1, "{inputs:?}");
}

/// The run the History view opened is asked of the actor with the
/// session's frame, at its bar size and plot width.
#[test]
fn the_opened_run_is_asked_of_the_process_actor() {
    let mut harness = harness();
    let opened = target(9);
    harness
        .submit(navigation(ViewKind::History, Some(opened.clone()), ""))
        .unwrap();
    harness.ports.clock.advance_ms(100);
    harness.settle();
    let shown: Vec<RunRequest> = harness
        .ports
        .process_inputs
        .take()
        .into_iter()
        .filter_map(|input| match input {
            ProcessHistoryInput::Show(request) => Some(request),
            _ => None,
        })
        .collect();
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!((&shown[0].key, shown[0].bar), (&opened, BarSize::Auto));
    assert!(shown[0].columns > 0);
}

/// The shutdown sends the flush and waits until the actor says it was
/// handled and its writes answered.
#[test]
fn the_shutdown_waits_for_the_process_history_flush() {
    let mut harness = harness();
    harness.manager.shutdown_trigger().trigger("test");
    harness.step();
    assert!(
        harness
            .ports
            .process_inputs
            .take()
            .iter()
            .any(|input| matches!(input, ProcessHistoryInput::Flush))
    );
    assert!(harness.manager.summary().is_none(), "the flush is pending");
    harness.ports.process_history.publish(ProcessHistoryModel {
        revision: 1,
        writes: WriteProgress {
            flushes: 1,
            pending: 0,
        },
        ..ProcessHistoryModel::default()
    });
    harness.step();
    let summary = harness.manager.summary().expect("finished");
    assert!(!summary.timed_out);
}
