use std::path::PathBuf;

use dgxtop_core::action::Signal;
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{Pid, PidNamespace};
use dgxtop_core::process_history::{
    EndCause, KeepLatest, MAX_LISTED_RUNS, ProcessHistoryJournal, RUN_METRICS, RecentHistory,
    RecentRun, RunBucket, RunBucketRow, RunDetails, RunEnd, RunInfo, RunMetric,
};
use dgxtop_core::testing::{FakeClock, boot_id, host_id};
use dgxtop_core::tiers::Tier;

use super::*;
use crate::sqlite::{
    PROCESS_HISTORY_DDL, SchemaKind, SqliteGateError, SqliteVersion, format_stamp,
};

const MINUTE: i64 = 60 * 1_000_000_000;

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "dgxtop-process-history-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Like the real state directory: a file is recreated only in a
        // private one (D-85).
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        Self(dir)
    }

    fn open(&self) -> ProcessHistoryFiles {
        ProcessHistoryFiles::open(
            &self.0.join("process_history.sqlite3"),
            &self.0.join("process_recent.json"),
        )
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn clock() -> std::sync::Arc<FakeClock> {
    FakeClock::new(1_000_000_000)
}

/// Of every stored size, coarsest first, as many as a size keeps.
fn every_size() -> BucketRequest {
    BucketRequest {
        latest: Tier::ALL
            .into_iter()
            .skip(1)
            .rev()
            .map(|tier| (tier, tier.capacity()))
            .collect(),
    }
}

/// What the history asks for at startup.
fn startup() -> ProcessHistoryLoadRequest {
    ProcessHistoryLoadRequest {
        runs: MAX_LISTED_RUNS,
        ended_runs_with_buckets: 32,
        buckets: every_size(),
    }
}

fn now_utc(clock: &FakeClock) -> i64 {
    i64::try_from(clock.now_utc_ns()).unwrap()
}

fn key(pid: i64) -> ProcessKey {
    ProcessKey {
        host_id: host_id("host-a"),
        boot_id: boot_id("boot-a"),
        pid_namespace: PidNamespace::new("pid:[4026531836]").unwrap(),
        pid: Pid::new(pid).unwrap(),
        start_time_ticks: 700,
    }
}

fn run(pid: i64, last_seen_utc_ns: i64, end: Option<RunEnd>) -> RunInfo {
    RunInfo {
        key: key(pid),
        name: "acme_llm.train".to_owned(),
        command: "python -m acme_llm.train --epochs 3".to_owned(),
        cwd: "/home/u/code/acme-llm".to_owned(),
        uid: Some(1000),
        gpus: vec!["GPU 0".to_owned(), "GPU 1".to_owned()],
        started_utc_ns: last_seen_utc_ns - 30 * MINUTE,
        last_seen_utc_ns,
        end,
        details: RunDetails {
            parent_pid: Some(7),
            parent_name: "torchrun".to_owned(),
            executable: "/venv/bin/python3.12".to_owned(),
            cgroup: "/user.slice/user-1000.slice/session-3.scope".to_owned(),
            nice: Some(-5),
        },
    }
}

fn bucket(start_utc_ns: i64, cpu: f64) -> RunBucket {
    let mut bucket = RunBucket::new(start_utc_ns);
    bucket.add(RunMetric::Cpu, Some(cpu));
    bucket.add(RunMetric::Cpu, Some(cpu * 3.0));
    bucket.add(RunMetric::GpuMemory, Some(64.0 * 1024.0 * 1024.0 * 1024.0));
    bucket
}

fn row(pid: i64, tier: Tier, bucket: RunBucket) -> RunBucketRow {
    RunBucketRow {
        key: key(pid),
        tier,
        bucket,
    }
}

/// Runs and buckets written come back at the next start, ends with their
/// causes, and a run written again is updated in place.
#[test]
fn runs_and_buckets_survive_a_restart() {
    let scratch = Scratch::new("restart");
    let clock = clock();
    let now = now_utc(&clock);
    let mut store = scratch.open();
    let killed = RunEnd {
        utc_ns: now - MINUTE,
        cause: EndCause::SignaledByDgxtop(Signal::Sigkill),
    };
    store
        .append(&ProcessHistoryJournal {
            runs: vec![run(1, now - 2 * MINUTE, None), run(2, now - MINUTE, None)],
            buckets: vec![
                row(1, Tier::OneMinute, bucket(now - 3 * MINUTE, 10.0)),
                row(1, Tier::OneMinute, bucket(now - 2 * MINUTE, 20.0)),
                row(1, Tier::TenMinutes, bucket(now - 10 * MINUTE, 15.0)),
                row(9, Tier::OneMinute, bucket(now, 1.0)),
            ],
            ..ProcessHistoryJournal::default()
        })
        .unwrap();
    store
        .append(&ProcessHistoryJournal {
            runs: vec![run(2, now - MINUTE, Some(killed))],
            buckets: Vec::new(),
            ..ProcessHistoryJournal::default()
        })
        .unwrap();
    drop(store);
    let mut store = scratch.open();
    let load = store.load(&startup()).unwrap();
    assert_eq!(load.runs.len(), 2);
    assert_eq!(load.runs[0].key, key(2), "the last seen first");
    assert_eq!(load.runs[0].end, Some(killed));
    let first = &load.runs[1];
    assert_eq!(first.gpus, vec!["GPU 0".to_owned(), "GPU 1".to_owned()]);
    assert_eq!(first.command, "python -m acme_llm.train --epochs 3");
    assert_eq!(first.cwd, "/home/u/code/acme-llm");
    assert_eq!(first.uid, Some(1000));
    assert_eq!(first.details, run(1, 0, None).details);
    let tiers: Vec<(Tier, i64)> = load
        .buckets
        .iter()
        .map(|row| (row.tier, (row.bucket.start_utc_ns - now) / MINUTE))
        .collect();
    assert_eq!(
        tiers,
        vec![
            (Tier::TenMinutes, -10),
            (Tier::OneMinute, -3),
            (Tier::OneMinute, -2)
        ],
        "ten minutes, then minutes oldest first; no unknown run"
    );
    let cpu = load.buckets[1].bucket.value(RunMetric::Cpu);
    assert_eq!(
        (cpu.count, cpu.mean(), cpu.peak()),
        (2, Some(20.0), Some(30.0))
    );
    assert_eq!(store.read_run(&key(1), &every_size()).unwrap().len(), 3);
    assert!(store.read_run(&key(8), &every_size()).unwrap().is_empty());
}

/// Every end cause round-trips through its columns.
#[test]
fn every_end_cause_is_stored() {
    let scratch = Scratch::new("causes");
    let clock = clock();
    let now = now_utc(&clock);
    let causes = [
        EndCause::SignaledByDgxtop(Signal::Sigterm),
        EndCause::SignaledByDgxtop(Signal::Sigkill),
        EndCause::Signal(9),
        EndCause::Exit(2),
        EndCause::OutOfMemory,
        EndCause::NotVisible,
        EndCause::WhileAway,
        EndCause::HostRestarted,
    ];
    let mut store = scratch.open();
    let runs: Vec<RunInfo> = causes
        .iter()
        .zip(1..)
        .map(|(cause, pid)| {
            run(
                pid,
                now - pid * MINUTE,
                Some(RunEnd {
                    utc_ns: now,
                    cause: *cause,
                }),
            )
        })
        .collect();
    store
        .append(&ProcessHistoryJournal {
            runs,
            buckets: Vec::new(),
            ..ProcessHistoryJournal::default()
        })
        .unwrap();
    let loaded: Vec<EndCause> = store
        .load(&startup())
        .unwrap()
        .runs
        .iter()
        .filter_map(|run| run.end.map(|end| end.cause))
        .collect();
    assert_eq!(loaded, causes);
}

/// The store deletes nothing on its own (D-94): runs of any age and every
/// bucket written stay until a journal names them.
#[test]
fn nothing_is_deleted_unless_a_journal_says_so() {
    let scratch = Scratch::new("no-policy");
    let clock = clock();
    let now = now_utc(&clock);
    let day = 24 * 60 * MINUTE;
    let mut store = scratch.open();
    let minutes = i64::try_from(Tier::OneMinute.capacity()).unwrap() + 5;
    let buckets: Vec<RunBucketRow> = (0..minutes)
        .map(|minute| {
            row(
                1,
                Tier::OneMinute,
                bucket(now - (minutes - minute) * MINUTE, 1.0),
            )
        })
        .collect();
    let ended = |pid: i64| {
        run(
            pid,
            now - 400 * day + pid * MINUTE,
            Some(RunEnd {
                utc_ns: now - 400 * day,
                cause: EndCause::Exit(0),
            }),
        )
    };
    let mut runs = vec![run(1, now, None)];
    runs.extend((2..=60).map(ended));
    let usage = store
        .append(&ProcessHistoryJournal {
            runs,
            buckets,
            ..ProcessHistoryJournal::default()
        })
        .unwrap();
    assert_eq!(usage.runs, 60);
    assert!(usage.used_bytes > 0);
    let everything = BucketRequest {
        latest: vec![(Tier::OneMinute, usize::MAX)],
    };
    let held = store.read_run(&key(1), &everything).unwrap();
    assert_eq!(held.len(), usize::try_from(minutes).unwrap(), "all kept");
    drop(store);
    let mut store = scratch.open();
    assert_eq!(store.load(&startup()).unwrap().runs.len(), 60);
}

/// A journal's deletions apply after its writes: each size it names keeps
/// its latest buckets, the runs it forgets go with their buckets, and past
/// the runs it keeps the ones last seen longest ago go. The usage it
/// returns counts what is left.
#[test]
fn a_journal_deletes_what_it_names() {
    let scratch = Scratch::new("journal-deletes");
    let clock = clock();
    let now = now_utc(&clock);
    let mut store = scratch.open();
    let runs: Vec<RunInfo> = (1..=5)
        .map(|pid| run(pid, now - pid * MINUTE, None))
        .collect();
    let buckets: Vec<RunBucketRow> = (0..10)
        .map(|minute| {
            row(
                1,
                Tier::OneMinute,
                bucket(now - (10 - minute) * MINUTE, 1.0),
            )
        })
        .chain([row(2, Tier::OneMinute, bucket(now - MINUTE, 2.0))])
        .collect();
    let before = store
        .append(&ProcessHistoryJournal {
            runs,
            buckets,
            ..ProcessHistoryJournal::default()
        })
        .unwrap();
    assert_eq!(before.runs, 5);
    let after = store
        .append(&ProcessHistoryJournal {
            keep_latest: vec![KeepLatest {
                key: key(1),
                tier: Tier::OneMinute,
                buckets: 3,
            }],
            forget: vec![key(2), key(77)],
            keep_runs: Some(2),
            ..ProcessHistoryJournal::default()
        })
        .unwrap();
    assert_eq!(
        after.runs, 2,
        "run 2 forgotten, then the two last seen kept"
    );
    let load = store.load(&startup()).unwrap();
    let kept: Vec<i32> = load.runs.iter().map(|run| run.key.pid.get()).collect();
    assert_eq!(kept, vec![1, 3]);
    let minutes: Vec<i64> = load
        .buckets
        .iter()
        .map(|row| (row.bucket.start_utc_ns - now) / MINUTE)
        .collect();
    assert_eq!(minutes, vec![-3, -2, -1], "run 1 keeps its latest three");
    assert!(store.read_run(&key(2), &every_size()).unwrap().is_empty());
}

/// The load reads what its request asks: the runs last seen up to its
/// count, buckets of the unfinished runs and of the ended ones it asks
/// for, of the sizes it names and as many of each.
#[test]
fn the_load_reads_what_its_request_asks() {
    let scratch = Scratch::new("load-request");
    let clock = clock();
    let now = now_utc(&clock);
    let mut store = scratch.open();
    let ended = |pid: i64| {
        run(
            pid,
            now - pid * MINUTE,
            Some(RunEnd {
                utc_ns: now - pid * MINUTE,
                cause: EndCause::Exit(0),
            }),
        )
    };
    let mut buckets = Vec::new();
    for pid in 1..=4 {
        for minute in 1..=3 {
            buckets.push(row(
                pid,
                Tier::OneMinute,
                bucket(now - minute * MINUTE, 1.0),
            ));
        }
        buckets.push(row(pid, Tier::OneHour, bucket(now - 60 * MINUTE, 1.0)));
    }
    store
        .append(&ProcessHistoryJournal {
            runs: vec![run(4, now - 4 * MINUTE, None), ended(1), ended(2), ended(3)],
            buckets,
            ..ProcessHistoryJournal::default()
        })
        .unwrap();
    let load = store
        .load(&ProcessHistoryLoadRequest {
            runs: 3,
            ended_runs_with_buckets: 1,
            buckets: BucketRequest {
                latest: vec![(Tier::OneMinute, 2)],
            },
        })
        .unwrap();
    let listed: Vec<i32> = load.runs.iter().map(|run| run.key.pid.get()).collect();
    assert_eq!(listed, vec![1, 2, 3], "the three last seen");
    let with_buckets: Vec<(i32, Tier)> = load
        .buckets
        .iter()
        .map(|row| (row.key.pid.get(), row.tier))
        .collect();
    assert_eq!(
        with_buckets,
        vec![(1, Tier::OneMinute), (1, Tier::OneMinute)],
        "only the latest ended run, two minutes, no hour"
    );
}

/// The short-term file round-trips, is private, and a missing or broken
/// one is no load failure.
#[test]
fn the_short_term_file_is_replaced_atomically_and_read_back() {
    let scratch = Scratch::new("recent");
    let clock = clock();
    let mut store = scratch.open();
    assert!(
        store.load(&startup()).unwrap().recent.is_none(),
        "no file yet"
    );
    let recent = RecentHistory {
        boot_id: boot_id("boot-a"),
        saved_utc_ns: now_utc(&clock),
        runs: vec![RecentRun {
            key: key(1),
            recent: vec![bucket(0, 5.0)],
            filling: vec![(Tier::OneMinute, bucket(0, 5.0))],
        }],
    };
    store.save_recent(&recent).unwrap();
    store.save_recent(&recent).unwrap();
    let path = scratch.0.join("process_recent.json");
    let mode =
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&path).unwrap().permissions());
    assert_eq!(mode & 0o777, 0o600);
    let leftovers = std::fs::read_dir(&scratch.0)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with('.'))
        .count();
    assert_eq!(leftovers, 0, "no temporary file stays behind");
    assert_eq!(store.load(&startup()).unwrap().recent, Some(recent));
    std::fs::write(&path, b"{not json").unwrap();
    assert!(store.load(&startup()).unwrap().recent.is_none());
}

