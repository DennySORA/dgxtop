//! The process history book (D-76): follows every GPU process as a run,
//! from the first process table that lists it using a GPU until it ends,
//! and keeps the latest runs listed (D-77: however long ago they ended).
//!
//! Each cycle it adds a sample to each live run (a [`RunSeries`] of the
//! compression, D-79) and asks the
//! process probe about the live runs: their command line, working folder
//! and details once, their IO counters, threads, swap, open files and
//! descendants each cycle (D-77), which a [`ProbeDeriver`] turns into the
//! samples' values (D-79), and, for a run the table no longer lists,
//! whether it ended and how ([`ending`]). Closed buckets and changed
//! runs leave through [`ProcessHistoryBook::take_journal`] for the
//! long-term store; the short-term file takes [`ProcessHistoryBook::recent`].
//! Nothing here performs I/O.

mod ending;
mod service;

pub(crate) use self::service::{ProcessHistoryPorts, ProcessHistoryService};

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_compression::{RunSample, RunSeries};
use dgxtop_core::action::Signal;
use dgxtop_core::canonical::{ProcessRow, ProcessTable};
use dgxtop_core::identity::{BootId, EntityId, ProcessKey};
use dgxtop_core::ports::ProbeDeriver;
use dgxtop_core::process_history::{
    EndCause, MAX_LISTED_RUNS, MAX_LIVE_RUNS, ProcessHistoryJournal, ProcessHistoryLoad,
    RUN_METRICS, RecentHistory, RecentRun, RunBucket, RunBucketRow, RunEnd, RunInfo, RunMetric,
    Tier,
};
use dgxtop_core::process_probe::{
    ProbeLiveness, ProbeReading, ProbeReport, ProbeState, ProbeTarget, ProbedValues,
};

use self::ending::{EndEvidence, end_cause};
use crate::process::Now;

const NANOS_PER_SECOND: u64 = 1_000_000_000;
/// A live run the table stops listing ends after this long even when no
/// probe answers.
const MISSING_GRACE_NS: u64 = 10 * NANOS_PER_SECOND;
/// Ended runs whose buckets stay in memory; older ones keep their figures
/// and read their buckets from the long-term store when opened.
const ENDED_RUNS_WITH_SERIES: usize = 32;

/// What a cycle's sample needs beside the process table.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SampleInputs<'a> {
    pub(crate) now: Now,
    /// Clock ticks per second, to place a run's start; `None` takes the
    /// time it was first seen.
    pub(crate) clk_tck: Option<u64>,
    pub(crate) gpu_labels: &'a BTreeMap<EntityId, String>,
    /// What the host receives and sends, bytes per second.
    pub(crate) host_network: [Option<f64>; 2],
}

/// One run: what it was, what it used, and while it runs, what following
/// it needs.
#[derive(Debug, Clone)]
pub struct Run {
    info: RunInfo,
    series: RunSeries,
    live: Option<Live>,
    changed: bool,
}

impl Run {
    pub(crate) fn info(&self) -> &RunInfo {
        &self.info
    }

    pub(crate) fn series(&self) -> &RunSeries {
        &self.series
    }

    pub(crate) fn is_live(&self) -> bool {
        self.live.is_some()
    }

    /// What the last probe read of it, while it runs.
    pub(crate) fn probed(&self) -> ProbedValues {
        self.live
            .as_ref()
            .map_or_else(ProbedValues::default, |live| live.probe.values)
    }
}

#[derive(Debug, Clone, Default)]
struct Live {
    /// What the last probe reading gave, and the bases the next one's
    /// rates count from.
    probe: ProbeState,
    text_read: bool,
    missing_since_boot_ns: Option<u64>,
    /// Restored at startup and not listed since: if it is gone, it ended
    /// while dgxtop was off.
    restored: bool,
}

