use dgxtop_core::canonical::{ProcessRow, ProcessTable};
use dgxtop_core::identity::{EntityId, ProcessKey};
use dgxtop_core::testing::{gpu_process, process_table, test_process_key};

use super::tracking::preferred_color;
use super::*;

const SECOND: u64 = NANOS_PER_SECOND;
const GIB: u64 = 1 << 30;
const GPU_A: &str = "gpu:GPU-aaaa";
const GPU_B: &str = "gpu:GPU-bbbb";

fn key(pid: i64) -> ProcessKey {
    test_process_key(pid)
}

fn gpu(id: &str) -> EntityId {
    EntityId::new(id).unwrap()
}

fn row(pid: i64, name: &str, held: &[(&str, Option<u64>)]) -> ProcessRow {
    gpu_process(pid, name, held)
}

fn table(cycle: u64, at_s: u64, rows: Vec<ProcessRow>) -> ProcessTable {
    process_table(cycle, at_s * SECOND, rows)
}

fn on_a(id: &EntityId) -> bool {
    id.as_str() == GPU_A
}

#[allow(clippy::cast_precision_loss)]
fn gib(value: f64) -> f64 {
    value * GIB as f64
}

fn close(left: Option<f64>, right: f64) -> bool {
    left.is_some_and(|left| (left - right).abs() < 1.0)
}

/// A table's values hold until the next table, but never beyond three
/// sampling periods: a stalled read leaves a gap, not a stretched value.
#[test]
fn a_value_holds_until_the_next_table_and_at_most_three_periods() {
    let mut book = ProcessMemoryBook::new(1000);
    book.record(&table(
        1,
        1,
        vec![row(9, "python", &[(GPU_A, Some(4 * GIB))])],
    ));
    book.record(&table(
        2,
        2,
        vec![row(9, "python", &[(GPU_A, Some(4 * GIB))])],
    ));
    book.record(&table(
        3,
        10,
        vec![row(9, "python", &[(GPU_A, Some(4 * GIB))])],
    ));
    let held = book.held_on(&key(9), &gpu(GPU_A), (0, 20 * SECOND));
    assert_eq!(held.covered_ns, 4 * SECOND, "1 s, then 3 s of the 8 s gap");
    assert!(close(held.mean(), gib(4.0)));
    assert_eq!(held.peak, Some(4 * GIB));
    // The same table handed again by another domain's commit adds nothing.
    book.record(&table(
        3,
        10,
        vec![row(9, "python", &[(GPU_A, Some(4 * GIB))])],
    ));
    book.record(&table(
        4,
        11,
        vec![row(9, "python", &[(GPU_A, Some(4 * GIB))])],
    ));
    let held = book.held_on(&key(9), &gpu(GPU_A), (0, 20 * SECOND));
    assert_eq!(held.covered_ns, 5 * SECOND);
    // A table without a read time is not one.
    book.record(&ProcessTable::default());
    assert_eq!(book.tracked().len(), 1);
}