/// A bucket packs 12 bytes a metric, through its last metric with a
/// sample, so a run on one GPU leaves the other GPUs' figures out (D-120).
/// A bucket packed before metrics were added reads with them empty; one of
/// more metrics than this version knows does not read (its load fails, and
/// nothing is deleted).
#[test]
fn buckets_pack_twelve_bytes_a_metric_and_back() {
    let mut full = bucket(MINUTE, 10.0);
    full.add(RunMetric::OpenFiles, Some(12.0));
    let packed = database::pack(&full);
    assert_eq!(packed.len(), 12 * (RunMetric::OpenFiles.index() + 1));
    let back = database::unpack(MINUTE, &packed).unwrap();
    assert_eq!(back.value(RunMetric::Cpu).mean(), Some(20.0));
    assert_eq!(back.value(RunMetric::Cpu).peak(), Some(30.0));
    assert_eq!(back.value(RunMetric::Memory).count, 0);
    assert_eq!(back.value(RunMetric::OpenFiles).peak(), Some(12.0));
    assert_eq!(back.value(RunMetric::Gpu0Memory).count, 0, "read empty");
    full.add(RunMetric::Gpu1Device, Some(40.0));
    let with_gpus = database::pack(&full);
    assert_eq!(with_gpus.len(), 12 * (RunMetric::Gpu1Device.index() + 1));
    let back = database::unpack(MINUTE, &with_gpus).unwrap();
    assert_eq!(back.value(RunMetric::Gpu1Device).mean(), Some(40.0));
    assert!(database::unpack(0, &packed[..95]).is_none());
    assert!(database::unpack(0, &[]).is_none());
    let mut longer = with_gpus.clone();
    longer.resize(12 * (RUN_METRICS + 1), 0);
    assert!(database::unpack(MINUTE, &longer).is_none());
}

