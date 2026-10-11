use std::path::PathBuf;

use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::overview_history::{OverviewBucket, TierRow};
use dgxtop_core::process_history::{TIER_BUCKETS, Tier};
use dgxtop_core::testing::{FakeClock, metric_key};

use super::*;
use crate::sqlite::{SqliteGateError, SqliteVersion};

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
        Self(dir)
    }

    fn path(&self) -> PathBuf {
        self.0.join("history_tiers.sqlite3")
    }

    fn open(&self) -> SqliteHistoryTiers {
        SqliteHistoryTiers::open(&self.path(), FakeClock::new(1_000_000_000))
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
        rows,
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
            rows,
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

/// Past the size limit the series written longest ago give way.
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

#[test]
fn the_series_written_longest_ago_give_way() {
    let scratch = Scratch::new("retain");
    let mut store = scratch.open().with_max_used_bytes(40 * 1024);
    for (entity, start) in [("gpu:old", 0), ("gpu:new", 1_000 * MINUTE)] {
        let rows = (0..300)
            .map(|minute| row(entity, Tier::OneMinute, gauge(start + minute * MINUTE, 1.0)))
            .collect();
        store.append(&journal(rows)).unwrap();
    }
    let loaded = store.load().unwrap();
    assert_eq!(loaded.len(), 1, "one series fits");
    assert_eq!(loaded[0].series, series("gpu:new"));
    assert_eq!(loaded[0].buckets.len(), 300);
    let connection = store.connection().unwrap();
    assert_eq!(database::series_count(connection).unwrap(), 1);
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
    let clock = FakeClock::new(0);
    let mut store = SqliteHistoryTiers::open_with(
        &scratch.path(),
        clock.as_ref(),
        &RealFileSystem,
        failing_gate,
    );
    let error = store.load().unwrap_err().to_string();
    assert!(error.contains("history tiers database"), "{error}");
    assert!(store.append(&journal(Vec::new())).is_err());
}