#[derive(Debug)]
pub struct ProcessHistoryBook {
    boot_id: BootId,
    runs: BTreeMap<ProcessKey, Run>,
    signals: BTreeMap<ProcessKey, (Signal, u64)>,
    oom_kills: Option<u64>,
    /// The table sampled last (cycle, read time): every committed batch
    /// hands over the snapshot's table, sampled once.
    last_table: Option<(u64, u64)>,
    /// Clock ticks per second, from the samples, for the descendants' CPU.
    clk_tck: Option<u64>,
    /// The local clock's offset, nanoseconds east: where buckets begin.
    shift_ns: i64,
    journal: ProcessHistoryJournal,
    /// Runs whose buckets were asked of the long-term store.
    reading: BTreeSet<ProcessKey>,
}

impl ProcessHistoryBook {
    pub fn new(boot_id: BootId) -> Self {
        Self {
            boot_id,
            runs: BTreeMap::new(),
            signals: BTreeMap::new(),
            oom_kills: None,
            last_table: None,
            clk_tck: None,
            shift_ns: 0,
            journal: ProcessHistoryJournal::default(),
            reading: BTreeSet::new(),
        }
    }

    /// Buckets follow the local clock `utc_offset_s` east of UTC, so a
    /// day's is a local day.
    pub fn with_utc_offset(mut self, utc_offset_s: i32) -> Self {
        self.shift_ns = i64::from(utc_offset_s) * 1_000_000_000;
        self
    }

    /// The runs, in key order.
    pub(crate) fn runs(&self) -> impl Iterator<Item = &Run> {
        self.runs.values()
    }

    pub(crate) fn run(&self, key: &ProcessKey) -> Option<&Run> {
        self.runs.get(key)
    }

    /// Adds this cycle's sample of each live run; starts a run for each GPU
    /// process not yet followed; notes the live runs the table no longer
    /// lists and ends the ones gone for longer than the grace.
    pub(crate) fn observe(&mut self, table: &ProcessTable, inputs: &SampleInputs<'_>) {
        let identity = (table.cycle_id, table.read_finished_boot_ns);
        if self.last_table == Some(identity) {
            return;
        }
        self.last_table = Some(identity);
        if inputs.clk_tck.is_some() {
            self.clk_tck = inputs.clk_tck;
        }
        let utc_ns = utc_of(inputs.now);
        let mut listed = BTreeSet::new();
        for row in &table.rows {
            listed.insert(&row.key);
            if !self.runs.contains_key(&row.key) {
                if row.gpu.is_empty() || self.live_count() >= MAX_LIVE_RUNS {
                    continue;
                }
                let run = new_run(row, inputs, (utc_ns, self.shift_ns));
                self.runs.insert(row.key.clone(), run);
            }
            let Some(run) = self.runs.get_mut(&row.key) else {
                continue;
            };
            if run.info.end.is_some() {
                // Listed again: it never ended (a run restored as ended).
                run.info.end = None;
                run.live = Some(Live::default());
                run.changed = true;
            }
            let closed = sample_run(run, row, inputs, utc_ns);
            self.journal_buckets(&row.key, closed);
        }
        if !table.truncated {
            self.note_missing(&listed, inputs.now);
        }
        self.bound();
    }

    /// The live runs to probe this cycle: first the ones the table lost.
    pub(crate) fn probe_targets(&self) -> Vec<ProbeTarget> {
        let mut targets: Vec<(bool, ProbeTarget)> = self
            .runs
            .iter()
            .filter_map(|(key, run)| {
                let live = run.live.as_ref()?;
                Some((
                    live.missing_since_boot_ns.is_none(),
                    ProbeTarget {
                        key: key.clone(),
                        with_text: !live.text_read,
                    },
                ))
            })
            .collect();
        targets.sort_by_key(|(listed, _)| *listed);
        targets
            .into_iter()
            .take(MAX_LIVE_RUNS)
            .map(|(_, target)| target)
            .collect()
    }

