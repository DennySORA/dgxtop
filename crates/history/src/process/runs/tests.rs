use std::collections::BTreeMap;
use std::sync::Mutex;

use dgxtop_core::action::Signal;
use dgxtop_core::canonical::{ProcessDescendants, ProcessRow, ProcessTable};
use dgxtop_core::identity::{EntityId, ProcessKey};
use dgxtop_core::process_history::{
    EndCause, ProcessHistoryLoad, RunBucket, RunDetails, RunMetric, Tier,
};
use dgxtop_core::process_probe::{
    ExitStatus, IoCounters, ProbeLiveness, ProbeReading, ProbeReport,
};
use dgxtop_core::testing::{
    IdleProbeDeriver, boot_id, gpu_process, process_table, test_process_key,
};

use super::*;

const SECOND: u64 = NANOS_PER_SECOND;
const GIB: u64 = 1 << 30;
const GPU_A: &str = "gpu:GPU-aaaa";
/// UTC at boot second 0 of the tests, on a ten-minute boundary.
const BOOT_UTC: i128 = 1_789_999_800_000_000_000;

fn now(second: u64) -> Now {
    Now {
        boot_ns: second * SECOND,
        utc_ns: BOOT_UTC + i128::from(second * SECOND),
    }
}

fn labels() -> BTreeMap<EntityId, String> {
    BTreeMap::from([(EntityId::new(GPU_A).unwrap(), "GPU 0".to_owned())])
}

fn row(pid: i64, cpu: f64, gpu_bytes: Option<u64>) -> ProcessRow {
    let mut row = gpu_process(pid, "acme_llm.train", &[(GPU_A, gpu_bytes)]);
    row.cpu_percent = Some(cpu);
    row.rss_bytes = Some(2 * GIB);
    row.command = "python -m acme_llm.train".to_owned();
    row
}

fn cpu_only(pid: i64) -> ProcessRow {
    let mut row = gpu_process(pid, "bash", &[]);
    row.cpu_percent = Some(1.0);
    row
}

/// The table read at `second` (each table its own cycle).
fn table(second: u64, rows: Vec<ProcessRow>) -> ProcessTable {
    process_table(second, second * SECOND, rows)
}

/// Observes `rows` at `second`, with 100 ticks per second.
fn observe(book: &mut ProcessHistoryBook, second: u64, rows: Vec<ProcessRow>) {
    let labels = labels();
    book.observe(
        &table(second, rows),
        &SampleInputs {
            now: now(second),
            clk_tck: Some(100),
            gpu_labels: &labels,
            host_network: [Some(1000.0), Some(500.0)],
        },
    );
}

fn key(pid: i64) -> ProcessKey {
    test_process_key(pid)
}

fn reading(pid: i64, liveness: ProbeLiveness, io: Option<(u64, u64)>) -> ProbeReading {
    ProbeReading {
        liveness,
        io: io.map(|(read_bytes, write_bytes)| IoCounters {
            read_bytes,
            write_bytes,
        }),
        ..ProbeReading::gone(key(pid))
    }
}

fn report(second: u64, oom_kills: Option<u64>, readings: Vec<ProbeReading>) -> ProbeReport {
    ProbeReport {
        readings,
        oom_kills,
        read_boot_ns: second * SECOND,
    }
}

fn book() -> ProcessHistoryBook {
    ProcessHistoryBook::new(boot_id("boot-test"))
}

/// Hands `report` to `book` with a deriver that keeps the state as it was.
fn probe(book: &mut ProcessHistoryBook, report: &ProbeReport, now: Now) {
    book.on_probe(report, now, &IdleProbeDeriver);
}

fn end_of(book: &ProcessHistoryBook, pid: i64) -> Option<EndCause> {
    book.run(&key(pid))?.info().end.map(|end| end.cause)
}

/// A GPU process becomes a run with its start placed on the wall clock;
/// a process without a GPU is not followed.
#[test]
fn gpu_processes_become_runs_with_their_start_time() {
    let mut book = book();
    observe(
        &mut book,
        100,
        vec![row(9, 50.0, Some(4 * GIB)), cpu_only(10)],
    );
    assert_eq!(book.runs().count(), 1);
    let run = book.run(&key(9)).unwrap();
    let info = run.info();
    assert!(run.is_live());
    // Start ticks 7 at 100 per second: 70 ms after boot.
    assert_eq!(
        i128::from(info.started_utc_ns),
        BOOT_UTC + 70_000_000,
        "start = now - (uptime - start ticks)"
    );
    assert_eq!(info.gpus, vec!["GPU 0".to_owned()]);
    assert_eq!(info.command, "python -m acme_llm.train");
    let journal = book.take_journal();
    assert_eq!(journal.runs.len(), 1, "a new run is journaled");
    assert!(book.take_journal().is_empty(), "once");
}