/// Writes a database of this schema holding one run, stamped with
/// `application_id` (0: written before format versions existed).
fn write_database_of_format(path: &std::path::Path, application_id: i32, clock: &FakeClock) {
    let now = now_utc(clock);
    let connection = rusqlite::Connection::open(path).unwrap();
    connection.execute_batch(PROCESS_HISTORY_DDL).unwrap();
    connection
        .pragma_update(None, "application_id", application_id)
        .unwrap();
    connection
        .execute(
            "INSERT INTO process_runs (host_id, boot_id, pid_namespace, pid, \
             start_time_ticks, name, command, cwd, uid, gpus, started_utc_ns, \
             last_seen_utc_ns) VALUES ('host-a', 'boot-a', 'pid:[4026531836]', 1, 700, \
             'acme_llm.train', 'python -m acme_llm.train', '/srv', 1000, 'GPU 0', ?1, ?2)",
            rusqlite::params![now - 30 * MINUTE, now - MINUTE],
        )
        .unwrap();
    drop(connection);
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
}

/// A database of another format version, older, newer or none (one
/// written before D-85), is deleted and created afresh, empty, without a
/// backup; one of this format keeps its runs (D-85).
#[test]
fn a_database_of_another_format_is_replaced_by_an_empty_one() {
    let clock = clock();
    let current = SchemaKind::ProcessHistory.format_version();
    for (label, application_id, kept) in [
        ("same", format_stamp(current), true),
        ("older", format_stamp(current - 1), false),
        ("newer", format_stamp(current + 1), false),
        ("missing", 0, false),
    ] {
        let scratch = Scratch::new(label);
        let path = scratch.0.join("process_history.sqlite3");
        write_database_of_format(&path, application_id, &clock);
        let mut store = scratch.open();
        assert_eq!(
            store.load(&startup()).unwrap().runs.len(),
            usize::from(kept),
            "{label}"
        );
        assert_eq!(store.recreated().is_none(), kept, "{label}");
        drop(store);
        let stamp: i32 = rusqlite::Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA application_id", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stamp, format_stamp(current), "{label}");
        let others: Vec<String> = std::fs::read_dir(&scratch.0)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".bak"))
            .collect();
        assert!(others.is_empty(), "no backup: {others:?}");
    }
}