    /// Takes what the probe read: texts, the values `deriver` makes of the
    /// readings (D-79), and the end of each run it found ended.
    pub(crate) fn on_probe(&mut self, report: &ProbeReport, now: Now, deriver: &dyn ProbeDeriver) {
        let oom_rose = matches!(
            (self.oom_kills, report.oom_kills),
            (Some(before), Some(after)) if after > before
        );
        if report.oom_kills.is_some() {
            self.oom_kills = report.oom_kills;
        }
        for reading in &report.readings {
            self.take_reading(reading, (report.read_boot_ns, now), oom_rose, deriver);
        }
    }

    /// dgxtop sent `signal` to `key` (D-71): if the run ends within a
    /// minute, that is why.
    pub(crate) fn note_signal(&mut self, key: &ProcessKey, signal: Signal, now: Now) {
        if self.runs.contains_key(key) {
            self.signals.insert(key.clone(), (signal, now.boot_ns));
        }
    }

    /// Takes the runs the stores kept: unfinished runs of an earlier boot
    /// ended with it; this boot's resume until a table shows whether they
    /// still run.
    pub(crate) fn restore(&mut self, load: ProcessHistoryLoad, now: Now) {
        for info in load.runs {
            if self.runs.contains_key(&info.key) {
                continue;
            }
            let run = restored_run(info, (&self.boot_id, self.shift_ns), now);
            if run.changed {
                self.journal.runs.push(run.info.clone());
            }
            self.runs.insert(run.info.key.clone(), run);
        }
        for row in load.buckets {
            if let Some(run) = self.runs.get_mut(&row.key) {
                run.series.restore(row.tier, row.bucket, false);
            }
        }
        if let Some(recent) = load.recent.filter(|recent| recent.boot_id == self.boot_id) {
            for saved in recent.runs {
                let Some(run) = self.runs.get_mut(&saved.key).filter(|run| run.is_live()) else {
                    continue;
                };
                for bucket in saved.recent {
                    run.series.restore(Tier::Recent, bucket, false);
                }
                for (tier, bucket) in saved.filling {
                    run.series.restore(tier, bucket, true);
                }
            }
        }
        let keys: Vec<ProcessKey> = self.runs.keys().cloned().collect();
        for key in keys {
            self.backfill(&key);
        }
        self.bound();
    }

    /// Makes the sizes a run's stored buckets lack from the finer ones
    /// (D-77), and journals them.
    fn backfill(&mut self, key: &ProcessKey) {
        let Some(run) = self.runs.get_mut(key) else {
            return;
        };
        let live = run.is_live();
        let made = run.series.backfill(live);
        if !made.is_empty() {
            self.journal
                .buckets
                .extend(made.into_iter().map(|(tier, bucket)| RunBucketRow {
                    key: key.clone(),
                    tier,
                    bucket,
                }));
        }
    }

    /// A run whose buckets are not in memory, to read from the long-term
    /// store; once per run.
    pub(crate) fn wants_buckets(&mut self, key: &ProcessKey) -> bool {
        let Some(run) = self.runs.get(key) else {
            return false;
        };
        if !run.series.is_empty() || run.is_live() || self.reading.contains(key) {
            return false;
        }
        self.reading.insert(key.clone());
        true
    }

    /// Takes the buckets read for `key`; drops the buckets of the ended
    /// runs that ended longest ago beyond the ones kept in memory.
    pub(crate) fn on_buckets(&mut self, key: &ProcessKey, rows: Vec<RunBucketRow>) {
        if let Some(run) = self.runs.get_mut(key) {
            for row in rows.into_iter().filter(|row| row.key == *key) {
                run.series.restore(row.tier, row.bucket, false);
            }
        }
        self.backfill(key);
        self.limit_series(Some(key));
    }

    /// What closed and changed since the last call, for the long-term
    /// store.
    pub(crate) fn take_journal(&mut self) -> ProcessHistoryJournal {
        for run in self.runs.values_mut().filter(|run| run.changed) {
            run.changed = false;
            self.journal.runs.push(run.info.clone());
        }
        std::mem::take(&mut self.journal)
    }

