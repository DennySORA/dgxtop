//! Optional history archive: bounded batch inserts, exact TEXT encodings,
//! 20-digit key ordering (AT27 key part), the deletions a journal asks for
//! (expiry with cascade, eviction when full; D-96) and nothing else, and
//! ENOSPC/budget/WAL failures that leave the caller running (AT06).

// Test helpers panic on setup failures; the lints target production code.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dgxtop_core::action::{PersistenceKind, PersistenceState};
use dgxtop_core::canonical::GapRecord;
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::errors::InfraError;
use dgxtop_core::history::{HistoryMethod, SeriesInEveryBoot};
use dgxtop_core::metric::CanonicalUnit;
use dgxtop_core::overview_history::ArchivePrune;
use dgxtop_core::ports::{
    ArchiveExpiry, ArchiveJournal, ArchiveSink, ArchiveWhenFull, ArchivedBridge, ArchivedBucket,
    ArchivedSegment,
};
use dgxtop_core::quality::SampleKind;
use dgxtop_core::testing::{FakeClock, metric_key, source_id};
use dgxtop_core::wire::parse_fixed_width;
use dgxtop_store::archive::{ArchiveLimits, SqliteArchive};
use dgxtop_store::sqlite::{FreeSpaceProbe, StorageFootprint, format_stamp, format_utc_rfc3339};
use rusqlite::Connection;
use uuid::Uuid;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;
const MINUTE_NS: u64 = 60_000_000_000;
const DAY_NS: i128 = 86_400 * 1_000_000_000;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "dgxtop-archive-it-{label}-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).unwrap();
        // Like the real state directory: a file is recreated only in a
        // private one (D-85).
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }

    fn database(&self) -> PathBuf {
        self.0.join("history.sqlite3")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug)]
struct FakeFreeSpace(AtomicU64);

impl FreeSpaceProbe for FakeFreeSpace {
    fn available_bytes(&self, _path: &Path) -> Result<u64, InfraError> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

struct Fixture {
    dir: TempDir,
    clock: Arc<FakeClock>,
    free: Arc<FakeFreeSpace>,
}

impl Fixture {
    fn new(label: &str) -> Self {
        Self {
            dir: TempDir::new(label),
            clock: FakeClock::new(10 * MINUTE_NS),
            free: Arc::new(FakeFreeSpace(AtomicU64::new(100 * 1024 * MIB))),
        }
    }

    fn open(&self, limits: ArchiveLimits) -> SqliteArchive {
        SqliteArchive::open(
            &self.dir.database(),
            limits,
            self.clock.clone(),
            self.free.clone(),
        )
        .unwrap()
    }

    fn utc_days_ago(&self, days: i128) -> String {
        format_utc_rfc3339(self.clock.now_utc_ns() - days * DAY_NS)
    }

    /// What the history asks for with a seven-day age limit (D-96).
    fn seven_day_expiry(&self) -> ArchiveExpiry {
        let week_ns = 7 * u64::try_from(DAY_NS).unwrap();
        ArchiveExpiry {
            before_utc_ns: i64::try_from(self.clock.now_utc_ns()).unwrap()
                - i64::try_from(week_ns).unwrap(),
            current_boot_before_ns: self.clock.now_boot_ns().saturating_sub(week_ns),
        }
    }