#[test]
fn samples_aggregate_the_run_and_minutes_reach_the_journal() {
    let mut book = book();
    for second in 0..61 {
        let cpu = if second % 2 == 0 { 100.0 } else { 0.0 };
        observe(&mut book, second, vec![row(9, cpu, Some(4 * GIB))]);
    }
    let journal = book.take_journal();
    let minutes: Vec<&RunBucket> = journal
        .buckets
        .iter()
        .filter(|row| row.tier == Tier::OneMinute)
        .map(|row| &row.bucket)
        .collect();
    assert_eq!(minutes.len(), 1);
    let minute = minutes[0];
    assert_eq!(minute.value(RunMetric::Cpu).count, 60);
    assert_eq!(minute.value(RunMetric::Cpu).peak(), Some(100.0));
    #[allow(clippy::cast_precision_loss)]
    let gib4 = (4 * GIB) as f64;
    assert_eq!(minute.value(RunMetric::GpuMemory).mean(), Some(gib4));
    assert_eq!(
        minute.value(RunMetric::HostNetworkReceive).mean(),
        Some(1000.0)
    );
    assert_eq!(minute.value(RunMetric::DiskRead).count, 0, "no probe yet");
    assert_eq!(journal.runs.len(), 1, "the run, with its last-seen time");
}

/// Records what the book hands the deriver and gives values that tell
/// the readings apart: the read time as the disk read rate, the threads as
/// read, and the read time as the IO base.
#[derive(Debug, Default)]
struct RecordingDeriver {
    calls: Mutex<Vec<(ProbeState, u64)>>,
}

impl RecordingDeriver {
    fn calls(&self) -> Vec<(ProbeState, u64)> {
        self.calls.lock().unwrap().clone()
    }
}

impl ProbeDeriver for RecordingDeriver {
    fn derive(
        &self,
        previous: &ProbeState,
        reading: &ProbeReading,
        read_boot_ns: u64,
    ) -> ProbeState {
        self.calls.lock().unwrap().push((*previous, read_boot_ns));
        #[allow(clippy::cast_precision_loss)]
        let as_value = |value: u64| value as f64;
        let mut next = *previous;
        next.values.disk = [Some(as_value(read_boot_ns / SECOND)), None];
        next.values.threads = reading.threads.map(as_value);
        let counters = IoCounters {
            read_bytes: read_boot_ns,
            write_bytes: 0,
        };
        next.bases.io = Some((counters, read_boot_ns));
        next
    }
}

/// The probe's texts replace the table's short command line once, and its
/// details are read once with them.
#[test]
fn probes_give_the_full_command_and_details_once() {
    let mut book = book();
    observe(&mut book, 1, vec![row(9, 10.0, Some(GIB))]);
    let targets = book.probe_targets();
    assert_eq!(targets.len(), 1);
    assert!(targets[0].with_text);
    let details = RunDetails {
        parent_pid: Some(7),
        parent_name: "torchrun".to_owned(),
        executable: "/venv/bin/python3.12".to_owned(),
        cgroup: "/user.slice".to_owned(),
        nice: Some(0),
    };
    let mut first = reading(9, ProbeLiveness::Running, Some((1_000, 0)));
    first.command = Some("python -m acme_llm.train --epochs 3 --data /srv/acme".to_owned());
    first.cwd = Some("/home/u/code/acme-llm".to_owned());
    first.details = Some(details.clone());
    probe(&mut book, &report(1, Some(0), vec![first]), now(1));
    let mut later = reading(9, ProbeLiveness::Running, None);
    later.command = Some("other".to_owned());
    probe(&mut book, &report(3, Some(0), vec![later]), now(3));
    let run = book.run(&key(9)).unwrap();
    assert_eq!(run.info().cwd, "/home/u/code/acme-llm");
    assert!(run.info().command.ends_with("--data /srv/acme"));
    assert_eq!(run.info().details, details);
    assert!(!book.probe_targets()[0].with_text, "texts are read once");
}

