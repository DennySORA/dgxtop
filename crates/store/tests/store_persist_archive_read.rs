//! Reading the history archive back for the history view (D-51): minutes of
//! every boot of a series merged into points on the wall clock, a window
//! that keeps only the minutes started in it, units that never mix, and a
//! read that stays bounded (missing or unready file, refused schema,
//! cancel, deadline, point cap).

// Test helpers panic on setup failures; the lints target production code.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::errors::ErrorCode;
use dgxtop_core::history::HistoryMethod;
use dgxtop_core::history::{ARCHIVE_MINUTE_NS, ArchiveQuery, SeriesInEveryBoot};
use dgxtop_core::identity::{BootId, HostId, MetricKey};
use dgxtop_core::metric::CanonicalUnit;
use dgxtop_core::ports::{
    ArchiveJournal, ArchiveReader, ArchiveSink, ArchivedBucket, ArchivedSegment, CancelFlag,
};
use dgxtop_core::quality::SampleKind;
use dgxtop_core::testing::{FakeClock, metric_key, source_id};
use dgxtop_store::archive::{ArchiveLimits, SqliteArchive, SqliteArchiveReader};
use dgxtop_store::sqlite::StatvfsFreeSpace;
use uuid::Uuid;

const MINUTE: u64 = ARCHIVE_MINUTE_NS;
const SECOND: u64 = 1_000_000_000;
/// A deadline no read reaches.
const NO_DEADLINE: u64 = u64::MAX;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "dgxtop-archive-read-{label}-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).unwrap();
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

/// An archive written by the real writer and read by the reader.
struct Fixture {
    dir: TempDir,
    clock: Arc<FakeClock>,
    archive: SqliteArchive,
    revision: u64,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let dir = TempDir::new(label);
        let clock = FakeClock::new(10 * MINUTE);
        let archive = SqliteArchive::open(
            &dir.database(),
            ArchiveLimits::default(),
            clock.clone(),
            Arc::new(StatvfsFreeSpace),
        )
        .unwrap();
        Self {
            dir,
            clock,
            archive,
            revision: 0,
        }
    }

    /// UTC of now on the fixture's wall clock, whole minutes.
    fn now_utc(&self) -> u64 {
        let now = u64::try_from(self.clock.now_utc_ns()).unwrap();
        now - now % MINUTE
    }

    /// Writes `minutes` (value per minute, `None` for an uncovered one) of
    /// `key`'s segment `segment_id` starting at `start_utc` on boot time
    /// `start_boot`.
    fn write(&mut self, run: &Run<'_>, minutes: &[Option<f64>]) {
        self.revision += 1;
        let buckets = minutes
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let offset = u64::try_from(index).unwrap() * MINUTE;
                bucket(run, offset, *value)
            })
            .collect();
        let journal = ArchiveJournal {
            revision: self.revision,
            host_id: run.key.host_id.to_string(),
            boot_id: run.key.boot_id.to_string(),
            segments: vec![segment(run)],
            buckets,
            bridges: Vec::new(),
            gaps: Vec::new(),
            prunes: Vec::new(),
            permit: None,
        };
        self.archive.append(journal).unwrap();
    }

    fn reader(&self) -> SqliteArchiveReader {
        SqliteArchiveReader::new(&self.dir.database())
    }
}

/// Where a run's minutes go.
struct Run<'a> {
    key: &'a MetricKey,
    segment_id: &'a str,
    unit: CanonicalUnit,
    start_utc: u64,
    start_boot: u64,
}

fn utc_text(utc_ns: u64) -> String {
    let seconds = utc_ns / SECOND;
    let nanos = utc_ns % SECOND;
    let text = dgxtop_store::sqlite::format_utc_rfc3339(i128::from(seconds * SECOND));
    format!("{}.{nanos:09}Z", text.trim_end_matches('Z'))
}