/// A part is a process's share of the time lists were recorded: while it
/// is not listed it counts as 0, so the parts of all processes add up to
/// what they held together; cells without a list are unknown.
#[test]
fn parts_count_a_time_not_listed_as_zero_and_add_up() {
    // Tables every 5 s, on the edges of the 5 s buckets.
    let mut book = ProcessMemoryBook::new(5000);
    let both = || {
        vec![
            row(1, "train", &[(GPU_A, Some(6 * GIB))]),
            row(2, "serve", &[(GPU_A, Some(2 * GIB))]),
        ]
    };
    book.record(&table(1, 10, both()));
    book.record(&table(2, 15, both()));
    // 1 exits at 20 s; 2 grows.
    book.record(&table(
        3,
        20,
        vec![row(2, "serve", &[(GPU_A, Some(4 * GIB))])],
    ));
    book.record(&table(
        4,
        25,
        vec![row(2, "serve", &[(GPU_A, Some(4 * GIB))])],
    ));
    // Cells of 5 s from 5 s: nothing recorded yet, then each table's span.
    let window = (5 * SECOND, 25 * SECOND);
    let train = book.part_means(&key(1), on_a, window, 4);
    let serve = book.part_means(&key(2), on_a, window, 4);
    assert_eq!((train[0], serve[0]), (None, None));
    assert!(close(train[1], gib(6.0)) && close(train[2], gib(6.0)));
    assert!(close(train[3], 0.0), "not listed counts as 0");
    assert!(close(serve[1], gib(2.0)) && close(serve[3], gib(4.0)));
    // Still tracked after it exited, but holding nothing now; each has the
    // color its name hashes to, and they come in color order.
    let mut expected = vec![
        (preferred_color("train"), "train", false),
        (preferred_color("serve"), "serve", true),
    ];
    expected.sort_unstable();
    let tracked = book.tracked();
    assert_eq!(
        tracked
            .iter()
            .map(|process| (process.slot, process.name, process.is_listed()))
            .collect::<Vec<_>>(),
        expected
    );
    let serve = tracked
        .iter()
        .find(|process| process.name == "serve")
        .unwrap();
    assert_eq!(serve.bytes_on(on_a), 4 * GIB);
    assert_eq!(book.slot_of(&key(2)), Some(preferred_color("serve")));
    // Over the window: mean over the time each was listed.
    let held = book.held_on(&key(1), &gpu(GPU_A), window);
    assert_eq!(held.covered_ns, 10 * SECOND);
    assert!(close(held.mean(), gib(6.0)));
    let held = book.held_on(&key(2), &gpu(GPU_A), window);
    assert_eq!((held.covered_ns, held.peak), (15 * SECOND, Some(4 * GIB)));
    assert!(close(held.mean(), gib(8.0 / 3.0)));
}

/// A process on two GPUs: each GPU keeps its own record, a part sums the
/// GPUs a chart covers, and a GPU without a known figure adds nothing.
#[test]
fn every_gpu_of_a_process_is_recorded_on_its_own() {
    let mut book = ProcessMemoryBook::new(1000);
    let both = || {
        vec![row(
            5,
            "ddp",
            &[
                (GPU_A, Some(3 * GIB)),
                (GPU_B, Some(GIB)),
                ("gpu:GPU-cccc", None),
            ],
        )]
    };
    book.record(&table(1, 1, both()));
    book.record(&table(2, 2, both()));
    assert_eq!(
        book.gpus_of(&key(5))
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>(),
        vec![GPU_A, GPU_B]
    );
    let window = (SECOND, 2 * SECOND);
    assert!(close(
        book.part_means(&key(5), on_a, window, 1)[0],
        gib(3.0)
    ));
    assert!(close(
        book.part_means(&key(5), |_| true, window, 1)[0],
        gib(4.0)
    ));
    assert_eq!(book.tracked()[0].bytes_on(|_| true), 4 * GIB);
    // An untracked process has no part.
    assert_eq!(
        book.part_means(&key(77), |_| true, window, 2),
        vec![None, None]
    );
}

/// Only tracked processes are recorded; one that loses its place takes its
/// record along, and a newcomer starts its own.
#[test]
fn a_process_that_loses_its_place_takes_its_record_along() {
    let mut book = ProcessMemoryBook::new(1000);
    let small = |pid| row(pid, "small", &[(GPU_A, Some(GIB))]);
    book.record(&table(1, 1, (1..=9).map(small).collect()));
    assert_eq!(book.tracked().len(), 8);
    assert_eq!(book.slot_of(&key(9)), None, "eight colors, one left over");
    book.record(&table(2, 2, (1..=9).map(small).collect()));
    // 9 grows past twice the smallest holders: it takes the place of one.
    let mut rows: Vec<ProcessRow> = (1..=8).map(small).collect();
    rows.push(row(9, "big", &[(GPU_A, Some(8 * GIB))]));
    book.record(&table(3, 3, rows.clone()));
    book.record(&table(4, 4, rows));
    assert!(book.slot_of(&key(9)).is_some());
    let gone: Vec<i64> = (1..=8)
        .filter(|pid| book.slot_of(&key(*pid)).is_none())
        .collect();
    assert_eq!(gone.len(), 1);
    let held = book.held_on(&key(gone[0]), &gpu(GPU_A), (0, 10 * SECOND));
    assert_eq!(held.covered_ns, 0, "its record went with its place");
    let held = book.held_on(&key(9), &gpu(GPU_A), (0, 10 * SECOND));
    assert_eq!(
        held.covered_ns, SECOND,
        "recorded from the table it got a color"
    );
}

