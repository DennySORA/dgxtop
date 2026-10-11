use std::path::PathBuf;

use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::overview_history::{OverviewBucket, TierRow};
use dgxtop_core::testing::metric_key;
use dgxtop_core::tiers::{TIER_BUCKETS, Tier};

use super::*;
use crate::sqlite::{SchemaKind, SqliteGateError, SqliteVersion};

const MINUTE: i64 = 60 * 1_000_000_000;

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "dgxtop-history-tiers-{label}-{}-{}",
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

    fn path(&self) -> PathBuf {
        self.0.join("history_tiers.sqlite3")
    }

    fn open(&self) -> SqliteHistoryTiers {
        SqliteHistoryTiers::open(&self.path())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn series(entity: &str) -> SeriesInEveryBoot {
    SeriesInEveryBoot::from(&metric_key(entity, "gpu.power.draw", "gpu"))
}

fn gauge(start: i64, value: f64) -> OverviewBucket {
    OverviewBucket::gauge(
        start,
        60_000_000_000,
        value * 6e10,
        (value - 1.0, value + 1.0),
        12,
    )
}

fn row(entity: &str, tier: Tier, bucket: OverviewBucket) -> TierRow {
    TierRow {
        series: series(entity),
        tier,
        bucket,
    }
}

fn journal(rows: Vec<TierRow>) -> TierJournal {
    TierJournal {
        forget: Vec::new(),
        rows: std::sync::Arc::new(rows),
        keep_latest: u32::try_from(TIER_BUCKETS).unwrap(),
    }
}

/// Buckets written come back at the next start exactly, each size oldest
/// first; a bucket written again replaces the earlier one; empty and 5 s
/// buckets are not kept.
#[test]
fn buckets_survive_a_restart_exactly() {
    let scratch = Scratch::new("restart");
    let mut store = scratch.open();
    let counter = OverviewBucket::counter(0, MINUTE.unsigned_abs(), Some(12_345), 3);
    store
        .append(&journal(vec![
            row("gpu:0", Tier::OneMinute, gauge(MINUTE, 40.0)),
            row("gpu:0", Tier::OneMinute, gauge(0, 30.0)),
            row("gpu:0", Tier::OneHour, gauge(0, 35.0)),
            row("gpu:0", Tier::Recent, gauge(0, 99.0)),
            row("gpu:0", Tier::OneDay, OverviewBucket::new(0)),
            row("gpu:1", Tier::FiveMinutes, counter),
        ]))
        .unwrap();
    store
        .append(&journal(vec![row(
            "gpu:0",
            Tier::OneMinute,
            gauge(MINUTE, 50.0),
        )]))
        .unwrap();
    drop(store);
    let mut loaded = scratch.open().load().unwrap();
    loaded.sort_by(|left, right| left.series.cmp(&right.series));
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].series, series("gpu:0"));
    let minutes: Vec<(Tier, i64, Option<f64>)> = loaded[0]
        .buckets
        .iter()
        .map(|(tier, bucket)| (*tier, bucket.start_utc_ns, bucket.mean()))
        .collect();
    assert_eq!(
        minutes,
        vec![
            (Tier::OneMinute, 0, Some(30.0)),
            (Tier::OneMinute, MINUTE, Some(50.0)),
            (Tier::OneHour, 0, Some(35.0)),
        ]
    );
    assert_eq!(loaded[0].buckets[0].1, gauge(0, 30.0), "bit for bit");
    assert_eq!(loaded[1].buckets, vec![(Tier::FiveMinutes, counter)]);
    assert_eq!(loaded[1].buckets[0].1.total, 12_345);
}

/// Each (series, size) keeps the latest the journal asks for.
#[test]
fn each_size_keeps_its_latest_buckets() {
    let scratch = Scratch::new("latest");
    let mut store = scratch.open();
    let rows = (0..10)
        .map(|minute| row("gpu:0", Tier::OneMinute, gauge(minute * MINUTE, 1.0)))
        .chain([row("gpu:0", Tier::OneHour, gauge(0, 1.0))])
        .collect();
    store
        .append(&TierJournal {
            forget: Vec::new(),
            rows: std::sync::Arc::new(rows),
            keep_latest: 4,
        })
        .unwrap();
    let loaded = store.load().unwrap();
    let starts: Vec<(Tier, i64)> = loaded[0]
        .buckets
        .iter()
        .map(|(tier, bucket)| (*tier, bucket.start_utc_ns / MINUTE))
        .collect();
    assert_eq!(
        starts,
        vec![
            (Tier::OneMinute, 6),
            (Tier::OneMinute, 7),
            (Tier::OneMinute, 8),
            (Tier::OneMinute, 9),
            (Tier::OneHour, 0),
        ]
    );
}