fn segment(run: &Run<'_>) -> ArchivedSegment {
    ArchivedSegment {
        segment_id: run.segment_id.to_owned(),
        key: run.key.clone(),
        source_id: source_id("nvml:GPU-1:temp"),
        source_generation: 1,
        policy_revision: 1,
        unit: run.unit,
        sample_kind: SampleKind::Measured,
        started_utc: utc_text(run.start_utc),
    }
}

/// A gauge minute at `value` (the maximum two above), fully covered; an
/// uncovered minute when `value` is `None`.
fn bucket(run: &Run<'_>, offset: u64, value: Option<f64>) -> ArchivedBucket {
    let covered = if value.is_some() { MINUTE } else { 0 };
    #[allow(clippy::cast_precision_loss)]
    let weighted = value.map(|value| value * covered as f64);
    ArchivedBucket {
        segment_id: run.segment_id.to_owned(),
        start_boot_ns: run.start_boot + offset,
        end_boot_ns: run.start_boot + offset + MINUTE,
        start_utc: utc_text(run.start_utc + offset),
        end_utc: utc_text(run.start_utc + offset + MINUTE),
        covered_ns: covered,
        weighted_sum: weighted,
        minimum: value,
        maximum: value.map(|value| value + 2.0),
        counter_delta: None,
        sample_count: if value.is_some() { 60 } else { 0 },
        method: if value.is_some() {
            HistoryMethod::TimeWeightedStep
        } else {
            HistoryMethod::Gap
        },
        reason_code: None,
    }
}

fn temperature_in(boot: &str) -> MetricKey {
    let mut key = metric_key("gpu:GPU-1", "gpu.temperature", "gpu");
    key.boot_id = BootId::new(boot).unwrap();
    key
}

fn query(key: &MetricKey, start_utc: u64, end_utc: u64, point_ns: u64) -> ArchiveQuery {
    ArchiveQuery {
        request_id: Uuid::new_v4(),
        series: SeriesInEveryBoot::from(key),
        start_utc_ns: start_utc,
        end_utc_ns: end_utc,
        point_ns,
        max_points: 2000,
    }
}

#[test]
fn every_boot_of_a_series_is_read_back_on_the_wall_clock() {
    let mut fixture = Fixture::new("boots");
    let now = fixture.now_utc();
    let (boot_a, boot_b) = (temperature_in("boot-a"), temperature_in("boot-b"));
    // Boot A three hours ago, ten minutes at 50 °C; boot B (its boot clock
    // restarted near zero) an hour ago, five minutes at 70 °C.
    let first = Run {
        key: &boot_a,
        segment_id: "seg-a",
        unit: CanonicalUnit::DegC,
        start_utc: now - 180 * MINUTE,
        start_boot: 7_000 * MINUTE,
    };
    fixture.write(&first, &[Some(50.0); 10]);
    let second = Run {
        key: &boot_b,
        segment_id: "seg-b",
        unit: CanonicalUnit::DegC,
        start_utc: now - 60 * MINUTE,
        start_boot: 2 * MINUTE,
    };
    fixture.write(&second, &[Some(70.0); 5]);
    let mut reader = fixture.reader();
    // Asked through the current boot's key: the boot is not part of it.
    let current = temperature_in("boot-c");
    let half_hour = 30 * MINUTE;
    let result = reader
        .read(
            &query(&current, now - 240 * MINUTE, now, half_hour),
            NO_DEADLINE,
            &CancelFlag::new(),
            fixture.clock.as_ref(),
        )
        .unwrap();
    assert_eq!(result.boots, 2);
    assert_eq!(result.points.len(), 2, "{result:#?}");
    let (older, newer) = (&result.points[0], &result.points[1]);
    // A point spans the minutes merged into it, where they were recorded.
    assert_eq!(older.start_utc_ns, now - 180 * MINUTE);
    assert_eq!(older.end_utc_ns, now - 170 * MINUTE);
    assert_eq!(newer.end_utc_ns - newer.start_utc_ns, 5 * MINUTE);
    assert_eq!((older.mean, older.max), (Some(50.0), Some(52.0)));
    assert_eq!(older.covered_ns, 10 * MINUTE);
    assert_eq!((newer.mean, newer.max), (Some(70.0), Some(72.0)));
    assert_eq!(newer.covered_ns, 5 * MINUTE);
    assert_eq!(result.covered_ns, 15 * MINUTE);
    assert!(!result.truncated);
}

