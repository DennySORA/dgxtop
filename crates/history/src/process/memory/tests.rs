use dgxtop_core::canonical::{ProcessRow, ProcessTable};
use dgxtop_core::identity::{EntityId, ProcessKey};
use dgxtop_core::testing::{gpu_process, process_table, test_process_key};
use dgxtop_core::tiers::Tier;

use super::buckets::Held;
use super::tracking::preferred_color;
use super::*;

/// What the tests read of a tracked process.
impl TrackedProcess<'_> {
    /// Bytes it holds now on the GPUs `include` accepts.
    pub(crate) fn bytes_on(&self, include: impl Fn(&EntityId) -> bool) -> u64 {
        self.current
            .iter()
            .filter(|(gpu, _)| include(gpu))
            .fold(0, |sum, (_, bytes)| sum.saturating_add(*bytes))
    }
}

/// What the tests read of the record.
impl ProcessMemoryBook {
    /// Records `table` with the wall clock on the boot clock, so the
    /// tests' times are both.
    pub(crate) fn record_table(&mut self, table: &ProcessTable) {
        self.record(table, 0);
    }

    /// `key`'s part of a chart of the GPUs `include` accepts in `cells`
    /// even cells of `window` (wall clock).
    pub(crate) fn part_means(
        &self,
        key: &ProcessKey,
        include: impl Fn(&EntityId) -> bool,
        window: (u64, u64),
        cells: usize,
    ) -> Vec<Option<f64>> {
        let window = utc_window(window);
        let listing = cells_between(&self.listing, window, cells);
        self.means_over(key, include, window, &listing)
    }

    /// The color of `key` while it is tracked.
    pub(crate) fn slot_of(&self, key: &ProcessKey) -> Option<u8> {
        self.tracking.color_of(key)
    }

    /// The GPUs `key` held memory on in its record.
    pub(crate) fn gpus_of(&self, key: &ProcessKey) -> Vec<&EntityId> {
        self.processes
            .get(key)
            .map(|record| record.gpus.keys().collect())
            .unwrap_or_default()
    }

    /// What `key` held on `gpu` over `window` (wall clock): its mean over
    /// the time it was listed, that time and its peak.
    pub(crate) fn held_on(&self, key: &ProcessKey, gpu: &EntityId, window: (u64, u64)) -> Held {
        self.processes
            .get(key)
            .and_then(|record| record.gpus.get(gpu))
            .map(|series| cells_between(series, utc_window(window), 1)[0])
            .unwrap_or_default()
    }
}

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
    let mut book = ProcessMemoryBook::new(1000, 0);
    book.record_table(&table(
        1,
        1,
        vec![row(9, "python", &[(GPU_A, Some(4 * GIB))])],
    ));
    book.record_table(&table(
        2,
        2,
        vec![row(9, "python", &[(GPU_A, Some(4 * GIB))])],
    ));
    book.record_table(&table(
        3,
        10,
        vec![row(9, "python", &[(GPU_A, Some(4 * GIB))])],
    ));
    let held = book.held_on(&key(9), &gpu(GPU_A), (0, 20 * SECOND));
    assert_eq!(held.covered_ns, 4 * SECOND, "1 s, then 3 s of the 8 s gap");
    assert!(close(held.mean(), gib(4.0)));
    assert_eq!(held.peak, Some(4 * GIB));
    book.record_table(&table(
        4,
        11,
        vec![row(9, "python", &[(GPU_A, Some(4 * GIB))])],
    ));
    let held = book.held_on(&key(9), &gpu(GPU_A), (0, 20 * SECOND));
    assert_eq!(held.covered_ns, 5 * SECOND);
    // A table without a read time is not one.
    book.record_table(&ProcessTable::default());
    assert_eq!(book.tracked().len(), 1);
}