/// A series the history let go is deleted in every size before the
/// journal's rows are written, so one coming back in the same journal
/// starts fresh; a forgotten series never stored is no error, and a
/// journal of deletions alone is carried out.
#[test]
fn a_forgotten_series_is_deleted_before_the_rows() {
    let scratch = Scratch::new("forget");
    let mut store = scratch.open();
    store
        .append(&journal(vec![
            row("gpu:0", Tier::OneMinute, gauge(0, 1.0)),
            row("gpu:0", Tier::OneHour, gauge(0, 1.0)),
            row("gpu:1", Tier::OneMinute, gauge(0, 2.0)),
            row("gpu:2", Tier::OneMinute, gauge(0, 3.0)),
        ]))
        .unwrap();
    store
        .append(&TierJournal {
            forget: vec![series("gpu:0"), series("gpu:1"), series("gpu:9")],
            ..journal(vec![row("gpu:1", Tier::OneMinute, gauge(MINUTE, 5.0))])
        })
        .unwrap();
    let loaded = store.load().unwrap();
    let kept: Vec<(&str, Vec<i64>)> = loaded
        .iter()
        .map(|stored| {
            let starts = stored
                .buckets
                .iter()
                .map(|(_, bucket)| bucket.start_utc_ns / MINUTE)
                .collect();
            (stored.series.entity_id.as_str(), starts)
        })
        .collect();
    assert_eq!(kept, vec![("gpu:1", vec![1]), ("gpu:2", vec![0])]);
    store
        .append(&TierJournal {
            forget: vec![series("gpu:2")],
            ..journal(Vec::new())
        })
        .unwrap();
    let left: Vec<String> = store
        .load()
        .unwrap()
        .iter()
        .map(|stored| stored.series.entity_id.to_string())
        .collect();
    assert_eq!(left, vec!["gpu:1".to_owned()]);
}

/// The store deletes only what the history's journal says (D-95): however
/// many pages are in use, every series it was not told to forget keeps
/// every bucket, and the append reports the pages in use and the series
/// held for the history's budget.
#[test]
fn the_store_never_lets_a_series_go_on_its_own() {
    let scratch = Scratch::new("retain");
    let mut store = scratch.open();
    let mut usages = Vec::new();
    for (entity, start) in [("gpu:old", 0), ("gpu:new", 1_000 * MINUTE)] {
        let rows = (0..300)
            .map(|minute| row(entity, Tier::OneMinute, gauge(start + minute * MINUTE, 1.0)))
            .collect();
        usages.push(store.append(&journal(rows)).unwrap());
    }
    let loaded = store.load().unwrap();
    assert_eq!(loaded.len(), 2, "nothing was forgotten");
    assert!(loaded.iter().all(|stored| stored.buckets.len() == 300));
    assert_eq!(usages[0].series, 1);
    assert_eq!(usages[1].series, 2);
    assert!(usages[1].used_bytes > usages[0].used_bytes, "{usages:?}");
    assert!(usages[1].used_bytes > 40 * 1024, "past the old limit");
    let forgotten = store
        .append(&TierJournal {
            forget: vec![series("gpu:old")],
            ..journal(Vec::new())
        })
        .unwrap();
    assert_eq!(forgotten.series, 1, "the journal's deletion is counted");
    let idle = store.append(&journal(Vec::new())).unwrap();
    assert_eq!(idle, forgotten, "an empty journal reports what is held");
}

/// The database is private to its user.
#[test]
fn the_database_is_private() {
    let scratch = Scratch::new("mode");
    let mut store = scratch.open();
    store
        .append(&journal(vec![row("gpu:0", Tier::OneMinute, gauge(0, 1.0))]))
        .unwrap();
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(scratch.path()).unwrap().permissions(),
    );
    assert_eq!(mode & 0o777, 0o600);
}

fn failing_gate() -> Result<SqliteVersion, SqliteGateError> {
    Err(SqliteGateError::TooOld {
        found: 3_000_000,
        required: 3_051_003,
    })
}