/// A file that is no database at all is deleted and created afresh, and
/// the store works at once.
#[test]
fn an_unreadable_database_is_replaced() {
    let scratch = Scratch::new("garbage");
    let clock = clock();
    let path = scratch.0.join("process_history.sqlite3");
    std::fs::write(&path, vec![0x42u8; 8192]).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let mut store = scratch.open();
    assert!(
        store.recreated().unwrap().contains("schema inspection"),
        "{:?}",
        store.recreated()
    );
    let now = now_utc(&clock);
    store
        .append(&ProcessHistoryJournal {
            runs: vec![run(1, now, None)],
            buckets: vec![row(1, Tier::OneMinute, bucket(now - MINUTE, 10.0))],
            ..ProcessHistoryJournal::default()
        })
        .unwrap();
    assert_eq!(store.load(&startup()).unwrap().runs.len(), 1);
}

fn failing_gate() -> Result<SqliteVersion, SqliteGateError> {
    Err(SqliteGateError::TooOld {
        found: 3_000_000,
        required: 3_051_003,
    })
}

/// An unusable database fails every call with its reason; the short-term
/// file still works.
#[test]
fn an_unusable_database_reports_why() {
    let scratch = Scratch::new("unusable");
    let mut store = ProcessHistoryFiles::open_with(
        &scratch.0.join("process_history.sqlite3"),
        &scratch.0.join("process_recent.json"),
        Box::new(RealFileSystem),
        failing_gate,
    );
    let error = store.load(&startup()).unwrap_err().to_string();
    assert!(error.contains("process history database"), "{error}");
    assert!(store.append(&ProcessHistoryJournal::default()).is_err());
    let recent = RecentHistory {
        boot_id: boot_id("boot-a"),
        saved_utc_ns: 0,
        runs: Vec::new(),
    };
    assert!(store.save_recent(&recent).is_ok());
}