    /// Marks every live run changed, so the next journal carries the time
    /// each was last seen (at shutdown: a restart dates what ended
    /// meanwhile from it).
    pub(crate) fn touch_live(&mut self) {
        for run in self.runs.values_mut().filter(|run| run.is_live()) {
            run.changed = true;
        }
    }

    /// The live runs' recent buckets, for the short-term file.
    pub(crate) fn recent(&self, now: Now) -> RecentHistory {
        RecentHistory {
            boot_id: self.boot_id.clone(),
            saved_utc_ns: utc_of(now),
            runs: self
                .runs
                .values()
                .filter(|run| run.is_live())
                .map(|run| RecentRun {
                    key: run.info.key.clone(),
                    recent: run.series.buckets(Tier::Recent).copied().collect(),
                    filling: run.series.filling(),
                })
                .collect(),
        }
    }

    fn live_count(&self) -> usize {
        self.runs.values().filter(|run| run.is_live()).count()
    }

    fn take_reading(
        &mut self,
        reading: &ProbeReading,
        (read_boot_ns, now): (u64, Now),
        oom_rose: bool,
        deriver: &dyn ProbeDeriver,
    ) {
        let signal = self.signal_of(&reading.key, now);
        let clk_tck = self.clk_tck;
        let Some(run) = self.runs.get_mut(&reading.key) else {
            return;
        };
        let Some(live) = run.live.as_mut() else {
            return;
        };
        if reading.liveness == ProbeLiveness::Running {
            take_text(&mut run.info, live, reading, &mut run.changed);
            live.probe = deriver.derive(&live.probe, reading, read_boot_ns, clk_tck);
            return;
        }
        // The probe read the process itself after the table did: gone is
        // gone, also on a busy host whose tables are cut short.
        let evidence = EndEvidence {
            signal,
            status: match reading.liveness {
                ProbeLiveness::Zombie(status) => status,
                ProbeLiveness::Running | ProbeLiveness::Gone => None,
            },
            oom_rose,
            while_away: live.restored,
        };
        self.end(&reading.key, &evidence, now);
    }

    fn signal_of(&self, key: &ProcessKey, now: Now) -> Option<Signal> {
        self.signals
            .get(key)
            .filter(|(_, sent)| now.boot_ns.saturating_sub(*sent) <= ending::SIGNAL_WINDOW_NS)
            .map(|(signal, _)| *signal)
    }

    fn note_missing(&mut self, listed: &BTreeSet<&ProcessKey>, now: Now) {
        let mut overdue = Vec::new();
        for (key, run) in &mut self.runs {
            let Some(live) = run.live.as_mut() else {
                continue;
            };
            if listed.contains(key) {
                live.missing_since_boot_ns = None;
                live.restored = false;
                continue;
            }
            let since = *live.missing_since_boot_ns.get_or_insert(now.boot_ns);
            if now.boot_ns.saturating_sub(since) >= MISSING_GRACE_NS {
                overdue.push((key.clone(), live.restored));
            }
        }
        for (key, restored) in overdue {
            let evidence = EndEvidence {
                signal: self.signal_of(&key, now),
                status: None,
                oom_rose: false,
                while_away: restored,
            };
            self.end(&key, &evidence, now);
        }
    }

    fn end(&mut self, key: &ProcessKey, evidence: &EndEvidence, now: Now) {
        let Some(run) = self.runs.get_mut(key) else {
            return;
        };
        if run.live.take().is_none() {
            return;
        }
        let cause = end_cause(evidence);
        let utc_ns = if evidence.while_away {
            run.info.last_seen_utc_ns
        } else {
            utc_of(now)
        };
        run.info.end = Some(RunEnd { utc_ns, cause });
        run.changed = true;
        let closed = run.series.close_all();
        self.signals.remove(key);
        self.journal_buckets(key, closed);
        self.limit_series(None);
    }

