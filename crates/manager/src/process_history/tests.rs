use std::collections::BTreeMap;

use dgxtop_core::action::Signal;
use dgxtop_core::canonical::{ProcessRow, ProcessTable};
use dgxtop_core::identity::{EntityId, ProcessKey};
use dgxtop_core::process_history::{EndCause, ProcessHistoryLoad, RunBucket, RunMetric, Tier};
use dgxtop_core::process_probe::{
    ExitStatus, IoCounters, ProbeLiveness, ProbeReading, ProbeReport,
};
use dgxtop_core::testing::boot_id;

use super::*;
use crate::test_support::{gpu_process, process_table, test_process_key};

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
        key: key(pid),
        liveness,
        command: None,
        cwd: None,
        io: io.map(|(read_bytes, write_bytes)| IoCounters {
            read_bytes,
            write_bytes,
        }),
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
        .filter(|row| row.tier == Tier::Minute)
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

/// The probe's counters become disk rates of the next samples; its texts
/// replace the table's short command line once.
#[test]
fn probes_give_disk_rates_and_the_full_command() {
    let mut book = book();
    observe(&mut book, 1, vec![row(9, 10.0, Some(GIB))]);
    let targets = book.probe_targets();
    assert_eq!(targets.len(), 1);
    assert!(targets[0].with_text);
    let mut first = reading(9, ProbeLiveness::Running, Some((1_000, 0)));
    first.command = Some("python -m acme_llm.train --epochs 3 --data /srv/acme".to_owned());
    first.cwd = Some("/home/u/code/acme-llm".to_owned());
    book.on_probe(&report(1, Some(0), vec![first]), now(1));
    book.on_probe(
        &report(
            3,
            Some(0),
            vec![reading(9, ProbeLiveness::Running, Some((5_000, 2_000)))],
        ),
        now(3),
    );
    let run = book.run(&key(9)).unwrap();
    assert_eq!(run.disk_rates(), [Some(2_000.0), Some(1_000.0)]);
    assert_eq!(run.info().cwd, "/home/u/code/acme-llm");
    assert!(run.info().command.ends_with("--data /srv/acme"));
    assert!(!book.probe_targets()[0].with_text, "texts are read once");
    observe(&mut book, 4, vec![row(9, 10.0, Some(GIB))]);
    let recent: Vec<&RunBucket> = book
        .run(&key(9))
        .unwrap()
        .series()
        .buckets(Tier::Recent)
        .collect();
    assert_eq!(
        recent.last().unwrap().value(RunMetric::DiskRead).mean(),
        Some(2_000.0)
    );
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
    book.on_probe(&report(1, Some(4), Vec::new()), now(1));
    book.note_signal(&key(1), Signal::Sigterm, now(1));
    observe(&mut book, 2, Vec::new());
    assert_eq!(
        book.probe_targets().len(),
        5,
        "the runs the table lost are probed"
    );
    book.on_probe(
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
    book.on_probe(
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
        journal.buckets.iter().any(|row| row.tier == Tier::Minute),
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
    book.on_probe(
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
    book.on_probe(
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
    book.on_probe(
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
    book.on_probe(
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
            tier: Tier::Minute,
            bucket,
        }],
    );
    assert_eq!(book.run(&key(1)).unwrap().series().len(), 1);
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
        book.on_probe(
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
    book.on_probe(
        &report(2, None, vec![reading(1, ProbeLiveness::Gone, None)]),
        now(2),
    );
    assert_eq!(end_of(&book, 1), Some(EndCause::NotVisible));
}
