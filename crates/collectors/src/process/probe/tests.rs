use dgxtop_core::identity::{Pid, PidNamespace, ProcessKey};
use dgxtop_core::process_probe::{ChildUsage, IoCounters, ProbeLiveness, ProbeReading};
use dgxtop_core::testing::{boot_id, host_id};

use super::*;

const SECOND: u64 = NANOS_PER_SECOND;
const GIB: u64 = 1 << 30;
const CLK_TCK: Option<u64> = Some(100);

fn key() -> ProcessKey {
    ProcessKey {
        host_id: host_id("host-test"),
        boot_id: boot_id("boot-test"),
        pid_namespace: PidNamespace::new("pid:[4026531836]").unwrap(),
        pid: Pid::new(9).unwrap(),
        start_time_ticks: 7,
    }
}

fn running(io: Option<(u64, u64)>) -> ProbeReading {
    ProbeReading {
        liveness: ProbeLiveness::Running,
        io: io.map(|(read_bytes, write_bytes)| IoCounters {
            read_bytes,
            write_bytes,
        }),
        ..ProbeReading::gone(key())
    }
}

fn with_children(ticks: u64) -> ProbeReading {
    let mut reading = running(None);
    reading.threads = Some(24);
    reading.swap_bytes = Some(4096);
    reading.open_files = Some(31);
    reading.children = Some(ChildUsage {
        processes: 4,
        cpu_ticks: ticks,
        rss_bytes: Some(3 * GIB),
    });
    reading
}

/// Derives `reading` read at `second` from `state`.
fn derive(state: &ProbeState, reading: &ProbeReading, second: u64) -> ProbeState {
    ProcessProbeDeriver.derive(state, reading, second * SECOND, CLK_TCK)
}

/// The IO counters become disk rates from the second reading on.
#[test]
fn probes_give_disk_rates() {
    let first = derive(&ProbeState::default(), &running(Some((1_000, 0))), 1);
    assert_eq!(first.values.disk, [None, None], "a rate needs two readings");
    let second = derive(&first, &running(Some((5_000, 2_000))), 3);
    assert_eq!(second.values.disk, [Some(2_000.0), Some(1_000.0)]);
}

/// A counter that went back gives no rate for that reading, and the next
/// reading counts from it.
#[test]
fn a_counter_that_went_back_gives_no_rate() {
    let first = derive(&ProbeState::default(), &running(Some((5_000, 2_000))), 1);
    let reset = derive(&first, &running(Some((1_000, 3_000))), 2);
    assert_eq!(reset.values.disk, [None, Some(1_000.0)]);
    let after = derive(&reset, &running(Some((3_000, 3_000))), 3);
    assert_eq!(after.values.disk, [Some(2_000.0), Some(0.0)]);
}

/// Unreadable counters clear the rates and their base; a reading at the
/// same instant as its base keeps the rates it had.
#[test]
fn unreadable_counters_clear_and_a_zero_interval_keeps_the_rates() {
    let first = derive(&ProbeState::default(), &running(Some((1_000, 0))), 1);
    let rated = derive(&first, &running(Some((3_000, 1_000))), 2);
    let same_instant = derive(&rated, &running(Some((9_000, 9_000))), 2);
    assert_eq!(same_instant.values.disk, [Some(2_000.0), Some(1_000.0)]);
    let unreadable = derive(&same_instant, &running(None), 3);
    assert_eq!(unreadable.values.disk, [None, None]);
    assert_eq!(unreadable.bases.io, None);
    let again = derive(&unreadable, &running(Some((10_000, 10_000))), 4);
    assert_eq!(again.values.disk, [None, None], "it counts from scratch");
}

/// The threads, swap, open files and descendants become values (D-77): the
/// descendants' CPU from their ticks since the last reading.
#[test]
fn probes_give_threads_swap_files_and_descendants() {
    let first = derive(&ProbeState::default(), &with_children(1_000), 1);
    assert_eq!(first.values.children_cpu, None, "a rate needs two readings");
    // 300 ticks in 2 s at 100 a second: one and a half cores.
    let second = derive(&first, &with_children(1_300), 3);
    let values = second.values;
    assert_eq!(values.children_cpu, Some(150.0));
    assert_eq!(values.children, Some(4.0));
    #[allow(clippy::cast_precision_loss)]
    let held = (3 * GIB) as f64;
    assert_eq!(values.children_memory, Some(held));
    assert_eq!(
        (values.threads, values.swap, values.open_files),
        (Some(24.0), Some(4096.0), Some(31.0))
    );
    // A descendant that left the tree unreaped takes its time along: no
    // rate for that reading, and the next counts from it.
    let dropped = derive(&second, &with_children(900), 4);
    assert_eq!(dropped.values.children_cpu, None);
    assert_eq!(dropped.values.children, Some(4.0));
    let after = derive(&dropped, &with_children(1_000), 5);
    assert_eq!(after.values.children_cpu, Some(100.0));
    // A list that could not be read: the descendants are unknown.
    let unknown = derive(&after, &running(None), 6);
    assert_eq!(unknown.values.children, None);
    assert_eq!(unknown.values.children_memory, None);
    assert_eq!(unknown.bases.child_ticks, None);
    assert_eq!(unknown.values.threads, None, "nothing else was read");
}

/// Without clock ticks per second the descendants' CPU has no rate.
#[test]
fn descendants_cpu_needs_the_clock_ticks() {
    let first = ProcessProbeDeriver.derive(&ProbeState::default(), &with_children(0), SECOND, None);
    let second = ProcessProbeDeriver.derive(&first, &with_children(100), 2 * SECOND, Some(0));
    assert_eq!(second.values.children_cpu, None);
    assert_eq!(second.values.children, Some(4.0));
}
