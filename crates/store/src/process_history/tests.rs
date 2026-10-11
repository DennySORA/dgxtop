use std::path::PathBuf;
use std::sync::Arc;

use dgxtop_core::action::Signal;
use dgxtop_core::identity::{Pid, PidNamespace};
use dgxtop_core::process_history::{
    EndCause, ProcessHistoryJournal, RUN_METRICS, RecentHistory, RecentRun, RunBucket,
    RunBucketRow, RunDetails, RunEnd, RunInfo, RunMetric, Tier,
};
use dgxtop_core::testing::{FakeClock, boot_id, host_id};

use super::*;
use crate::sqlite::{SqliteGateError, SqliteVersion};

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
        Self(dir)
    }

    fn open(&self, clock: &Arc<FakeClock>) -> ProcessHistoryFiles {
        ProcessHistoryFiles::open(
            &self.0.join("process_history.sqlite3"),
            &self.0.join("process_recent.json"),
            Arc::clone(clock) as Arc<dyn TimeProvider>,
        )
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn clock() -> Arc<FakeClock> {
    FakeClock::new(1_000_000_000)
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
    let mut store = scratch.open(&clock);
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
                row(1, Tier::Recent, bucket(now, 99.0)),
                row(9, Tier::OneMinute, bucket(now, 1.0)),
            ],
        })
        .unwrap();
    store
        .append(&ProcessHistoryJournal {
            runs: vec![run(2, now - MINUTE, Some(killed))],
            buckets: Vec::new(),
        })
        .unwrap();
    drop(store);
    let mut store = scratch.open(&clock);
    let load = store.load().unwrap();
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
        "ten minutes, then minutes oldest first; no 5 s bucket, no unknown run"
    );
    let cpu = load.buckets[1].bucket.value(RunMetric::Cpu);
    assert_eq!(
        (cpu.count, cpu.mean(), cpu.peak()),
        (2, Some(20.0), Some(30.0))
    );
    assert_eq!(store.read_run(&key(1)).unwrap().len(), 3);
    assert!(store.read_run(&key(8)).unwrap().is_empty());
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
    let mut store = scratch.open(&clock);
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
        })
        .unwrap();
    let loaded: Vec<EndCause> = store
        .load()
        .unwrap()
        .runs
        .iter()
        .filter_map(|run| run.end.map(|end| end.cause))
        .collect();
    assert_eq!(loaded, causes);
}

/// No run expires by age (D-77); each size keeps a run's latest buckets.
#[test]
fn retention_keeps_runs_of_any_age_and_each_size_its_latest_buckets() {
    let scratch = Scratch::new("retention");
    let clock = clock();
    let now = now_utc(&clock);
    let day = 24 * 60 * MINUTE;
    let mut store = scratch.open(&clock);
    let minutes = i64::try_from(Tier::OneMinute.capacity()).unwrap() + 5;
    let mut buckets: Vec<RunBucketRow> = (0..minutes)
        .map(|minute| {
            row(
                1,
                Tier::OneMinute,
                bucket(now - (minutes - minute) * MINUTE, 1.0),
            )
        })
        .collect();
    buckets.push(row(1, Tier::OneDay, bucket(now - 400 * day, 1.0)));
    store
        .append(&ProcessHistoryJournal {
            runs: vec![run(1, now, None), run(2, now - 400 * day, None)],
            buckets,
        })
        .unwrap();
    drop(store);
    let mut store = scratch.open(&clock);
    let load = store.load().unwrap();
    assert_eq!(load.runs.len(), 2, "a run unseen for 400 days stays");
    let held = |tier: Tier| {
        load.buckets
            .iter()
            .filter(|row| row.key == key(1) && row.tier == tier)
            .count()
    };
    assert_eq!(held(Tier::OneMinute), Tier::OneMinute.capacity());
    assert_eq!(held(Tier::OneDay), 1);
    let first_minute = load
        .buckets
        .iter()
        .find(|row| row.tier == Tier::OneMinute)
        .unwrap();
    assert_eq!(
        first_minute.bucket.start_utc_ns,
        now - (minutes - 5) * MINUTE,
        "the oldest gave way"
    );
    assert_eq!(load.buckets[0].tier, Tier::OneDay, "coarsest first");
}

/// Near its size the database lets the ended runs last seen longest ago
/// go; live runs stay.
#[test]
fn a_full_database_lets_the_oldest_ended_runs_go() {
    let scratch = Scratch::new("full");
    let clock = clock();
    let now = now_utc(&clock);
    let ended = |pid: i64| {
        run(
            pid,
            now - (100 - pid) * MINUTE,
            Some(RunEnd {
                utc_ns: now - (100 - pid) * MINUTE,
                cause: EndCause::Exit(0),
            }),
        )
    };
    let mut store = scratch.open(&clock).with_max_used_bytes(1);
    let mut runs: Vec<RunInfo> = (1..=60).map(ended).collect();
    runs.push(run(99, now - 200 * MINUTE, None));
    store
        .append(&ProcessHistoryJournal {
            runs,
            buckets: Vec::new(),
        })
        .unwrap();
    drop(store);
    let mut store = scratch.open(&clock).with_max_used_bytes(1);
    let kept: Vec<i32> = store
        .load()
        .unwrap()
        .runs
        .iter()
        .map(|run| run.key.pid.get())
        .collect();
    assert_eq!(
        kept,
        vec![99],
        "every ended run gave way, the live one stays"
    );
}