/// A short-term file of another format is ignored at load and simply
/// replaced by the next save (D-85): no migration, nothing kept aside.
#[test]
fn a_short_term_file_of_another_format_is_replaced() {
    let scratch = Scratch::new("recent-format");
    let clock = clock();
    let path = scratch.0.join("process_recent.json");
    std::fs::write(&path, br#"{"version":1,"runs":[["an","older","layout"]]}"#).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let mut store = scratch.open();
    assert!(store.load(&startup()).unwrap().recent.is_none());
    let recent = RecentHistory {
        boot_id: boot_id("boot-a"),
        saved_utc_ns: now_utc(&clock),
        runs: Vec::new(),
    };
    store.save_recent(&recent).unwrap();
    assert_eq!(store.load(&startup()).unwrap().recent, Some(recent));
    let names: Vec<String> = std::fs::read_dir(&scratch.0)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("process_recent"))
        .collect();
    assert_eq!(
        names,
        vec!["process_recent.json".to_owned()],
        "nothing kept aside"
    );
}

/// Caps the database at the pages it has, so the next growth is
/// `SQLITE_FULL`.
fn fill_up(connection: &rusqlite::Connection) {
    let _cap: i64 = connection
        .query_row("PRAGMA max_page_count = 1", [], |row| row.get(0))
        .unwrap();
}