/// An unusable database fails every call with its reason.
#[test]
fn an_unusable_database_reports_why() {
    let scratch = Scratch::new("unusable");
    let mut store = SqliteHistoryTiers::open_with(&scratch.path(), &RealFileSystem, failing_gate);
    let error = store.load().unwrap_err().to_string();
    assert!(error.contains("history tiers database"), "{error}");
    assert!(store.append(&journal(Vec::new())).is_err());
}

/// A tiers database of another format version, or none (written before
/// D-85), starts empty; one of this format keeps its buckets.
#[test]
fn a_database_of_another_format_starts_empty() {
    let current = SchemaKind::HistoryTiers.format_stamp();
    for (label, stamp, kept) in [
        ("same", current, true),
        ("unstamped", 0, false),
        ("newer", crate::sqlite::format_stamp(2), false),
    ] {
        let scratch = Scratch::new(label);
        let mut store = scratch.open();
        store
            .append(&journal(vec![row(
                "gpu0",
                Tier::OneMinute,
                gauge(10 * MINUTE, 40.0),
            )]))
            .unwrap();
        drop(store);
        rusqlite::Connection::open(scratch.path())
            .unwrap()
            .pragma_update(None, "application_id", stamp)
            .unwrap();
        let mut store = scratch.open();
        assert_eq!(store.recreated().is_none(), kept, "{label}");
        assert_eq!(store.load().unwrap().len(), usize::from(kept), "{label}");
    }
}

/// A full database is a full disk to the history, like the archive's
/// (one SQLite mapping for every partition).
#[test]
fn a_full_database_reports_disk_full() {
    let scratch = Scratch::new("disk-full");
    let mut store = scratch.open();
    let _cap: i64 = store
        .connection()
        .unwrap()
        .query_row("PRAGMA max_page_count = 1", [], |row| row.get(0))
        .unwrap();
    let rows = (0..300)
        .map(|minute| row("gpu:0", Tier::OneMinute, gauge(minute * MINUTE, 1.0)))
        .collect();
    let error = store.append(&journal(rows)).unwrap_err();
    assert!(matches!(error, InfraError::DiskFull { .. }), "{error:?}");
}

/// A stored bucket SQLite cannot hand over as this build's types is
/// reported, not dropped: the load fails with what is wrong.
#[test]
fn an_unreadable_bucket_fails_the_load() {
    let scratch = Scratch::new("bad-bucket");
    let mut store = scratch.open();
    store
        .append(&journal(vec![row("gpu:0", Tier::OneMinute, gauge(0, 1.0))]))
        .unwrap();
    store
        .connection()
        .unwrap()
        .execute("UPDATE tier_buckets SET sample_count = 1099511627776", [])
        .unwrap();
    let error = store.load().unwrap_err();
    assert!(matches!(error, InfraError::Database(_)), "{error:?}");
    assert!(
        error.to_string().contains("history_tiers.buckets"),
        "{error}"
    );
}

/// Corruption stops the database like every partition's (sqlite::Writer):
/// later calls say why.
#[test]
fn a_corrupt_database_stops_with_its_reason() {
    let scratch = Scratch::new("corrupt");
    let mut store = scratch.open();
    let corrupt = store
        .with_database(|_| -> Result<(), DatabaseFailure> {
            Err(DatabaseFailure::Transaction {
                operation: "history_tiers.append",
                error: crate::sqlite::TxError::Commit(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_NOTADB),
                    None,
                )),
            })
        })
        .unwrap_err();
    assert!(matches!(corrupt, InfraError::Database(_)), "{corrupt:?}");
    let error = store.append(&journal(Vec::new())).unwrap_err();
    assert!(
        error.to_string().contains("history tiers stopped"),
        "{error}"
    );
    assert!(store.load().is_err());
}

/// A relative path is refused like the archive's and the audit's: the
/// store is unusable and says why, and nothing is created anywhere.
#[test]
fn a_relative_path_is_refused() {
    let relative = PathBuf::from(format!(
        "dgxtop-relative-{}/history_tiers.sqlite3",
        uuid::Uuid::new_v4().simple()
    ));
    let mut store = SqliteHistoryTiers::open(&relative);
    let error = store.load().unwrap_err().to_string();
    assert!(error.contains("must be absolute"), "{error}");
    assert!(!relative.exists());
}