#[test]
fn a_point_weighs_its_minutes_by_covered_time_and_keeps_the_peak() {
    let mut fixture = Fixture::new("weights");
    let now = fixture.now_utc();
    let key = temperature_in("boot-a");
    let run = Run {
        key: &key,
        segment_id: "seg-a",
        unit: CanonicalUnit::DegC,
        start_utc: now - 30 * MINUTE,
        start_boot: 100 * MINUTE,
    };
    // An uncovered minute in the middle adds no time and no weight.
    fixture.write(&run, &[Some(40.0), None, Some(60.0), Some(80.0)]);
    let result = fixture
        .reader()
        .read(
            &query(&key, now - 60 * MINUTE, now, 60 * MINUTE),
            NO_DEADLINE,
            &CancelFlag::new(),
            fixture.clock.as_ref(),
        )
        .unwrap();
    let points: Vec<_> = result
        .points
        .iter()
        .map(|point| (point.mean, point.max, point.covered_ns))
        .collect();
    assert_eq!(points, vec![(Some(60.0), Some(82.0), 3 * MINUTE)]);
}

#[test]
fn a_window_keeps_only_the_minutes_that_started_in_it() {
    let mut fixture = Fixture::new("window");
    let now = fixture.now_utc();
    let key = temperature_in("boot-a");
    let run = Run {
        key: &key,
        segment_id: "seg-a",
        unit: CanonicalUnit::DegC,
        start_utc: now - 20 * MINUTE,
        start_boot: 500 * MINUTE,
    };
    fixture.write(&run, &[Some(55.0); 10]);
    let mut reader = fixture.reader();
    let window = query(&key, now - 17 * MINUTE, now - 13 * MINUTE, MINUTE);
    let result = reader
        .read(
            &window,
            NO_DEADLINE,
            &CancelFlag::new(),
            fixture.clock.as_ref(),
        )
        .unwrap();
    let starts: Vec<u64> = result
        .points
        .iter()
        .map(|point| (now - point.start_utc_ns) / MINUTE)
        .collect();
    assert_eq!(starts, vec![17, 16, 15, 14]);
    // A window with no minute in it.
    let empty = query(&key, now - 5 * MINUTE, now, MINUTE);
    let result = reader
        .read(
            &empty,
            NO_DEADLINE,
            &CancelFlag::new(),
            fixture.clock.as_ref(),
        )
        .unwrap();
    assert!(result.points.is_empty() && result.boots == 0);
}

#[test]
fn other_units_series_and_hosts_are_never_merged() {
    let mut fixture = Fixture::new("isolation");
    let now = fixture.now_utc();
    let key = temperature_in("boot-a");
    let other_unit = Run {
        key: &key,
        segment_id: "seg-percent",
        unit: CanonicalUnit::Percent,
        start_utc: now - 10 * MINUTE,
        start_boot: 10 * MINUTE,
    };
    fixture.write(&other_unit, &[Some(300.0); 3]);
    let mut other_series = metric_key("gpu:GPU-2", "gpu.temperature", "gpu");
    other_series.boot_id = BootId::new("boot-a").unwrap();
    let neighbour = Run {
        key: &other_series,
        segment_id: "seg-other",
        unit: CanonicalUnit::DegC,
        start_utc: now - 10 * MINUTE,
        start_boot: 10 * MINUTE,
    };
    fixture.write(&neighbour, &[Some(99.0); 3]);
    let mut other_host = key.clone();
    other_host.host_id = HostId::new("other-host").unwrap();
    let elsewhere = Run {
        key: &other_host,
        segment_id: "seg-host",
        unit: CanonicalUnit::DegC,
        start_utc: now - 10 * MINUTE,
        start_boot: 10 * MINUTE,
    };
    fixture.write(&elsewhere, &[Some(98.0); 3]);
    let result = fixture
        .reader()
        .read(
            &query(&key, now - 60 * MINUTE, now, MINUTE),
            NO_DEADLINE,
            &CancelFlag::new(),
            fixture.clock.as_ref(),
        )
        .unwrap();
    assert!(result.points.is_empty(), "{result:#?}");
}