/// A full database is a full disk to the history, like the archive's
/// (one SQLite mapping for every partition): the page cap is the store's
/// only bound, and reaching it deletes nothing (D-94).
#[test]
fn a_full_database_reports_disk_full() {
    let scratch = Scratch::new("disk-full");
    let clock = clock();
    let now = now_utc(&clock);
    let mut store = scratch.open();
    let ended = RunEnd {
        utc_ns: now - MINUTE,
        cause: EndCause::Exit(0),
    };
    store
        .append(&ProcessHistoryJournal {
            runs: vec![run(500, now - MINUTE, Some(ended))],
            ..ProcessHistoryJournal::default()
        })
        .unwrap();
    fill_up(store.connection().unwrap());
    let runs = (1..=200).map(|pid| run(pid, now, None)).collect();
    let error = store
        .append(&ProcessHistoryJournal {
            runs,
            buckets: Vec::new(),
            ..ProcessHistoryJournal::default()
        })
        .unwrap_err();
    assert!(matches!(error, InfraError::DiskFull { .. }), "{error:?}");
    let kept: Vec<i32> = store
        .load(&startup())
        .unwrap()
        .runs
        .iter()
        .map(|run| run.key.pid.get())
        .collect();
    assert_eq!(kept, vec![500], "the ended run did not give way");
}