/// The book hands the deriver each running reading with the state the
/// last reading left and the probe's read time (D-79), and the next samples
/// take the values it gives.
#[test]
fn the_derived_values_become_the_next_samples() {
    let deriver = RecordingDeriver::default();
    let mut book = book();
    observe(&mut book, 1, vec![row(9, 10.0, Some(GIB))]);
    let mut probed = reading(9, ProbeLiveness::Running, None);
    probed.threads = Some(24);
    book.on_probe(&report(2, None, vec![probed.clone()]), now(2), &deriver);
    book.on_probe(&report(3, None, vec![probed]), now(3), &deriver);
    let calls = deriver.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], (ProbeState::default(), 2 * SECOND));
    let (second_previous, second_read) = calls[1];
    assert_eq!(second_read, 3 * SECOND);
    assert_eq!(
        second_previous.bases.io.map(|(_, at)| at),
        Some(2 * SECOND),
        "the state the first reading left"
    );
    let values = book.run(&key(9)).unwrap().probed();
    assert_eq!(values.disk, [Some(3.0), None]);
    assert_eq!(values.threads, Some(24.0));
    observe(&mut book, 4, vec![row(9, 10.0, Some(GIB))]);
    let bucket = *book
        .run(&key(9))
        .unwrap()
        .series()
        .buckets(Tier::Recent)
        .last()
        .unwrap();
    assert_eq!(bucket.value(RunMetric::DiskRead).mean(), Some(3.0));
    assert_eq!(bucket.value(RunMetric::Threads).mean(), Some(24.0));
    assert_eq!(bucket.value(RunMetric::DiskWrite).count, 0);
}

/// A reading of a process that no longer runs ends its run and is not
/// derived.
#[test]
fn readings_of_ended_processes_are_not_derived() {
    let deriver = RecordingDeriver::default();
    let mut book = book();
    observe(&mut book, 1, vec![row(9, 10.0, Some(GIB))]);
    book.on_probe(
        &report(2, None, vec![reading(9, ProbeLiveness::Gone, None)]),
        now(2),
        &deriver,
    );
    assert!(deriver.calls().is_empty());
    assert_eq!(end_of(&book, 9), Some(EndCause::NotVisible));
    assert_eq!(book.run(&key(9)).unwrap().probed(), ProbedValues::default());
}

/// A run the table stops listing ends as its probe says: dgxtop's own
/// signal first, a zombie's status, the OOM killer, else not visible.
#[test]
fn runs_end_with_the_cause_the_probe_shows() {
    let mut book = book();
    observe(
        &mut book,
        1,
        (1..=5).map(|pid| row(pid, 10.0, Some(GIB))).collect(),
    );
    probe(&mut book, &report(1, Some(4), Vec::new()), now(1));
    book.note_signal(&key(1), Signal::Sigterm, now(1));
    observe(&mut book, 2, Vec::new());
    assert_eq!(
        book.probe_targets().len(),
        5,
        "the runs the table lost are probed"
    );
    probe(
        &mut book,
        &report(
            2,
            Some(4),
            vec![
                reading(1, ProbeLiveness::Gone, None),
                reading(
                    2,
                    ProbeLiveness::Zombie(Some(ExitStatus::Signaled(15))),
                    None,
                ),
                reading(3, ProbeLiveness::Zombie(Some(ExitStatus::Exited(2))), None),
                reading(4, ProbeLiveness::Gone, None),
            ],
        ),
        now(2),
    );
    assert_eq!(
        end_of(&book, 1),
        Some(EndCause::SignaledByDgxtop(Signal::Sigterm))
    );
    assert_eq!(end_of(&book, 2), Some(EndCause::Signal(15)));
    assert_eq!(end_of(&book, 3), Some(EndCause::Exit(2)));
    assert_eq!(end_of(&book, 4), Some(EndCause::NotVisible));
    assert_eq!(end_of(&book, 5), None, "not probed yet");
    probe(
        &mut book,
        &report(3, Some(5), vec![reading(5, ProbeLiveness::Gone, None)]),
        now(3),
    );
    assert_eq!(end_of(&book, 5), Some(EndCause::OutOfMemory));
    let ended = book.run(&key(5)).unwrap();
    assert!(!ended.is_live());
    assert_eq!(
        i128::from(ended.info().end.unwrap().utc_ns),
        BOOT_UTC + i128::from(3 * SECOND)
    );
    assert!(book.probe_targets().is_empty());
    let journal = book.take_journal();
    assert!(
        journal
            .buckets
            .iter()
            .any(|row| row.tier == Tier::OneMinute),
        "an ended run's partial minute is kept"
    );
}