#[test]
fn a_missing_or_unready_archive_answers_with_no_points() {
    let dir = TempDir::new("missing");
    let clock = FakeClock::new(MINUTE);
    let key = temperature_in("boot-a");
    let window = query(&key, MINUTE, 10 * MINUTE, MINUTE);
    let mut reader = SqliteArchiveReader::new(&dir.database());
    let result = reader
        .read(&window, NO_DEADLINE, &CancelFlag::new(), clock.as_ref())
        .unwrap();
    assert!(result.points.is_empty());
    assert!(
        !dir.database().exists(),
        "the reader never creates the file"
    );
    // A file the writer has not given its schema yet.
    rusqlite::Connection::open(dir.database()).unwrap();
    let result = reader
        .read(&window, NO_DEADLINE, &CancelFlag::new(), clock.as_ref())
        .unwrap();
    assert!(result.points.is_empty());
}

#[test]
fn a_schema_this_build_does_not_know_is_refused_and_left_alone() {
    let dir = TempDir::new("future");
    let connection = rusqlite::Connection::open(dir.database()).unwrap();
    connection.pragma_update(None, "user_version", 3).unwrap();
    drop(connection);
    let clock = FakeClock::new(MINUTE);
    let key = temperature_in("boot-a");
    let mut reader = SqliteArchiveReader::new(&dir.database());
    let error = reader
        .read(
            &query(&key, MINUTE, 10 * MINUTE, MINUTE),
            NO_DEADLINE,
            &CancelFlag::new(),
            clock.as_ref(),
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert!(error.message.contains("schema version 3"), "{error:?}");
    let version: i64 = rusqlite::Connection::open(dir.database())
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 3);
}

#[test]
fn a_read_stops_when_cancelled_or_past_its_deadline_and_rejects_bad_windows() {
    let mut fixture = Fixture::new("bounds");
    let now = fixture.now_utc();
    let key = temperature_in("boot-a");
    let run = Run {
        key: &key,
        segment_id: "seg-a",
        unit: CanonicalUnit::DegC,
        start_utc: now - 10 * MINUTE,
        start_boot: 10 * MINUTE,
    };
    fixture.write(&run, &[Some(50.0); 3]);
    let mut reader = fixture.reader();
    let window = query(&key, now - 60 * MINUTE, now, MINUTE);
    let cancel = CancelFlag::new();
    cancel.cancel();
    let cancelled = reader
        .read(&window, NO_DEADLINE, &cancel, fixture.clock.as_ref())
        .unwrap_err();
    assert_eq!(cancelled.code, ErrorCode::Cancelled);
    let past = fixture.clock.now_boot_ns();
    let late = reader
        .read(&window, past, &CancelFlag::new(), fixture.clock.as_ref())
        .unwrap_err();
    assert_eq!(late.code, ErrorCode::TimedOut);
    let mut bad = window.clone();
    bad.point_ns = 30 * SECOND;
    let invalid = reader
        .read(
            &bad,
            NO_DEADLINE,
            &CancelFlag::new(),
            fixture.clock.as_ref(),
        )
        .unwrap_err();
    assert_eq!(invalid.code, ErrorCode::ValidationFailed);
    // The reader keeps working after a failed read.
    let result = reader
        .read(
            &window,
            NO_DEADLINE,
            &CancelFlag::new(),
            fixture.clock.as_ref(),
        )
        .unwrap();
    assert_eq!(result.points.len(), 3);
}

