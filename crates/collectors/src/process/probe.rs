//! What a probe reading of a followed GPU process gives its run's samples
//! (D-76, D-77, D-79): disk read and write rates from the IO counters, the
//! descendants' CPU as percent of one core from their ticks, and the
//! threads, swap, open files and descendants as read.
//!
//! Like every collector it keeps nothing between calls: the previous
//! reading's [`ProbeState`] comes in and the next one goes out, as
//! [`dgxtop_core::process_probe::ProbedValues`] and the bases the next
//! rates count from.

use dgxtop_core::clock::NANOS_PER_SECOND;
use dgxtop_core::ports::ProbeDeriver;
use dgxtop_core::process_probe::{ChildUsage, IoCounters, ProbeBases, ProbeReading, ProbeState};

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
        clk_tck: Option<u64>,
    ) -> ProbeState {
        let mut next = *previous;
        take_io(&mut next, reading.io, read_boot_ns);
        take_children(&mut next, reading.children, (read_boot_ns, clk_tck));
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
/// gives no rate.
fn take_io(state: &mut ProbeState, io: Option<IoCounters>, read_boot_ns: u64) {
    let Some(io) = io else {
        state.bases.io = None;
        state.values.disk = [None, None];
        return;
    };
    if let Some((before, at)) = state.bases.io {
        let elapsed = read_boot_ns.saturating_sub(at);
        if elapsed > 0 {
            let rate = |now: u64, then: u64| {
                now.checked_sub(then)
                    .map(|bytes| as_count(bytes) * as_count(NANOS_PER_SECOND) / as_count(elapsed))
            };
            state.values.disk = [
                rate(io.read_bytes, before.read_bytes),
                rate(io.write_bytes, before.write_bytes),
            ];
        }
    }
    state.bases.io = Some((io, read_boot_ns));
}

/// The descendants' count and memory as read, and their CPU since the
/// previous reading as percent of one core. Their time drops when one
/// leaves the tree unreaped (a daemon, an orphan): that reading gives no
/// rate, and the next one counts from it.
fn take_children(
    state: &mut ProbeState,
    children: Option<ChildUsage>,
    (read_boot_ns, clk_tck): (u64, Option<u64>),
) {
    let Some(children) = children else {
        state.bases.child_ticks = None;
        state.values.children = None;
        state.values.children_cpu = None;
        state.values.children_memory = None;
        return;
    };
    state.values.children = Some(as_count(children.processes));
    state.values.children_memory = children.rss_bytes.map(as_count);
    state.values.children_cpu =
        children_cpu(state.bases, children.cpu_ticks, read_boot_ns, clk_tck);
    state.bases.child_ticks = Some((children.cpu_ticks, read_boot_ns));
}

fn children_cpu(
    bases: ProbeBases,
    cpu_ticks: u64,
    read_boot_ns: u64,
    clk_tck: Option<u64>,
) -> Option<f64> {
    let (before, at) = bases.child_ticks?;
    let ticks = cpu_ticks.checked_sub(before)?;
    let elapsed = read_boot_ns
        .checked_sub(at)
        .filter(|elapsed| *elapsed > 0)?;
    let per_second = clk_tck.filter(|ticks| *ticks > 0)?;
    Some(
        as_count(ticks) * 100.0 * as_count(NANOS_PER_SECOND)
            / as_count(per_second)
            / as_count(elapsed),
    )
}

#[cfg(test)]
mod tests;