/// A zombie or a gone process ends at once even while the table still
/// lists it (the probe reads after the table); one the probe says runs
/// stays live.
#[test]
fn the_probe_decides_whether_a_listed_run_ended() {
    let mut book = book();
    observe(
        &mut book,
        1,
        vec![row(1, 10.0, Some(GIB)), row(2, 10.0, Some(GIB))],
    );
    probe(
        &mut book,
        &report(
            1,
            None,
            vec![
                reading(1, ProbeLiveness::Zombie(None), None),
                reading(2, ProbeLiveness::Gone, None),
            ],
        ),
        now(1),
    );
    assert_eq!(end_of(&book, 1), Some(EndCause::NotVisible));
    assert_eq!(end_of(&book, 2), Some(EndCause::NotVisible));
    observe(&mut book, 2, vec![row(3, 10.0, Some(GIB))]);
    probe(
        &mut book,
        &report(2, None, vec![reading(3, ProbeLiveness::Running, None)]),
        now(2),
    );
    assert!(book.run(&key(3)).unwrap().is_live());
}

/// Without a probe answer a lost run ends after the grace; a truncated
/// table says nothing of the processes it left out.
#[test]
fn lost_runs_end_after_the_grace_unless_the_table_was_cut() {
    let mut book = book();
    observe(&mut book, 1, vec![row(1, 10.0, Some(GIB))]);
    let mut cut = table(30, Vec::new());
    cut.truncated = true;
    let labels = labels();
    let inputs = |second| SampleInputs {
        now: now(second),
        clk_tck: Some(100),
        gpu_labels: &labels,
        host_network: [None, None],
    };
    book.observe(&cut, &inputs(30));
    assert!(book.run(&key(1)).unwrap().is_live());
    observe(&mut book, 31, Vec::new());
    observe(&mut book, 40, Vec::new());
    assert!(book.run(&key(1)).unwrap().is_live(), "9 s");
    observe(&mut book, 41, Vec::new());
    assert_eq!(end_of(&book, 1), Some(EndCause::NotVisible));
}

#[test]
fn a_signal_long_before_the_end_is_not_its_cause() {
    let mut book = book();
    observe(&mut book, 1, vec![row(1, 10.0, Some(GIB))]);
    book.note_signal(&key(1), Signal::Sigterm, now(1));
    observe(&mut book, 100, Vec::new());
    probe(
        &mut book,
        &report(100, None, vec![reading(1, ProbeLiveness::Gone, None)]),
        now(100),
    );
    assert_eq!(end_of(&book, 1), Some(EndCause::NotVisible));
}

/// Restored runs: an earlier boot's unfinished run ended with it; this
/// boot's resumes when listed and otherwise ended while dgxtop was off.
#[test]
fn restored_runs_resume_or_end_by_what_happened_meanwhile() {
    let mut writer = book();
    observe(
        &mut writer,
        1,
        vec![row(1, 10.0, Some(GIB)), row(2, 10.0, Some(GIB))],
    );
    let mut old_boot = writer.run(&key(1)).unwrap().info().clone();
    old_boot.key.boot_id = boot_id("boot-before");
    let resumed = writer.run(&key(1)).unwrap().info().clone();
    let away = writer.run(&key(2)).unwrap().info().clone();
    let mut recent = writer.recent(now(1));
    assert_eq!(recent.runs.len(), 2);
    recent.runs.retain(|run| run.key == key(1));
    let mut book = book();
    book.restore(
        ProcessHistoryLoad {
            runs: vec![old_boot.clone(), resumed, away.clone()],
            buckets: Vec::new(),
            recent: Some(recent),
        },
        now(50),
    );
    assert_eq!(
        book.run(&old_boot.key)
            .unwrap()
            .info()
            .end
            .map(|end| end.cause),
        Some(EndCause::HostRestarted)
    );
    assert!(
        !book.run(&key(1)).unwrap().series().is_empty(),
        "the short-term file's buckets resume"
    );
    observe(&mut book, 51, vec![row(1, 10.0, Some(GIB))]);
    probe(
        &mut book,
        &report(51, None, vec![reading(2, ProbeLiveness::Gone, None)]),
        now(51),
    );
    assert!(book.run(&key(1)).unwrap().is_live());
    let ended = book.run(&key(2)).unwrap().info();
    assert_eq!(ended.end.map(|end| end.cause), Some(EndCause::WhileAway));
    assert_eq!(
        ended.end.unwrap().utc_ns,
        away.last_seen_utc_ns,
        "it ended after it was last seen"
    );
}