#[test]
fn the_point_cap_keeps_the_newest_points_and_says_so() {
    let mut fixture = Fixture::new("cap");
    let now = fixture.now_utc();
    let key = temperature_in("boot-a");
    let run = Run {
        key: &key,
        segment_id: "seg-a",
        unit: CanonicalUnit::DegC,
        start_utc: now - 10 * MINUTE,
        start_boot: 10 * MINUTE,
    };
    fixture.write(&run, &[Some(10.0), Some(20.0), Some(30.0)]);
    let mut window = query(&key, now - 60 * MINUTE, now, MINUTE);
    window.max_points = 2;
    let result = fixture
        .reader()
        .read(
            &window,
            NO_DEADLINE,
            &CancelFlag::new(),
            fixture.clock.as_ref(),
        )
        .unwrap();
    let means: Vec<_> = result.points.iter().map(|point| point.mean).collect();
    assert_eq!(means, vec![Some(20.0), Some(30.0)]);
    assert!(result.truncated);
}

fn utc_i64(utc_ns: u64) -> i64 {
    i64::try_from(utc_ns).unwrap()
}

/// The listing names the series of the asked metrics that earlier runs
/// left in their canonical unit, once each over every boot; this run's own
/// segments and other metrics, units and hosts are not listed (D-82).
#[test]
fn the_backfill_lists_the_series_earlier_runs_left() {
    let mut fixture = Fixture::new("listing");
    let now = fixture.now_utc();
    let (boot_a, boot_b) = (temperature_in("boot-a"), temperature_in("boot-b"));
    let mut other_metric = temperature_in("boot-a");
    other_metric.metric_id =
        dgxtop_core::identity::MetricId::new("cpu.utilization.iowait").unwrap();
    let mut other_host = temperature_in("boot-a");
    other_host.host_id = HostId::new("other-host").unwrap();
    let runs = [
        (&boot_a, "seg-a", CanonicalUnit::DegC, now - 100 * MINUTE),
        (&boot_b, "seg-b", CanonicalUnit::DegC, now - 50 * MINUTE),
        (&boot_b, "seg-k", CanonicalUnit::Count, now - 50 * MINUTE),
        (
            &other_metric,
            "seg-o",
            CanonicalUnit::Percent,
            now - 50 * MINUTE,
        ),
        (&other_host, "seg-h", CanonicalUnit::DegC, now - 50 * MINUTE),
    ];
    for (key, segment_id, unit, start_utc) in runs {
        let run = Run {
            key,
            segment_id,
            unit,
            start_utc,
            start_boot: MINUTE,
        };
        fixture.write(&run, &[Some(50.0)]);
    }
    let list = |before: u64| dgxtop_core::overview_history::ArchivedSeriesList {
        host_id: boot_a.host_id.clone(),
        metric_ids: vec!["gpu.temperature".to_owned(), "gpu.power.draw".to_owned()],
        before_utc_ns: utc_i64(before),
    };
    let mut reader = fixture.reader();
    let listed = reader.archived_series(&list(now)).unwrap();
    assert_eq!(listed, vec![SeriesInEveryBoot::from(&boot_a)]);
    assert!(
        reader
            .archived_series(&list(now - 200 * MINUTE))
            .unwrap()
            .is_empty(),
        "segments started since are this run's"
    );
}

