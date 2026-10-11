//! What a probe reading of a followed GPU process gives its run's samples
//! (D-76, D-77, D-79): disk read and write rates from the IO counters, and
//! the threads, swap and open files as read. The descendants come with the
//! process table ([`crate::process::table`]), which scans every process.
//!
//! Like every collector it keeps nothing between calls: the previous
//! reading's [`ProbeState`] comes in and the next one goes out, as
//! [`dgxtop_core::process_probe::ProbedValues`] and the bases the next
//! rates count from.

use dgxtop_core::ports::ProbeDeriver;
use dgxtop_core::process_probe::{IoCounters, ProbeReading, ProbeState};

use crate::shared::counter;

/// Derives the probed values of a running process's reading from the
/// previous reading's state.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessProbeDeriver;

impl ProbeDeriver for ProcessProbeDeriver {
    fn derive(
        &self,
        previous: &ProbeState,
        reading: &ProbeReading,
        read_boot_ns: u64,
    ) -> ProbeState {
        let mut next = *previous;
        take_io(&mut next, reading.io, read_boot_ns);
        next.values.threads = reading.threads.map(as_count);
        next.values.swap = reading.swap_bytes.map(as_count);
        next.values.open_files = reading.open_files.map(as_count);
        next
    }
}

#[allow(clippy::cast_precision_loss)]
fn as_count(value: u64) -> f64 {
    value as f64
}

/// The disk rates since the previous reading; a counter that went back
/// (another process behind a reused key cannot, but a kernel may reset)
/// or no time since it gives no rate.
fn take_io(state: &mut ProbeState, io: Option<IoCounters>, read_boot_ns: u64) {
    let Some(io) = io else {
        state.bases.io = None;
        state.values.disk = [None, None];
        return;
    };
    state.values.disk = state.bases.io.map_or([None, None], |(before, at)| {
        let elapsed = read_boot_ns.saturating_sub(at);
        let rate = |now: u64, then: u64| {
            now.checked_sub(then)
                .and_then(|bytes| counter::rate_of(as_count(bytes), elapsed))
        };
        [
            rate(io.read_bytes, before.read_bytes),
            rate(io.write_bytes, before.write_bytes),
        ]
    });
    state.bases.io = Some((io, read_boot_ns));
}

#[cfg(test)]
mod tests;