#[test]
fn ended_runs_read_their_buckets_once_when_none_are_held() {
    let mut book = book();
    let mut info = {
        let mut writer = self::book();
        observe(&mut writer, 1, vec![row(1, 10.0, Some(GIB))]);
        writer.run(&key(1)).unwrap().info().clone()
    };
    info.end = Some(RunEnd {
        utc_ns: info.last_seen_utc_ns,
        cause: EndCause::Exit(0),
    });
    book.restore(
        ProcessHistoryLoad {
            runs: vec![info],
            ..ProcessHistoryLoad::default()
        },
        now(10),
    );
    assert!(book.wants_buckets(&key(1)));
    assert!(!book.wants_buckets(&key(1)), "asked once");
    assert!(!book.wants_buckets(&key(2)), "no such run");
    let mut bucket = RunBucket::new(0);
    bucket.add(RunMetric::Cpu, Some(10.0));
    book.on_buckets(
        &key(1),
        vec![RunBucketRow {
            key: key(1),
            tier: Tier::OneMinute,
            bucket,
        }],
    );
    let series = book.run(&key(1)).unwrap().series();
    assert_eq!(
        series.len(),
        Tier::ALL.len() - 1,
        "the minute, and each coarser size made from it"
    );
    assert_eq!(series.buckets(Tier::OneDay).count(), 1);
}

/// At most the live-run limit is followed; the listed runs are bounded,
/// the earliest ended giving way.
#[test]
fn runs_are_bounded() {
    let mut book = book();
    let pids = i64::try_from(MAX_LIVE_RUNS).unwrap() + 3;
    observe(
        &mut book,
        1,
        (1..=pids).map(|pid| row(pid, 1.0, Some(GIB))).collect(),
    );
    assert_eq!(book.runs().count(), MAX_LIVE_RUNS);
    assert_eq!(book.probe_targets().len(), MAX_LIVE_RUNS);
}

/// A restart: the run restored from the stores keeps running when the
/// tables list it, even tables cut short (a busy host), and probes that
/// say it runs.
#[test]
fn a_restored_run_listed_in_cut_tables_keeps_running() {
    let mut writer = book();
    observe(&mut writer, 1, vec![row(1, 10.0, Some(GIB))]);
    let info = writer.run(&key(1)).unwrap().info().clone();
    let recent = writer.recent(now(1));
    let mut book = book();
    book.restore(
        ProcessHistoryLoad {
            runs: vec![info],
            buckets: Vec::new(),
            recent: Some(recent),
        },
        now(100),
    );
    let labels = labels();
    for second in 101..130 {
        let mut cut = table(second, vec![row(1, 10.0, Some(GIB))]);
        cut.truncated = true;
        book.observe(
            &cut,
            &SampleInputs {
                now: now(second),
                clk_tck: Some(100),
                gpu_labels: &labels,
                host_network: [None, None],
            },
        );
        probe(
            &mut book,
            &report(second, None, vec![reading(1, ProbeLiveness::Running, None)]),
            now(second),
        );
    }
    assert_eq!(
        end_of(&book, 1),
        None,
        "{:?}",
        book.run(&key(1)).unwrap().info().end
    );
    assert!(book.run(&key(1)).unwrap().is_live());
}

/// Every committed batch hands the book the snapshot's process table; a
/// table it already sampled adds nothing.
#[test]
fn a_table_is_sampled_once() {
    let mut book = book();
    let labels = labels();
    let table = process_table(7, 5 * SECOND, vec![row(1, 10.0, Some(GIB))]);
    for _ in 0..5 {
        book.observe(
            &table,
            &SampleInputs {
                now: now(5),
                clk_tck: Some(100),
                gpu_labels: &labels,
                host_network: [None, None],
            },
        );
    }
    let bucket = book
        .run(&key(1))
        .unwrap()
        .series()
        .buckets(Tier::Recent)
        .next()
        .copied()
        .unwrap();
    assert_eq!(bucket.value(RunMetric::Cpu).count, 1);
}