/// The short-term file round-trips, is private, and a missing or broken
/// one is no load failure.
#[test]
fn the_short_term_file_is_replaced_atomically_and_read_back() {
    let scratch = Scratch::new("recent");
    let clock = clock();
    let mut store = scratch.open(&clock);
    assert!(store.load().unwrap().recent.is_none(), "no file yet");
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
    assert_eq!(store.load().unwrap().recent, Some(recent));
    std::fs::write(&path, b"{not json").unwrap();
    assert!(store.load().unwrap().recent.is_none());
}

#[test]
fn buckets_pack_twelve_bytes_a_metric_and_back() {
    let mut full = bucket(MINUTE, 10.0);
    full.add(RunMetric::OpenFiles, Some(12.0));
    let packed = database::pack(&full);
    assert_eq!(packed.len(), 12 * RUN_METRICS);
    let back = database::unpack(MINUTE, &packed).unwrap();
    assert_eq!(back.value(RunMetric::Cpu).mean(), Some(20.0));
    assert_eq!(back.value(RunMetric::Cpu).peak(), Some(30.0));
    assert_eq!(back.value(RunMetric::Memory).count, 0);
    assert_eq!(back.value(RunMetric::OpenFiles).peak(), Some(12.0));
    assert!(database::unpack(0, &packed[..95]).is_none());
    assert!(database::unpack(0, &[]).is_none());
    // A bucket of version 2 held the first eight metrics.
    let older = database::unpack(MINUTE, &packed[..96]).unwrap();
    assert_eq!(older.value(RunMetric::Cpu).mean(), Some(20.0));
    assert_eq!(older.value(RunMetric::OpenFiles).count, 0);
}

/// A version 2 database (D-76) is checked, backed up and upgraded: its
/// runs and buckets stay, and it takes hour buckets and the run details.
#[test]
fn a_version_2_database_upgrades_and_keeps_its_rows() {
    let scratch = Scratch::new("upgrade");
    let clock = clock();
    let now = now_utc(&clock);
    let path = scratch.0.join("process_history.sqlite3");
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(include_str!("schema_v2.sql"))
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
        let packed = database::pack(&bucket(now - 2 * MINUTE, 10.0));
        connection
            .execute(
                "INSERT INTO process_buckets (run_id, tier, start_utc_ns, aggregates) \
                 VALUES (1, 1, ?1, ?2)",
                rusqlite::params![now - 2 * MINUTE, &packed[..96]],
            )
            .unwrap();
    }
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let mut store = scratch.open(&clock);
    let load = store.load().unwrap();
    assert_eq!(load.runs.len(), 1);
    assert_eq!(load.runs[0].command, "python -m acme_llm.train");
    assert_eq!(load.runs[0].details, RunDetails::default());
    assert_eq!(load.buckets.len(), 1);
    assert_eq!(
        load.buckets[0].bucket.value(RunMetric::Cpu).mean(),
        Some(20.0)
    );
    let backups = std::fs::read_dir(&scratch.0)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".pre-v3-"))
        .count();
    assert_eq!(backups, 1, "a backup before the upgrade");
    let mut hour = bucket(Tier::OneHour.align(now, 0), 5.0);
    hour.add(RunMetric::Threads, Some(24.0));
    store
        .append(&ProcessHistoryJournal {
            runs: vec![run(1, now, None)],
            buckets: vec![row(1, Tier::OneHour, hour)],
        })
        .unwrap();
    let rows = store.read_run(&key(1)).unwrap();
    assert_eq!(rows[0].tier, Tier::OneHour);
    assert_eq!(rows[0].bucket.value(RunMetric::Threads).peak(), Some(24.0));
    drop(store);
    let version: i64 = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 3);
    let mut reopened = scratch.open(&clock);
    assert_eq!(
        reopened.load().unwrap().runs[0].details,
        run(1, 0, None).details
    );
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
    let clock = clock();
    let mut store = ProcessHistoryFiles::open_with(
        &scratch.0.join("process_history.sqlite3"),
        &scratch.0.join("process_recent.json"),
        Arc::clone(&clock) as Arc<dyn TimeProvider>,
        Box::new(RealFileSystem),
        failing_gate,
    );
    let error = store.load().unwrap_err().to_string();
    assert!(error.contains("process history database"), "{error}");
    assert!(store.append(&ProcessHistoryJournal::default()).is_err());
    let recent = RecentHistory {
        boot_id: boot_id("boot-a"),
        saved_utc_ns: 0,
        runs: Vec::new(),
    };
    assert!(store.save_recent(&recent).is_ok());
}