    fn reader(&self) -> Connection {
        Connection::open_with_flags(
            self.dir.database(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap()
    }
}

fn segment(segment_id: &str, started_utc: &str) -> ArchivedSegment {
    ArchivedSegment {
        segment_id: segment_id.to_owned(),
        key: metric_key("gpu:GPU-1", "gpu.temperature", "gpu"),
        source_id: source_id("nvml:GPU-1:temp"),
        source_generation: 1,
        policy_revision: 1,
        unit: CanonicalUnit::DegC,
        sample_kind: SampleKind::Measured,
        started_utc: started_utc.to_owned(),
    }
}

fn bucket(segment_id: &str, start_boot_ns: u64, start_utc: &str, end_utc: &str) -> ArchivedBucket {
    ArchivedBucket {
        segment_id: segment_id.to_owned(),
        start_boot_ns,
        end_boot_ns: start_boot_ns + MINUTE_NS,
        start_utc: start_utc.to_owned(),
        end_utc: end_utc.to_owned(),
        covered_ns: MINUTE_NS,
        weighted_sum: Some(68.5 * 60.0),
        minimum: Some(66.0),
        maximum: Some(73.25),
        counter_delta: None,
        sample_count: 60,
        method: HistoryMethod::TimeWeightedStep,
        reason_code: None,
    }
}

fn journal(
    revision: u64,
    segments: Vec<ArchivedSegment>,
    buckets: Vec<ArchivedBucket>,
) -> ArchiveJournal {
    ArchiveJournal {
        revision,
        host_id: "test-host".to_owned(),
        boot_id: "fake-boot".to_owned(),
        segments,
        buckets,
        bridges: Vec::new(),
        gaps: Vec::new(),
        prunes: Vec::new(),
        expire: None,
        when_full: ArchiveWhenFull::EvictOldest,
        permit: None,
    }
}

fn count(connection: &Connection, table: &str) -> i64 {
    connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn minute_utc(fixture: &Fixture, minutes_ago: i128) -> String {
    format_utc_rfc3339(fixture.clock.now_utc_ns() - minutes_ago * 60_000_000_000)
}

#[test]
fn batch_insert_writes_every_table_in_bounded_transactions() {
    let fixture = Fixture::new("batch");
    let limits = ArchiveLimits {
        batch_rows: 4,
        ..ArchiveLimits::default()
    };
    let mut archive = fixture.open(limits);
    let now = minute_utc(&fixture, 0);
    let buckets: Vec<ArchivedBucket> = (0..10u64)
        .map(|index| bucket("seg-a", index * MINUTE_NS, &now, &now))
        .collect();
    let mut entry = journal(7, vec![segment("seg-a", &now)], buckets);
    entry.bridges.push(ArchivedBridge {
        segment_id: "seg-a".to_owned(),
        start_boot_ns: MINUTE_NS - 500_000_000,
        end_boot_ns: MINUTE_NS + 500_000_000,
        delta: 300,
    });
    entry.gaps.push(GapRecord {
        key: Some(metric_key("gpu:GPU-1", "gpu.temperature", "gpu")),
        adapter_id: None,
        start_boot_ns: 11 * MINUTE_NS,
        end_boot_ns: 12 * MINUTE_NS,
        reason: "timeout".to_owned(),
        affected_count: 1,
    });
    let receipt = archive.append(entry).unwrap();
    assert_eq!(receipt.revision, 7);
    assert_eq!(receipt.rows, 13);
    let stats = archive.stats();
    assert_eq!(stats.rows_written, 13);
    assert_eq!(stats.transactions, 4, "13 rows in batches of at most 4");
    let reader = fixture.reader();
    assert_eq!(count(&reader, "history_segments"), 1);
    assert_eq!(count(&reader, "history_buckets"), 10);
    assert_eq!(count(&reader, "counter_bridges"), 1);
    assert_eq!(count(&reader, "history_gaps"), 1);
    let (covered, sum_type, method): (String, String, String) = reader
        .query_row(
            "SELECT covered_ns, typeof(weighted_sum), method FROM history_buckets LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(covered, "00000000060000000000");
    assert_eq!(sum_type, "real");
    assert_eq!(method, "time_weighted_step");
    let journal_mode: String = reader
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal_mode, "wal");
    let status = archive.status();
    assert_eq!(status.kind, PersistenceKind::Archive);
    assert_eq!(status.state, PersistenceState::Healthy);
    assert_eq!(status.bytes_limit, 1024 * MIB);
    assert_eq!(
        status.bytes_used,
        StorageFootprint::measure(&fixture.dir.database())
            .unwrap()
            .total()
    );
}

#[test]
fn re_appending_a_journal_is_idempotent() {
    let fixture = Fixture::new("idempotent");
    let mut archive = fixture.open(ArchiveLimits::default());
    let now = minute_utc(&fixture, 0);
    let make = || {
        journal(
            1,
            vec![segment("seg-a", &now)],
            vec![bucket("seg-a", 0, &now, &now)],
        )
    };
    assert_eq!(archive.append(make()).unwrap().rows, 2);
    assert_eq!(archive.append(make()).unwrap().rows, 0);
    assert_eq!(archive.stats().rows_already_present, 2);
    assert_eq!(count(&fixture.reader(), "history_buckets"), 1);
}

#[test]
fn fixed_width_keys_sort_numerically() {
    let fixture = Fixture::new("order");
    let mut archive = fixture.open(ArchiveLimits::default());
    let now = minute_utc(&fixture, 0);
    let mut buckets = Vec::new();
    for start in [100u64, 2, 10] {
        let mut entry = bucket("seg-a", start, &now, &now);
        entry.end_boot_ns = start + 1;
        entry.covered_ns = 1;
        buckets.push(entry);
    }
    archive
        .append(journal(1, vec![segment("seg-a", &now)], buckets))
        .unwrap();
    let reader = fixture.reader();
    let mut statement = reader
        .prepare("SELECT start_boot_ns FROM history_buckets ORDER BY start_boot_ns")
        .unwrap();
    let keys: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(keys.iter().all(|key| key.len() == 20));
    let values: Vec<u64> = keys
        .iter()
        .map(|key| parse_fixed_width(key).unwrap())
        .collect();
    assert_eq!(values, vec![2, 10, 100]);
}

#[test]
fn counter_totals_are_exact_decimal_text() {
    let fixture = Fixture::new("u128");
    let mut archive = fixture.open(ArchiveLimits::default());
    let now = minute_utc(&fixture, 0);
    let mut counter = bucket("seg-c", 0, &now, &now);
    counter.method = HistoryMethod::CounterDelta;
    counter.counter_delta = Some(u128::MAX);
    counter.weighted_sum = None;
    counter.minimum = None;
    counter.maximum = None;
    let mut entry = journal(1, vec![segment("seg-c", &now)], vec![counter]);
    entry.bridges.push(ArchivedBridge {
        segment_id: "seg-c".to_owned(),
        start_boot_ns: 59_500_000_000,
        end_boot_ns: 60_500_000_000,
        delta: u128::from(u64::MAX) + 1,
    });
    archive.append(entry).unwrap();
    let reader = fixture.reader();
    let (delta, kind): (String, String) = reader
        .query_row(
            "SELECT counter_delta_decimal, typeof(counter_delta_decimal) FROM history_buckets",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(kind, "text");
    assert_eq!(delta.parse::<u128>().unwrap(), u128::MAX);
    let bridge: String = reader
        .query_row("SELECT delta_decimal FROM counter_bridges", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(bridge, "18446744073709551616");
}

#[test]
fn invalid_journals_are_rejected_without_writing() {
    let fixture = Fixture::new("reject");
    let mut archive = fixture.open(ArchiveLimits::default());
    let now = minute_utc(&fixture, 0);
    let mut replay = segment("seg-r", &now);
    replay.sample_kind = SampleKind::Replay;
    let rejected = archive.append(journal(
        1,
        vec![replay],
        vec![bucket("seg-r", 0, &now, &now)],
    ));
    assert!(matches!(rejected, Err(InfraError::Database(message)) if message.contains("replay")));
    let mut nan = bucket("seg-a", 0, &now, &now);
    nan.maximum = Some(f64::NAN);
    assert!(
        archive
            .append(journal(2, vec![segment("seg-a", &now)], vec![nan]))
            .is_err()
    );
    let orphan = archive.append(journal(
        3,
        Vec::new(),
        vec![bucket("seg-missing", 0, &now, &now)],
    ));
    assert!(
        matches!(orphan, Err(InfraError::Database(message)) if message.contains("seg-missing"))
    );
    let reader = fixture.reader();
    assert_eq!(count(&reader, "history_segments"), 0);
    assert_eq!(count(&reader, "history_buckets"), 0);
    assert_eq!(archive.status().state, PersistenceState::Healthy);
}

#[test]
fn an_expiry_deletes_old_data_and_cascades_to_empty_segments() {
    let fixture = Fixture::new("retention");
    let mut archive = fixture.open(ArchiveLimits::default());
    let old_start = fixture.utc_days_ago(10);
    let old_end = format_utc_rfc3339(fixture.clock.now_utc_ns() - 10 * DAY_NS + 60_000_000_000);
    let recent = minute_utc(&fixture, 5);
    let mut old_journal = journal(
        1,
        vec![
            segment("seg-old", &old_start),
            segment("seg-mixed", &old_start),
        ],
        vec![
            bucket("seg-old", 0, &old_start, &old_end),
            bucket("seg-mixed", 0, &old_start, &old_end),
            bucket("seg-mixed", 2 * MINUTE_NS, &recent, &recent),
        ],
    );
    old_journal.bridges.push(ArchivedBridge {
        segment_id: "seg-old".to_owned(),
        start_boot_ns: 30_000_000_000,
        end_boot_ns: 90_000_000_000,
        delta: 5,
    });
    old_journal.bridges.push(ArchivedBridge {
        segment_id: "seg-mixed".to_owned(),
        start_boot_ns: 30_000_000_000,
        end_boot_ns: 90_000_000_000,
        delta: 6,
    });
    old_journal.gaps.push(GapRecord {
        key: None,
        adapter_id: None,
        start_boot_ns: 1,
        end_boot_ns: 2,
        reason: "boot-gap".to_owned(),
        affected_count: 1,
    });
    archive.append(old_journal).unwrap();
    let mut other_boot = journal(2, Vec::new(), Vec::new());
    other_boot.boot_id = "previous-boot".to_owned();
    other_boot.gaps.push(GapRecord {
        key: None,
        adapter_id: None,
        start_boot_ns: 5,
        end_boot_ns: 6,
        reason: "suspend".to_owned(),
        affected_count: 2,
    });
    archive.append(other_boot).unwrap();
    assert_eq!(
        count(&fixture.reader(), "history_buckets"),
        3,
        "no append expires"
    );
    let report = archive.expire(&fixture.seven_day_expiry()).unwrap();
    assert_eq!(report.buckets, 2, "both 10-day-old buckets expire");
    assert_eq!(report.segments, 1, "the segment without buckets is deleted");
    assert_eq!(report.gaps, 1, "only the gap of a boot without data goes");
    assert!(report.complete);
    let reader = fixture.reader();
    let segments: Vec<String> = reader
        .prepare("SELECT segment_id FROM history_segments")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(segments, vec!["seg-mixed".to_owned()]);
    assert_eq!(count(&reader, "history_buckets"), 1);
    // seg-old's bridge cascaded with its segment; seg-mixed's bridge ends
    // before its earliest remaining bucket and is stale.
    assert_eq!(count(&reader, "counter_bridges"), 0);
    let remaining_gap_boot: String = reader
        .query_row("SELECT boot_id FROM history_gaps", [], |row| row.get(0))
        .unwrap();
    assert_eq!(remaining_gap_boot, "fake-boot");
    assert!(archive.stats().buckets_expired >= 2);
}

#[test]
fn low_free_disk_refuses_appends_and_recovers() {
    let fixture = Fixture::new("enospc");
    let mut archive = fixture.open(ArchiveLimits::default());
    let now = minute_utc(&fixture, 0);
    fixture.free.0.store(16 * MIB, Ordering::SeqCst);
    let refused = archive.append(journal(
        1,
        vec![segment("seg-a", &now)],
        vec![bucket("seg-a", 0, &now, &now)],
    ));
    assert_eq!(
        refused,
        Err(InfraError::DiskFull {
            operation: "archive.free_space"
        })
    );
    let status = archive.status();
    assert_eq!(status.state, PersistenceState::Failed);
    assert!(!status.accepts_writes());
    assert_eq!(count(&fixture.reader(), "history_segments"), 0);
    fixture.free.0.store(100 * 1024 * MIB, Ordering::SeqCst);
    assert_eq!(
        archive
            .append(journal(
                2,
                vec![segment("seg-a", &now)],
                vec![bucket("seg-a", 0, &now, &now)]
            ))
            .unwrap()
            .rows,
        2
    );
    assert_eq!(archive.status().state, PersistenceState::Healthy);
}

#[test]
fn budget_evicts_the_oldest_minutes_first_and_refuses_what_cannot_fit() {
    let fixture = Fixture::new("budget");
    let limits = ArchiveLimits {
        limit_bytes: 768 * KIB,
        wal_checkpoint_bytes: 64 * KIB,
        wal_stop_bytes: 128 * KIB,
        batch_rows: 64,
        batch_bytes: 16 * KIB,
        ..ArchiveLimits::default()
    };
    let mut archive = fixture.open(limits);
    let started = minute_utc(&fixture, 3000);
    archive
        .append(journal(1, vec![segment("seg-a", &started)], Vec::new()))
        .unwrap();
    let total_minutes = 3000u64;
    for first in (0..total_minutes).step_by(50) {
        let buckets: Vec<ArchivedBucket> = (first..first + 50)
            .map(|minute| {
                let utc = minute_utc(&fixture, 3000 - i128::from(minute));
                let next = minute_utc(&fixture, 2999 - i128::from(minute));
                let mut entry = bucket("seg-a", minute * MINUTE_NS, &utc, &next);
                entry.reason_code = Some("x".repeat(100));
                entry
            })
            .collect();
        archive
            .append(journal(2 + first, Vec::new(), buckets))
            .unwrap();
    }
    let stats = archive.stats();
    assert!(stats.buckets_evicted > 0, "{stats:?}");
    // The status judges admission like the next append: room is made by
    // eviction, so the archive still takes journals.
    assert_eq!(archive.status().state, PersistenceState::Healthy);
    let reader = fixture.reader();
    let oldest: String = reader
        .query_row(
            "SELECT min(start_boot_ns) FROM history_buckets",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        parse_fixed_width(&oldest).unwrap() > 0,
        "the oldest minutes went first"
    );
    let newest: String = reader
        .query_row(
            "SELECT max(start_boot_ns) FROM history_buckets",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        parse_fixed_width(&newest).unwrap(),
        (total_minutes - 1) * MINUTE_NS
    );
    // The main file never passes its page cap; WAL and shm stay bounded.
    let footprint = StorageFootprint::measure(&fixture.dir.database()).unwrap();
    assert!(
        footprint.database_bytes <= limits.limit_bytes - limits.wal_stop_bytes,
        "{footprint:?}"
    );
    assert!(
        footprint.total() <= limits.limit_bytes + limits.wal_stop_bytes,
        "{footprint:?}"
    );
    // One journal larger than the whole budget is refused.
    let utc = minute_utc(&fixture, 0);
    let huge: Vec<ArchivedBucket> = (0..4000u64)
        .map(|index| {
            let mut entry = bucket("seg-a", (1000 + index) * MINUTE_NS, &utc, &utc);
            entry.reason_code = Some("y".repeat(120));
            entry
        })
        .collect();
    assert_eq!(
        archive.append(journal(9999, Vec::new(), huge)),
        Err(InfraError::DiskFull {
            operation: "archive.limit"
        })
    );
}

#[test]
fn wal_pinned_by_a_reader_pauses_archive_writes() {
    let fixture = Fixture::new("wal");
    let limits = ArchiveLimits {
        limit_bytes: 16 * MIB,
        wal_checkpoint_bytes: 16 * KIB,
        wal_stop_bytes: 48 * KIB,
        batch_bytes: 8 * KIB,
        ..ArchiveLimits::default()
    };
    let mut archive = fixture.open(limits);
    let now = minute_utc(&fixture, 0);
    archive
        .append(journal(1, vec![segment("seg-a", &now)], Vec::new()))
        .unwrap();
    let reader = fixture.reader();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT count(*) FROM history_segments", [], |row| {
            row.get(0)
        })
        .unwrap();
    let mut minute = 0u64;
    let paused = loop {
        let entry = bucket("seg-a", minute * MINUTE_NS, &now, &now);
        match archive.append(journal(2 + minute, Vec::new(), vec![entry])) {
            Ok(_) => minute += 1,
            Err(error) => break error,
        }
        assert!(minute < 1000, "the WAL stop never triggered");
    };
    assert!(matches!(paused, InfraError::Unavailable(ref message) if message.contains("paused")));
    assert_eq!(archive.status().state, PersistenceState::Degraded);
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    let entry = bucket("seg-a", minute * MINUTE_NS, &now, &now);
    archive
        .append(journal(5000, Vec::new(), vec![entry]))
        .unwrap();
}

#[test]
fn corrupt_and_other_format_archives_are_recreated() {
    let fixture = Fixture::new("corrupt");
    std::fs::write(fixture.dir.database(), vec![0x42u8; 8192]).unwrap();
    std::fs::set_permissions(
        fixture.dir.database(),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let mut archive = fixture.open(ArchiveLimits::default());
    assert!(archive.recreated().unwrap().contains("schema inspection"));
    assert_eq!(archive.status().state, PersistenceState::Healthy);
    let now = minute_utc(&fixture, 0);
    archive
        .append(journal(1, vec![segment("seg-a", &now)], Vec::new()))
        .unwrap();
    drop(archive);

    // An archive written before D-85 (the spec's schema, no format
    // version) starts empty instead of being kept, and so does a newer one.
    for (label, stamp) in [("unstamped", 0), ("newer", format_stamp(2))] {
        let other = Fixture::new(label);
        {
            let mut archive = other.open(ArchiveLimits::default());
            archive
                .append(journal(1, vec![segment("seg-a", &now)], Vec::new()))
                .unwrap();
        }
        Connection::open(other.dir.database())
            .unwrap()
            .pragma_update(None, "application_id", stamp)
            .unwrap();
        let archive = other.open(ArchiveLimits::default());
        assert!(archive.recreated().unwrap().contains("format"), "{label}");
        drop(archive);
        assert_eq!(count(&other.reader(), "history_segments"), 0, "{label}");
    }
    let kept = Fixture::new("kept");
    {
        let mut archive = kept.open(ArchiveLimits::default());
        archive
            .append(journal(1, vec![segment("seg-a", &now)], Vec::new()))
            .unwrap();
    }
    let archive = kept.open(ArchiveLimits::default());
    assert_eq!(archive.recreated(), None);
    drop(archive);
    assert_eq!(count(&kept.reader(), "history_segments"), 1);
}

fn budget_limits() -> ArchiveLimits {
    ArchiveLimits {
        limit_bytes: 768 * KIB,
        wal_checkpoint_bytes: 64 * KIB,
        wal_stop_bytes: 128 * KIB,
        batch_rows: 64,
        batch_bytes: 16 * KIB,
        ..ArchiveLimits::default()
    }
}

fn filler(
    fixture: &Fixture,
    segment_id: &str,
    first_minute: u64,
    count: u64,
) -> Vec<ArchivedBucket> {
    (first_minute..first_minute + count)
        .map(|minute| {
            let utc = minute_utc(fixture, 3000 - i128::from(minute));
            let mut entry = bucket(segment_id, minute * MINUTE_NS, &utc, &utc);
            entry.reason_code = Some("z".repeat(100));
            entry
        })
        .collect()
}

#[test]
fn eviction_never_deletes_a_segment_the_append_writes_into() {
    let fixture = Fixture::new("protect");
    let mut archive = fixture.open(budget_limits());
    let ancient = minute_utc(&fixture, 9000);
    archive
        .append(journal(1, vec![segment("seg-b", &ancient)], Vec::new()))
        .unwrap();
    // Fill the budget until eviction has started.
    let mut minute = 0u64;
    while archive.stats().buckets_evicted == 0 {
        archive
            .append(journal(
                2 + minute,
                Vec::new(),
                filler(&fixture, "seg-b", minute, 50),
            ))
            .unwrap();
        minute += 50;
        assert!(minute < 100_000, "the budget never filled");
    }
    // seg-a gets the oldest bucket of the whole archive.
    let mut oldest = bucket("seg-a", 1, &ancient, &ancient);
    oldest.end_boot_ns = 2;
    oldest.covered_ns = 1;
    archive
        .append(journal(
            90_000,
            vec![segment("seg-a", &ancient)],
            vec![oldest],
        ))
        .unwrap();
    // This append evicts seg-a's only bucket while writing new ones into
    // seg-a (the journal does not repeat the segment row).
    let evicted_before = archive.stats().buckets_evicted;
    let receipt = archive
        .append(journal(
            90_001,
            Vec::new(),
            filler(&fixture, "seg-a", minute + 10, 50),
        ))
        .unwrap();
    assert_eq!(receipt.rows, 50);
    assert!(archive.stats().buckets_evicted > evicted_before);
    let reader = fixture.reader();
    let seg_a_buckets: i64 = reader
        .query_row(
            "SELECT count(*) FROM history_buckets WHERE segment_id = 'seg-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(seg_a_buckets, 50);
    let seg_a_rows: i64 = reader
        .query_row(
            "SELECT count(*) FROM history_segments WHERE segment_id = 'seg-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(seg_a_rows, 1);
}

/// A status read is a read: it never deletes rows, however old they are
/// or however long ago the archive last deleted anything (D-96).
#[test]
fn status_never_deletes_rows() {
    let fixture = Fixture::new("status-read-only");
    let mut archive = fixture.open(ArchiveLimits::default());
    let old_start = fixture.utc_days_ago(10);
    let old_end = format_utc_rfc3339(fixture.clock.now_utc_ns() - 10 * DAY_NS + 60_000_000_000);
    archive
        .append(journal(
            1,
            vec![segment("seg-old", &old_start)],
            vec![bucket("seg-old", 0, &old_start, &old_end)],
        ))
        .unwrap();
    fixture.clock.advance_ns(3600 * 1_000_000_000);
    let status = archive.status();
    assert_eq!(status.state, PersistenceState::Healthy);
    let reader = fixture.reader();
    assert_eq!(count(&reader, "history_segments"), 1);
    assert_eq!(count(&reader, "history_buckets"), 1);
}

/// Rows the history asked to delete that cannot be deleted do not undo a
/// committed journal: the append still answers with its receipt, which
/// names the failed deletion, and the status shows it (D-96).
#[test]
fn a_failed_deletion_after_the_commit_keeps_the_append_committed() {
    let fixture = Fixture::new("delete-after-commit");
    let mut archive = fixture.open(ArchiveLimits::default());
    let old = minute_utc(&fixture, 30);
    let old_end = minute_utc(&fixture, 29);
    archive
        .append(journal(
            1,
            vec![segment("seg-a", &old)],
            vec![bucket("seg-a", 0, &old, &old_end)],
        ))
        .unwrap();
    let writer = Connection::open(fixture.dir.database()).unwrap();
    writer
        .execute_batch(
            "CREATE TRIGGER refuse_bucket_delete BEFORE DELETE ON history_buckets
             BEGIN SELECT RAISE(ABORT, 'deletion refused by the test'); END;",
        )
        .unwrap();
    let now = minute_utc(&fixture, 0);
    let mut pruning = journal(
        2,
        vec![segment("seg-a", &old)],
        vec![bucket("seg-a", 30 * MINUTE_NS, &now, &now)],
    );
    pruning.prunes.push(ArchivePrune {
        series: SeriesInEveryBoot::from(&segment("seg-a", &old).key),
        before_utc_ns: i64::try_from(fixture.clock.now_utc_ns()).unwrap(),
    });
    let receipt = archive.append(pruning).unwrap();
    assert_eq!(receipt.revision, 2);
    assert_eq!(receipt.rows, 1, "the new minute is committed");
    assert!(receipt.deletion_failure.is_some(), "{receipt:?}");
    let status = archive.status();
    assert_eq!(status.state, PersistenceState::Degraded);
    assert!(status.detail.unwrap().contains("delet"));
    assert_eq!(count(&fixture.reader(), "history_buckets"), 2);
}

/// Neither opening the archive nor an append that asks for nothing deletes
/// old rows: the age they are kept is the history's (D-96).
#[test]
fn only_an_expiry_request_deletes_old_rows() {
    let fixture = Fixture::new("no-own-expiry");
    let old_start = fixture.utc_days_ago(10);
    let old_end = format_utc_rfc3339(fixture.clock.now_utc_ns() - 10 * DAY_NS + 60_000_000_000);
    let mut archive = fixture.open(ArchiveLimits::default());
    archive
        .append(journal(
            1,
            vec![segment("seg-old", &old_start)],
            vec![bucket("seg-old", 0, &old_start, &old_end)],
        ))
        .unwrap();
    drop(archive);
    fixture.clock.advance_ns(3600 * 1_000_000_000);
    let mut archive = fixture.open(ArchiveLimits::default());
    assert_eq!(
        count(&fixture.reader(), "history_buckets"),
        1,
        "open deletes nothing"
    );
    let now = minute_utc(&fixture, 0);
    archive
        .append(journal(
            2,
            vec![segment("seg-new", &now)],
            vec![bucket("seg-new", 60 * MINUTE_NS, &now, &now)],
        ))
        .unwrap();
    assert_eq!(
        count(&fixture.reader(), "history_buckets"),
        2,
        "nor an append"
    );
    let mut expiring = journal(3, Vec::new(), Vec::new());
    expiring.expire = Some(fixture.seven_day_expiry());
    let receipt = archive.append(expiring).unwrap();
    assert_eq!(receipt.deletion_failure, None);
    let reader = fixture.reader();
    assert_eq!(count(&reader, "history_buckets"), 1);
    assert_eq!(
        count(&reader, "history_segments"),
        1,
        "seg-old went with it"
    );
    assert_eq!(archive.stats().buckets_expired, 1);
}

/// A journal that asks to be refused when the budget is reached deletes
/// nothing to make room (D-96).
#[test]
fn a_full_archive_refuses_a_journal_that_does_not_ask_for_eviction() {
    let fixture = Fixture::new("refuse-when-full");
    let limits = ArchiveLimits {
        limit_bytes: 768 * KIB,
        wal_checkpoint_bytes: 64 * KIB,
        wal_stop_bytes: 128 * KIB,
        batch_rows: 64,
        batch_bytes: 16 * KIB,
        ..ArchiveLimits::default()
    };
    let mut archive = fixture.open(limits);
    let started = minute_utc(&fixture, 3000);
    archive
        .append(journal(1, vec![segment("seg-a", &started)], Vec::new()))
        .unwrap();
    let mut refused = None;
    for first in (0..3000u64).step_by(50) {
        let buckets: Vec<ArchivedBucket> = (first..first + 50)
            .map(|minute| {
                let utc = minute_utc(&fixture, 3000 - i128::from(minute));
                let mut entry = bucket("seg-a", minute * MINUTE_NS, &utc, &utc);
                entry.reason_code = Some("x".repeat(100));
                entry
            })
            .collect();
        let mut full = journal(2 + first, Vec::new(), buckets);
        full.when_full = ArchiveWhenFull::Refuse;
        if let Err(error) = archive.append(full) {
            refused = Some(error);
            break;
        }
    }
    assert_eq!(
        refused,
        Some(InfraError::DiskFull {
            operation: "archive.limit"
        })
    );
    assert_eq!(archive.stats().buckets_evicted, 0);
    let status = archive.status();
    assert_eq!(status.state, PersistenceState::Degraded);
    assert_eq!(
        status.detail.as_deref(),
        Some("archive budget cannot hold this batch")
    );
    let oldest: String = fixture
        .reader()
        .query_row(
            "SELECT min(start_boot_ns) FROM history_buckets",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        parse_fixed_width(&oldest).unwrap(),
        0,
        "the oldest minute stays"
    );
}