/// The commits after a cycle's first hand over the same table: the book
/// says so before anyone builds its inputs, and a new cycle or read time is
/// a new table.
#[test]
fn a_sampled_table_is_known_before_its_inputs_are_built() {
    let mut book = book();
    let sampled = process_table(7, 5 * SECOND, vec![row(1, 10.0, Some(GIB))]);
    assert!(!book.has_sampled(&sampled), "nothing sampled yet");
    let labels = labels();
    book.observe(
        &sampled,
        &SampleInputs {
            now: now(5),
            clk_tck: Some(100),
            gpu_labels: &labels,
            host_network: [None, None],
        },
    );
    assert!(book.has_sampled(&sampled));
    assert!(book.has_sampled(&process_table(7, 5 * SECOND, Vec::new())));
    assert!(!book.has_sampled(&process_table(8, 5 * SECOND, Vec::new())));
    assert!(!book.has_sampled(&process_table(7, 6 * SECOND, Vec::new())));
}

/// On a busy host every table is cut short, so the table never says a run
/// is gone; the probe, which reads the process itself, does.
#[test]
fn a_probe_ends_a_run_even_when_tables_are_cut() {
    let mut book = book();
    let labels = labels();
    let mut cut = table(1, vec![row(1, 10.0, Some(GIB))]);
    cut.truncated = true;
    book.observe(
        &cut,
        &SampleInputs {
            now: now(1),
            clk_tck: Some(100),
            gpu_labels: &labels,
            host_network: [None, None],
        },
    );
    probe(
        &mut book,
        &report(2, None, vec![reading(1, ProbeLiveness::Gone, None)]),
        now(2),
    );
    assert_eq!(end_of(&book, 1), Some(EndCause::NotVisible));
}

/// The descendants come with the table row (D-79): their count, CPU and
/// memory become the run's samples; a row without them leaves no value.
#[test]
fn the_rows_descendants_become_the_samples() {
    let mut book = book();
    let mut with = row(9, 10.0, Some(GIB));
    with.descendants = Some(ProcessDescendants {
        processes: 3,
        cpu_percent: Some(150.0),
        rss_bytes: Some(GIB),
    });
    observe(&mut book, 1, vec![with]);
    observe(&mut book, 2, vec![row(9, 10.0, Some(GIB))]);
    observe(&mut book, 3, vec![row(9, 10.0, Some(GIB))]);
    let bucket = *book
        .run(&key(9))
        .unwrap()
        .series()
        .buckets(Tier::Recent)
        .next()
        .unwrap();
    assert_eq!(bucket.value(RunMetric::Children).mean(), Some(3.0));
    assert_eq!(bucket.value(RunMetric::ChildrenCpu).mean(), Some(150.0));
    #[allow(clippy::cast_precision_loss)]
    let held = GIB as f64;
    assert_eq!(bucket.value(RunMetric::ChildrenMemory).mean(), Some(held));
    assert_eq!(
        bucket.value(RunMetric::Children).count,
        1,
        "the rows without descendants add no value"
    );
}

const BUCKET_BYTES_FOR_TEST: usize = std::mem::size_of::<dgxtop_core::process_history::RunBucket>();

/// A run's whole-life figures follow every sample as its buckets close,
/// equal the compression's merge of what it holds, and stay when an ended
/// run's buckets leave memory (D-79: the list never merges buckets).
#[test]
fn whole_figures_follow_the_samples_and_outlive_the_buckets() {
    // A budget the 33 runs' buckets overflow: the first ended gives way.
    let mut book = book().with_series_budget(32 * 40 * BUCKET_BYTES_FOR_TEST);
    let pids: Vec<i64> = (1..=33).collect();
    for second in 0..130 {
        #[allow(clippy::cast_precision_loss)]
        let cpu = (second % 7) as f64 * 10.0;
        observe(
            &mut book,
            second,
            pids.iter().map(|pid| row(*pid, cpu, Some(GIB))).collect(),
        );
        let run = book.run(&key(1)).unwrap();
        assert_eq!(*run.whole(), whole_series(run.series()), "at {second} s");
    }
    let figures = *book.run(&key(1)).unwrap().whole();
    assert_eq!(figures.value(RunMetric::Cpu).count, 130);
    for (index, pid) in pids.iter().enumerate() {
        let second = 200 + u64::try_from(index).unwrap();
        probe(
            &mut book,
            &report(second, None, vec![reading(*pid, ProbeLiveness::Gone, None)]),
            now(second),
        );
    }
    let first = book.run(&key(1)).unwrap();
    assert!(first.series().is_empty(), "the run that ended first");
    assert_eq!(*first.whole(), figures, "keeps its figures");
    let last = book.run(&key(*pids.last().unwrap())).unwrap();
    assert!(!last.series().is_empty());
}
