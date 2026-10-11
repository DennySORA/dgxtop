use dgxtop_core::clock::NANOS_PER_SECOND;
use dgxtop_core::identity::{Pid, PidNamespace, ProcessKey};
use dgxtop_core::process_probe::{IoCounters, ProbeLiveness, ProbeReading};
use dgxtop_core::testing::{boot_id, host_id};

use super::*;

const SECOND: u64 = NANOS_PER_SECOND;

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

fn with_counts() -> ProbeReading {
    let mut reading = running(None);
    reading.threads = Some(24);
    reading.swap_bytes = Some(4096);
    reading.open_files = Some(31);
    reading
}

/// Derives `reading` read at `second` from `state`.
fn derive(state: &ProbeState, reading: &ProbeReading, second: u64) -> ProbeState {
    ProcessProbeDeriver.derive(state, reading, second * SECOND)
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
/// same instant as its base has no rate either: the previous rates spoke
/// of the interval before it.
#[test]
fn unreadable_counters_and_a_zero_interval_clear_the_rates() {
    let first = derive(&ProbeState::default(), &running(Some((1_000, 0))), 1);
    let rated = derive(&first, &running(Some((3_000, 1_000))), 2);
    assert_eq!(rated.values.disk, [Some(2_000.0), Some(1_000.0)]);
    let same_instant = derive(&rated, &running(Some((9_000, 9_000))), 2);
    assert_eq!(same_instant.values.disk, [None, None]);
    let unreadable = derive(&same_instant, &running(None), 3);
    assert_eq!(unreadable.values.disk, [None, None]);
    assert_eq!(unreadable.bases.io, None);
    let again = derive(&unreadable, &running(Some((10_000, 10_000))), 4);
    assert_eq!(again.values.disk, [None, None], "it counts from scratch");
}

/// The threads, swap and open files become values as read (D-77); one
/// that could not be read is unknown.
#[test]
fn probes_give_threads_swap_and_files() {
    let values = derive(&ProbeState::default(), &with_counts(), 1).values;
    assert_eq!(
        (values.threads, values.swap, values.open_files),
        (Some(24.0), Some(4096.0), Some(31.0))
    );
    let unread = derive(&ProbeState::default(), &running(None), 2).values;
    assert_eq!(unread.threads, None, "nothing else was read");
    assert_eq!(unread.open_files, None);
}