    fn journal_buckets(&mut self, key: &ProcessKey, closed: Vec<(Tier, RunBucket)>) {
        if closed.is_empty() {
            return;
        }
        if let Some(run) = self.runs.get_mut(key) {
            // A closed minute is also when the time it was last seen is
            // worth keeping.
            run.changed = true;
        }
        self.journal
            .buckets
            .extend(closed.into_iter().map(|(tier, bucket)| RunBucketRow {
                key: key.clone(),
                tier,
                bucket,
            }));
    }

    /// Keeps at most [`MAX_LISTED_RUNS`] runs: the ended runs that ended
    /// first give way.
    fn bound(&mut self) {
        let excess = self.runs.len().saturating_sub(MAX_LISTED_RUNS);
        if excess > 0 {
            let mut ended: Vec<(i64, ProcessKey)> = self
                .runs
                .iter()
                .filter_map(|(key, run)| Some((run.info.end?.utc_ns, key.clone())))
                .collect();
            ended.sort();
            for (_, key) in ended.into_iter().take(excess) {
                self.runs.remove(&key);
            }
        }
        self.limit_series(None);
    }

    /// Keeps the buckets of at most [`ENDED_RUNS_WITH_SERIES`] ended runs:
    /// the ones that ended last, and `keep`.
    fn limit_series(&mut self, keep: Option<&ProcessKey>) {
        let mut ended: Vec<(i64, ProcessKey)> = self
            .runs
            .iter()
            .filter(|(key, run)| Some(*key) != keep && !run.series.is_empty())
            .filter_map(|(key, run)| Some((run.info.end?.utc_ns, key.clone())))
            .collect();
        let limit = ENDED_RUNS_WITH_SERIES.saturating_sub(usize::from(keep.is_some()));
        if ended.len() <= limit {
            return;
        }
        ended.sort_by(|left, right| right.cmp(left));
        for (_, key) in ended.into_iter().skip(limit) {
            if let Some(run) = self.runs.get_mut(&key) {
                run.series.clear();
                self.reading.remove(&key);
            }
        }
    }
}

fn utc_of(now: Now) -> i64 {
    i64::try_from(now.utc_ns).unwrap_or(i64::MAX)
}

fn new_run(row: &ProcessRow, inputs: &SampleInputs<'_>, (utc_ns, shift_ns): (i64, i64)) -> Run {
    Run {
        info: RunInfo {
            key: row.key.clone(),
            name: row.name.clone(),
            // The table's bounded command line, until the probe reads the
            // whole of it.
            command: row.command.clone(),
            cwd: String::new(),
            uid: row.uid,
            gpus: Vec::new(),
            details: dgxtop_core::process_history::RunDetails::default(),
            started_utc_ns: started_utc_ns(&row.key, inputs, utc_ns),
            last_seen_utc_ns: utc_ns,
            end: None,
        },
        series: RunSeries::new(shift_ns),
        live: Some(Live::default()),
        changed: true,
    }
}

/// When the process started: its start ticks since boot, placed on the
/// wall clock through the time since boot now.
fn started_utc_ns(key: &ProcessKey, inputs: &SampleInputs<'_>, utc_ns: i64) -> i64 {
    let Some(clk_tck) = inputs.clk_tck.filter(|ticks| *ticks > 0) else {
        return utc_ns;
    };
    let started_boot_ns =
        u128::from(key.start_time_ticks) * u128::from(NANOS_PER_SECOND) / u128::from(clk_tck);
    let ago = u128::from(inputs.now.boot_ns).saturating_sub(started_boot_ns);
    utc_ns.saturating_sub(i64::try_from(ago).unwrap_or(i64::MAX))
}