/// A part is a process's share of the time lists were recorded: while it
/// is not listed it counts as 0, so the parts of all processes add up to
/// what they held together; cells without a list are unknown.
#[test]
fn parts_count_a_time_not_listed_as_zero_and_add_up() {
    // Tables every 5 s, on the edges of the 5 s buckets.
    let mut book = ProcessMemoryBook::new(5000, 0);
    let both = || {
        vec![
            row(1, "train", &[(GPU_A, Some(6 * GIB))]),
            row(2, "serve", &[(GPU_A, Some(2 * GIB))]),
        ]
    };
    book.record_table(&table(1, 10, both()));
    book.record_table(&table(2, 15, both()));
    // 1 exits at 20 s; 2 grows.
    book.record_table(&table(
        3,
        20,
        vec![row(2, "serve", &[(GPU_A, Some(4 * GIB))])],
    ));
    book.record_table(&table(
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
    let mut book = ProcessMemoryBook::new(1000, 0);
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
    book.record_table(&table(1, 1, both()));
    book.record_table(&table(2, 2, both()));
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
    let mut book = ProcessMemoryBook::new(1000, 0);
    let small = |pid| row(pid, "small", &[(GPU_A, Some(GIB))]);
    book.record_table(&table(1, 1, (1..=9).map(small).collect()));
    assert_eq!(book.tracked().len(), 8);
    assert_eq!(book.slot_of(&key(9)), None, "eight colors, one left over");
    book.record_table(&table(2, 2, (1..=9).map(small).collect()));
    // 9 grows past twice the smallest holders: it takes the place of one.
    let mut rows: Vec<ProcessRow> = (1..=8).map(small).collect();
    rows.push(row(9, "big", &[(GPU_A, Some(8 * GIB))]));
    book.record_table(&table(3, 3, rows.clone()));
    book.record_table(&table(4, 4, rows));
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
    let mut book = ProcessMemoryBook::new(1000, 0);
    book.record_table(&table(
        1,
        1,
        vec![row(1, "first-job", &[(GPU_A, Some(6 * GIB))])],
    ));
    book.record_table(&table(
        2,
        2,
        vec![row(1, "first-job", &[(GPU_A, Some(6 * GIB))])],
    ));
    // Eight other jobs come and go after it exits.
    let mut at = 3;
    for pid in 2..=9 {
        let name = format!("job{pid}");
        book.record_table(&table(at, at, vec![row(pid, &name, &[(GPU_A, Some(GIB))])]));
        book.record_table(&table(at + 1, at + 1, Vec::new()));
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
    let mut book = ProcessMemoryBook::new(1000, 0);
    book.record_table(&table(1, 1, vec![row(3, "old", &[(GPU_A, Some(GIB))])]));
    book.record_table(&table(2, 2, Vec::new()));
    assert_eq!(book.slot_of(&key(3)), Some(preferred_color("old")));
    book.record_table(&table(3, 2 + 86_401, Vec::new()));
    assert_eq!(book.slot_of(&key(3)), None);
    assert!(book.tracked().is_empty());
}

/// A longer interval lets a value hold longer.
#[test]
fn the_hold_follows_the_sampling_interval() {
    let mut book = ProcessMemoryBook::new(1000, 0);
    book.set_interval(5000);
    book.record_table(&table(1, 1, vec![row(9, "python", &[(GPU_A, Some(GIB))])]));
    book.record_table(&table(2, 11, vec![row(9, "python", &[(GPU_A, Some(GIB))])]));
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
    let mut book = ProcessMemoryBook::new(10_000, 0);
    for at in [10, 20, 30] {
        book.record_table(&table(
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

const MINUTE: u64 = 60 * SECOND;
const HOUR: u64 = 60 * MINUTE;
const MIB: u64 = 1 << 20;

/// `train`'s bytes in the `index`th table: they change with every table.
fn train_bytes(index: u64) -> u64 {
    (1 + (index * 7) % 37) * 64 * MIB
}

/// Tables every `every_s` seconds from `from_s`, `count` of them, `train`
/// holding `train_bytes` on GPU A; their read times placed `offset` ns
/// ahead on the wall clock.
fn feed_train(
    book: &mut ProcessMemoryBook,
    (from_s, every_s, count): (u64, u64, u64),
    offset: u64,
) {
    for index in 0..count {
        let at = from_s + index * every_s;
        let rows = vec![row(1, "train", &[(GPU_A, Some(train_bytes(index)))])];
        book.record(&table(index + 1, at, rows), i128::from(offset));
    }
}

/// What the old record computed, from the tables themselves: each value
/// held from its table until the next one (never beyond the hold), and the
/// mean of each cell over the time a list was recorded in it.
fn reference_means(
    (from_s, every_s, count): (u64, u64, u64),
    edges: &[(u64, u64)],
) -> Vec<Option<f64>> {
    edges
        .iter()
        .map(|(cell_start, cell_end)| {
            let (mut integral, mut listed) = (0.0, 0.0);
            // The last table is still pending: its time is not known yet.
            for index in 0..count.saturating_sub(1) {
                let start = (from_s + index * every_s) * SECOND;
                let end = start + every_s * SECOND;
                let overlap = end.min(*cell_end).saturating_sub(start.max(*cell_start));
                #[allow(clippy::cast_precision_loss)]
                let overlap = overlap as f64;
                #[allow(clippy::cast_precision_loss)]
                let bytes = train_bytes(index) as f64;
                integral += bytes * overlap;
                listed += overlap;
            }
            (listed > 0.0).then(|| integral / listed)
        })
        .collect()
}

/// Even cells of `[start, end)`.
fn even_edges((start, end): (u64, u64), cells: u64) -> Vec<(u64, u64)> {
    let edge = |cell: u64| start + (end - start) * cell / cells;
    (0..cells)
        .map(|cell| (edge(cell), edge(cell + 1)))
        .collect()
}

fn train_parts(book: &ProcessMemoryBook, window: (u64, u64), cells: usize) -> Vec<Option<f64>> {
    let gpus = BTreeSet::from([gpu(GPU_A)]);
    let parts = book.parts(&gpus, window, cells);
    assert_eq!(parts.len(), 1, "train's part only");
    parts[0].means.clone()
}

fn assert_within(actual: &[Option<f64>], expected: &[Option<f64>], tolerance: f64) {
    assert_eq!(actual.len(), expected.len());
    for (cell, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        match (actual, expected) {
            (Some(actual), Some(expected)) => assert!(
                (actual - expected).abs() <= tolerance,
                "cell {cell}: {actual} vs {expected} (± {tolerance})"
            ),
            _ => assert_eq!(actual, expected, "cell {cell}"),
        }
    }
}

/// Cells that are a chart's bars (on its sizes' boundaries) are exactly
/// what the old record gave: three hours of tables every 2 s, each changing
/// the value, in 180 one-minute bars.
#[test]
fn parts_on_bars_equal_the_old_record_exactly() {
    let mut book = ProcessMemoryBook::new(2_000, 0);
    let feed = (60, 2, 3 * 1_800);
    feed_train(&mut book, feed, 0);
    let window = (MINUTE, MINUTE + 3 * HOUR);
    let means = train_parts(&book, window, 180);
    let expected = reference_means(feed, &even_edges(window, 180));
    assert_within(&means, &expected, 1e-3);
}

/// A 24 h window: in 5 minute bars and in hour bars, the local clock at
/// UTC+8:30 and the wall clock ahead of the boot clock, the bars still
/// equal the old record's exact means; the old record would have answered
/// a day only from its minutes, which it kept for a day.
#[test]
fn a_day_of_parts_is_exact_in_five_minute_and_hour_bars() {
    let shift_s = 8 * 3_600 + 1_800;
    let mut book = ProcessMemoryBook::new(10_000, i32::try_from(shift_s).unwrap());
    let offset = 1_790_000_000 * SECOND;
    let feed = (60, 10, 6 * (24 * 60 + 30));
    feed_train(&mut book, feed, offset);
    let last_utc = offset + (60 + 10 * (feed.2 - 1)) * SECOND;
    for (tier, bars) in [(Tier::FiveMinutes, 288), (Tier::OneHour, 24)] {
        let shift = i64::try_from(shift_s * SECOND).unwrap();
        let (start, end) = tier.window_ending_at(i64::try_from(last_utc).unwrap(), shift, bars);
        let window = (u64::try_from(start).unwrap(), u64::try_from(end).unwrap());
        assert_eq!(window.1 - window.0, 24 * HOUR, "{tier:?}");
        let means = train_parts(&book, window, usize::try_from(bars).unwrap());
        let boot_edges: Vec<(u64, u64)> = even_edges(window, u64::try_from(bars).unwrap())
            .into_iter()
            .map(|(from, to)| (from.saturating_sub(offset), to.saturating_sub(offset)))
            .collect();
        assert_within(&means, &reference_means(feed, &boot_edges), 1e-3);
    }
    // Every series' buckets follow the local clock, as the chart's bars do.
    let local_hour =
        |start: i64| (start + i64::try_from(shift_s).unwrap() * 1_000_000_000) % 3_600_000_000_000;
    let train = &book.processes[&key(1)].gpus[&gpu(GPU_A)];
    for series in [train, &book.listing] {
        assert!(series.buckets(Tier::OneHour).count() >= 24);
        assert!(
            series
                .buckets(Tier::OneHour)
                .all(|bucket| local_hour(bucket.start_utc_ns) == 0)
        );
    }
}

/// Cells off the sizes' boundaries (an odd window in 13 cells) cut
/// buckets at their edges and take the share of each that falls in the
/// cell: the documented coarsening. The error of a cell is at most the
/// spread of the values over the two buckets cut, weighted by their share
/// of the cell (here minutes: older than the 5 s buckets reach).
#[test]
fn parts_of_cells_off_the_bars_are_within_the_cut_buckets() {
    let mut book = ProcessMemoryBook::new(2_000, 0);
    let feed = (60, 2, 1_800);
    feed_train(&mut book, feed, 0);
    let window = (MINUTE + 7 * SECOND, MINUTE + 7 * SECOND + HOUR);
    let means = train_parts(&book, window, 13);
    let expected = reference_means(feed, &even_edges(window, 13));
    #[allow(clippy::cast_precision_loss)]
    let spread = (36 * 64 * MIB) as f64;
    #[allow(clippy::cast_precision_loss)]
    let tolerance = spread * 2.0 * MINUTE as f64 / (HOUR as f64 / 13.0);
    assert_within(&means, &expected, tolerance);
    let whole = train_parts(&book, window, 1);
    assert_within(
        &whole,
        &reference_means(feed, &[window]),
        spread * 2.0 / 60.0,
    );
}

/// A process that appears in the middle of a window: before, lists were
/// recorded without it (its part is 0); the bar it appears in holds it for
/// the part of the bar it was listed; before any list, nothing is known.
#[test]
fn a_process_that_appears_mid_window_is_zero_before() {
    let mut book = ProcessMemoryBook::new(5_000, 0);
    for at in (60..=360).step_by(5) {
        let mut rows = vec![row(1, "train", &[(GPU_A, Some(6 * GIB))])];
        if at >= 200 {
            rows.push(row(2, "serve", &[(GPU_A, Some(3 * GIB))]));
        }
        book.record_table(&table(at, at, rows));
    }
    let window = (0, 6 * MINUTE);
    let serve = book.part_means(&key(2), on_a, window, 6);
    assert_eq!(serve[0], None, "no list before the first table");
    assert!(close(serve[1], 0.0) && close(serve[2], 0.0));
    assert!(close(serve[3], gib(3.0) * 40.0 / 60.0), "{:?}", serve[3]);
    assert!(close(serve[4], gib(3.0)));
    let train = book.part_means(&key(1), on_a, window, 6);
    assert!(close(train[3], gib(6.0)));
}

/// A GPU that leaves the tables (removed, or the process let it go): its
/// part counts 0 from then on, and a window after it holds no part there.
#[test]
fn a_gpu_removed_from_the_tables_has_no_part_after() {
    let mut book = ProcessMemoryBook::new(5_000, 0);
    for at in (60..=300).step_by(5) {
        let held: &[(&str, Option<u64>)] = if at < 180 {
            &[(GPU_A, Some(2 * GIB)), (GPU_B, Some(GIB))]
        } else {
            &[(GPU_A, Some(2 * GIB))]
        };
        book.record_table(&table(at, at, vec![row(5, "ddp", held)]));
    }
    let on_b = |id: &EntityId| id.as_str() == GPU_B;
    let both = book.part_means(&key(5), on_b, (MINUTE, 5 * MINUTE), 4);
    assert!(close(both[0], gib(1.0)) && close(both[1], gib(1.0)));
    assert!(close(both[2], 0.0) && close(both[3], 0.0));
    let after = BTreeSet::from([gpu(GPU_B)]);
    assert!(book.parts(&after, (3 * MINUTE, 5 * MINUTE), 2).is_empty());
    let a = book.part_means(&key(5), on_a, (MINUTE, 5 * MINUTE), 4);
    assert!(a.iter().all(|mean| close(*mean, gib(2.0))));
}