/// A stored bucket this format never writes is reported, not dropped: the
/// load fails with what is wrong, and the database keeps working.
#[test]
fn an_undecodable_bucket_fails_the_load() {
    let scratch = Scratch::new("bad-bucket");
    let clock = clock();
    let now = now_utc(&clock);
    let mut store = scratch.open();
    store
        .append(&ProcessHistoryJournal {
            runs: vec![run(1, now, None)],
            buckets: vec![row(1, Tier::OneMinute, bucket(now - MINUTE, 10.0))],
            ..ProcessHistoryJournal::default()
        })
        .unwrap();
    // One metric more than this version knows: a newer version's bucket.
    let newer = 12 * (RUN_METRICS + 1);
    store
        .connection()
        .unwrap()
        .execute(
            &format!("UPDATE process_buckets SET aggregates = zeroblob({newer})"),
            [],
        )
        .unwrap();
    let error = store.load(&startup()).unwrap_err();
    assert!(matches!(error, InfraError::Database(_)), "{error:?}");
    assert!(
        error.to_string().contains(&format!("{newer} bytes")),
        "{error}"
    );
    let error = store.read_run(&key(1), &every_size()).unwrap_err();
    assert!(
        error.to_string().contains(&format!("{newer} bytes")),
        "{error}"
    );
    store
        .append(&ProcessHistoryJournal {
            runs: vec![run(1, now, None)],
            buckets: vec![row(1, Tier::OneMinute, bucket(now - MINUTE, 10.0))],
            ..ProcessHistoryJournal::default()
        })
        .unwrap();
    assert_eq!(
        store.load(&startup()).unwrap().buckets.len(),
        1,
        "rewritten, read again"
    );
}

/// Corruption stops the database like every partition's (sqlite::Writer):
/// later calls say why, and the short-term file still works.
#[test]
fn a_corrupt_database_stops_with_its_reason() {
    let scratch = Scratch::new("corrupt");
    let mut store = scratch.open();
    let corrupt = store
        .with_database(|_| -> Result<(), DatabaseFailure> {
            Err(DatabaseFailure::Statement {
                operation: "process_history.load",
                error: rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
                    None,
                ),
            })
        })
        .unwrap_err();
    assert!(matches!(corrupt, InfraError::Database(_)), "{corrupt:?}");
    let error = store.load(&startup()).unwrap_err();
    assert!(matches!(error, InfraError::Unavailable(_)), "{error:?}");
    assert!(
        error.to_string().contains("process history stopped"),
        "{error}"
    );
    let recent = RecentHistory {
        boot_id: boot_id("boot-a"),
        saved_utc_ns: 0,
        runs: Vec::new(),
    };
    assert!(store.save_recent(&recent).is_ok());
}

/// Relative paths are refused like the archive's and the audit's: the
/// database is unusable and says why, the short-term file is never
/// written, and nothing is created anywhere.
#[test]
fn relative_paths_are_refused() {
    let scratch = Scratch::new("relative");
    let missing = format!("dgxtop-relative-{}", uuid::Uuid::new_v4().simple());
    let relative_database = PathBuf::from(&missing).join("process_history.sqlite3");
    let mut store =
        ProcessHistoryFiles::open(&relative_database, &scratch.0.join("process_recent.json"));
    let error = store.load(&startup()).unwrap_err().to_string();
    assert!(error.contains("must be absolute"), "{error}");
    let relative_recent = PathBuf::from(&missing).join("process_recent.json");
    let mut store =
        ProcessHistoryFiles::open(&scratch.0.join("process_history.sqlite3"), &relative_recent);
    let recent = RecentHistory {
        boot_id: boot_id("boot-a"),
        saved_utc_ns: 0,
        runs: Vec::new(),
    };
    let error = store.save_recent(&recent).unwrap_err().to_string();
    assert!(error.contains("must be absolute"), "{error}");
    assert!(!PathBuf::from(&missing).exists(), "nothing is created");
}