fn restored_run(mut info: RunInfo, (boot_id, shift_ns): (&BootId, i64), now: Now) -> Run {
    let mut changed = false;
    let live = if info.end.is_some() {
        None
    } else if info.key.boot_id == *boot_id {
        Some(Live {
            missing_since_boot_ns: Some(now.boot_ns),
            restored: true,
            ..Live::default()
        })
    } else {
        info.end = Some(RunEnd {
            utc_ns: info.last_seen_utc_ns,
            cause: EndCause::HostRestarted,
        });
        changed = true;
        None
    };
    Run {
        info,
        series: RunSeries::new(shift_ns),
        live,
        changed,
    }
}

/// Adds `row`'s sample to `run` and updates what it says of the run.
fn sample_run(
    run: &mut Run,
    row: &ProcessRow,
    inputs: &SampleInputs<'_>,
    utc_ns: i64,
) -> Vec<(Tier, RunBucket)> {
    let info = &mut run.info;
    info.last_seen_utc_ns = utc_ns;
    if !row.name.is_empty() && row.name != "?" && info.name != row.name {
        info.name.clone_from(&row.name);
        run.changed = true;
    }
    if info.command.is_empty() && !row.command.is_empty() {
        info.command.clone_from(&row.command);
        run.changed = true;
    }
    if info.uid.is_none() && row.uid.is_some() {
        info.uid = row.uid;
        run.changed = true;
    }
    for link in &row.gpu {
        let label = inputs
            .gpu_labels
            .get(&link.gpu_entity_id)
            .cloned()
            .unwrap_or_else(|| link.gpu_entity_id.to_string());
        if !info.gpus.contains(&label) {
            info.gpus.push(label);
            info.gpus.sort();
            run.changed = true;
        }
    }
    let probed = run.probed();
    let mut sample: RunSample = [None; RUN_METRICS];
    sample[RunMetric::Cpu.index()] = row.cpu_percent;
    #[allow(clippy::cast_precision_loss)]
    {
        sample[RunMetric::Memory.index()] = row.rss_bytes.map(|bytes| bytes as f64);
        sample[RunMetric::GpuMemory.index()] = sum_present(
            row.gpu
                .iter()
                .map(|link| link.memory_bytes.map(|bytes| bytes as f64)),
        );
    }
    sample[RunMetric::GpuUtilization.index()] =
        sum_present(row.gpu.iter().map(|link| link.utilization_percent));
    sample[RunMetric::DiskRead.index()] = probed.disk[0];
    sample[RunMetric::DiskWrite.index()] = probed.disk[1];
    sample[RunMetric::HostNetworkReceive.index()] = inputs.host_network[0];
    sample[RunMetric::HostNetworkTransmit.index()] = inputs.host_network[1];
    sample[RunMetric::Threads.index()] = probed.threads;
    sample[RunMetric::Children.index()] = probed.children;
    sample[RunMetric::ChildrenCpu.index()] = probed.children_cpu;
    sample[RunMetric::ChildrenMemory.index()] = probed.children_memory;
    sample[RunMetric::Swap.index()] = probed.swap;
    sample[RunMetric::OpenFiles.index()] = probed.open_files;
    run.series.add(utc_ns, &sample)
}

/// The sum of the values present; `None` when none is.
fn sum_present(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    values
        .flatten()
        .fold(None, |sum, value| Some(sum.unwrap_or(0.0) + value))
}

fn take_text(info: &mut RunInfo, live: &mut Live, reading: &ProbeReading, changed: &mut bool) {
    if live.text_read {
        return;
    }
    live.text_read = true;
    if let Some(command) = reading
        .command
        .as_ref()
        .filter(|command| **command != info.command)
    {
        info.command.clone_from(command);
        *changed = true;
    }
    if let Some(cwd) = reading.cwd.as_ref().filter(|cwd| **cwd != info.cwd) {
        info.cwd.clone_from(cwd);
        *changed = true;
    }
    if let Some(details) = reading
        .details
        .as_ref()
        .filter(|details| **details != info.details)
    {
        info.details.clone_from(details);
        *changed = true;
    }
}

#[cfg(test)]
mod tests;