/// An exited process keeps its record, and with it its history, while
/// new processes come (D-65): until the record is full.
#[test]
fn an_exited_process_keeps_its_history_while_new_processes_come() {
    let mut book = ProcessMemoryBook::new(1000);
    book.record(&table(
        1,
        1,
        vec![row(1, "first-job", &[(GPU_A, Some(6 * GIB))])],
    ));
    book.record(&table(
        2,
        2,
        vec![row(1, "first-job", &[(GPU_A, Some(6 * GIB))])],
    ));
    // Eight other jobs come and go after it exits.
    let mut at = 3;
    for pid in 2..=9 {
        let name = format!("job{pid}");
        book.record(&table(at, at, vec![row(pid, &name, &[(GPU_A, Some(GIB))])]));
        book.record(&table(at + 1, at + 1, Vec::new()));
        at += 2;
    }
    // Listed in the tables at 1 s and 2 s, each holding until the next.
    let held = book.held_on(&key(1), &gpu(GPU_A), (0, 100 * SECOND));
    assert_eq!(held.covered_ns, 2 * SECOND, "still recorded");
    assert!(close(held.mean(), gib(6.0)));
}

/// A process not listed for a day gives its slot and record up.
#[test]
fn a_day_without_a_list_entry_ends_tracking() {
    let mut book = ProcessMemoryBook::new(1000);
    book.record(&table(1, 1, vec![row(3, "old", &[(GPU_A, Some(GIB))])]));
    book.record(&table(2, 2, Vec::new()));
    assert_eq!(book.slot_of(&key(3)), Some(preferred_color("old")));
    book.record(&table(3, 2 + 86_401, Vec::new()));
    assert_eq!(book.slot_of(&key(3)), None);
    assert!(book.tracked().is_empty());
}

/// A longer interval lets a value hold longer.
#[test]
fn the_hold_follows_the_sampling_interval() {
    let mut book = ProcessMemoryBook::new(1000);
    book.set_interval(5000);
    book.record(&table(1, 1, vec![row(9, "python", &[(GPU_A, Some(GIB))])]));
    book.record(&table(2, 11, vec![row(9, "python", &[(GPU_A, Some(GIB))])]));
    let held = book.held_on(&key(9), &gpu(GPU_A), (0, 20 * SECOND));
    assert_eq!(held.covered_ns, 10 * SECOND);
}

/// A chart's parts (moved with the record from the Manager's presentation,
/// D-79): tables every 10 s from 10 s to 30 s, `train` holding 6 GiB of
/// GPU A, `serve` 2 GiB of it and 1 GiB of GPU B. Each GPU's chart gets the
/// parts on it in color order, a set of GPUs the parts on all of them, and
/// a window before any list none.
#[test]
fn parts_follow_the_gpus_asked_for_in_color_order() {
    let mut book = ProcessMemoryBook::new(10_000);
    for at in [10, 20, 30] {
        book.record(&table(
            at,
            at,
            vec![
                row(1, "train", &[(GPU_A, Some(6 * GIB))]),
                row(2, "serve", &[(GPU_A, Some(2 * GIB)), (GPU_B, Some(GIB))]),
            ],
        ));
    }
    let window = (10 * SECOND, 30 * SECOND);
    let parts = |gpus: &[&str]| -> Vec<(u8, Vec<Option<f64>>)> {
        let gpus: BTreeSet<EntityId> = gpus.iter().map(|id| gpu(id)).collect();
        book.parts(&gpus, window, 2)
            .into_iter()
            .map(|part| (part.slot, part.means))
            .collect()
    };
    let (train, serve) = (preferred_color("train"), preferred_color("serve"));
    let mut on_a = vec![
        (train, vec![Some(gib(6.0)); 2]),
        (serve, vec![Some(gib(2.0)); 2]),
    ];
    on_a.sort_by_key(|(slot, _)| *slot);
    assert_eq!(parts(&[GPU_A]), on_a);
    assert_eq!(parts(&[GPU_B]), vec![(serve, vec![Some(gib(1.0)); 2])]);
    let mut both = vec![
        (train, vec![Some(gib(6.0)); 2]),
        (serve, vec![Some(gib(3.0)); 2]),
    ];
    both.sort_by_key(|(slot, _)| *slot);
    assert_eq!(parts(&[GPU_A, GPU_B]), both);
    assert!(
        book.parts(&BTreeSet::from([gpu(GPU_A)]), (0, 5 * SECOND), 2)
            .is_empty()
    );
}