/// A series' minutes from every boot come oldest first, from the asked
/// start on, a page at a time, each a bucket at its minute's UTC start.
#[test]
fn the_backfill_reads_minutes_oldest_first_a_page_at_a_time() {
    let mut fixture = Fixture::new("minutes");
    let now = fixture.now_utc();
    let (boot_a, boot_b) = (temperature_in("boot-a"), temperature_in("boot-b"));
    let first = Run {
        key: &boot_a,
        segment_id: "seg-a",
        unit: CanonicalUnit::DegC,
        start_utc: now - 100 * MINUTE + 17 * SECOND,
        start_boot: 7_000 * MINUTE,
    };
    fixture.write(&first, &[Some(40.0), None, Some(50.0)]);
    let second = Run {
        key: &boot_b,
        segment_id: "seg-b",
        unit: CanonicalUnit::DegC,
        start_utc: now - 50 * MINUTE,
        start_boot: MINUTE,
    };
    fixture.write(&second, &[Some(60.0), Some(70.0)]);
    let read = |from: u64, max_minutes: u32| dgxtop_core::overview_history::ArchivedMinutesRead {
        series: SeriesInEveryBoot::from(&boot_a),
        from_utc_ns: utc_i64(from),
        segments_before_utc_ns: utc_i64(now),
        max_minutes,
    };
    let mut reader = fixture.reader();
    let all = reader.archived_minutes(&read(0, 100)).unwrap();
    assert!(!all.more);
    let shape: Vec<(i64, Option<f64>, Option<f64>)> = all
        .minutes
        .iter()
        .map(|minute| (minute.start_utc_ns, minute.mean(), minute.peak()))
        .collect();
    let at = |utc: u64| utc_i64(utc);
    assert_eq!(
        shape,
        vec![
            (at(first.start_utc), Some(40.0), Some(42.0)),
            (at(first.start_utc + 2 * MINUTE), Some(50.0), Some(52.0)),
            (at(second.start_utc), Some(60.0), Some(62.0)),
            (at(second.start_utc + MINUTE), Some(70.0), Some(72.0)),
        ],
        "uncovered minutes left out, unaligned starts kept"
    );
    assert_eq!(all.minutes[0].samples, 60);
    let page = reader.archived_minutes(&read(0, 2)).unwrap();
    assert!(page.more);
    assert_eq!(page.minutes.len(), 2);
    let rest = reader
        .archived_minutes(&read(first.start_utc + 2 * MINUTE + 1, 2))
        .unwrap();
    assert!(!rest.more);
    assert_eq!(rest.minutes.len(), 2, "from the asked start on");
}

/// A prune deletes one series' minutes that started before its cutoff in
/// every boot; other series and later minutes stay (D-82).
#[test]
fn a_prune_deletes_one_series_minutes_before_its_cutoff() {
    let mut fixture = Fixture::new("prune");
    let now = fixture.now_utc();
    let (boot_a, boot_b) = (temperature_in("boot-a"), temperature_in("boot-b"));
    let mut power = temperature_in("boot-b");
    power.metric_id = dgxtop_core::identity::MetricId::new("gpu.power.draw").unwrap();
    let runs = [
        (&boot_a, "seg-a", CanonicalUnit::DegC, now - 100 * MINUTE),
        (&boot_b, "seg-b", CanonicalUnit::DegC, now - 50 * MINUTE),
        (&power, "seg-p", CanonicalUnit::Watts, now - 50 * MINUTE),
    ];
    for (key, segment_id, unit, start_utc) in runs {
        let run = Run {
            key,
            segment_id,
            unit,
            start_utc,
            start_boot: MINUTE,
        };
        fixture.write(&run, &[Some(50.0); 10]);
    }
    fixture.revision += 1;
    let prune = dgxtop_core::overview_history::ArchivePrune {
        series: SeriesInEveryBoot::from(&boot_a),
        before_utc_ns: utc_i64(now - 45 * MINUTE),
    };
    fixture
        .archive
        .append(ArchiveJournal {
            revision: fixture.revision,
            host_id: boot_b.host_id.to_string(),
            boot_id: boot_b.boot_id.to_string(),
            segments: Vec::new(),
            buckets: Vec::new(),
            bridges: Vec::new(),
            gaps: Vec::new(),
            prunes: vec![prune],
            permit: None,
        })
        .unwrap();
    assert_eq!(fixture.archive.stats().buckets_pruned, 15);
    let mut reader = fixture.reader();
    let read = |key: &MetricKey| dgxtop_core::overview_history::ArchivedMinutesRead {
        series: SeriesInEveryBoot::from(key),
        from_utc_ns: 0,
        segments_before_utc_ns: utc_i64(now),
        max_minutes: 100,
    };
    let left = reader.archived_minutes(&read(&boot_a)).unwrap().minutes;
    assert_eq!(left.len(), 5);
    assert_eq!(left[0].start_utc_ns, utc_i64(now - 45 * MINUTE));
    assert_eq!(
        reader
            .archived_minutes(&read(&power))
            .unwrap()
            .minutes
            .len(),
        10
    );
}
