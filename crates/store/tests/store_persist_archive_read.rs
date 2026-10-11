//! Reading the history archive back (D-51, D-82): the series earlier runs
//! left and their minutes from every boot, oldest first, a page at a time,
//! for the history's startup compression; units that never mix; a missing,
//! unready or refused file; and the prunes the compression asks for.

// Test helpers panic on setup failures; the lints target production code.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::history::HistoryMethod;
use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::identity::{BootId, HostId, MetricKey};
use dgxtop_core::metric::CanonicalUnit;
use dgxtop_core::ports::{
    ArchiveJournal, ArchiveReader, ArchiveSink, ArchivedBucket, ArchivedSegment,
};
use dgxtop_core::quality::SampleKind;
use dgxtop_core::testing::{FakeClock, metric_key, source_id};
use dgxtop_store::archive::{ArchiveLimits, SqliteArchive, SqliteArchiveReader};
use dgxtop_store::sqlite::{StatvfsFreeSpace, format_stamp};
use uuid::Uuid;

const MINUTE: u64 = 60_000_000_000;
const SECOND: u64 = 1_000_000_000;

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

fn minutes_of(key: &MetricKey, before: u64) -> dgxtop_core::overview_history::ArchivedMinutesRead {
    dgxtop_core::overview_history::ArchivedMinutesRead {
        series: SeriesInEveryBoot::from(key),
        from_utc_ns: 0,
        segments_before_utc_ns: i64::try_from(before).unwrap(),
        max_minutes: 100,
    }
}

#[test]
fn a_missing_or_unready_archive_holds_no_minutes() {
    let dir = TempDir::new("missing");
    let key = temperature_in("boot-a");
    let mut reader = SqliteArchiveReader::new(&dir.database());
    let read = minutes_of(&key, 10 * MINUTE);
    assert!(reader.archived_minutes(&read).unwrap().minutes.is_empty());
    assert!(
        !dir.database().exists(),
        "the reader never creates the file"
    );
    // A file the writer has not given its schema yet.
    rusqlite::Connection::open(dir.database()).unwrap();
    assert!(reader.archived_minutes(&read).unwrap().minutes.is_empty());
}

#[test]
fn a_schema_this_build_does_not_know_is_refused_and_left_alone() {
    let dir = TempDir::new("future");
    let connection = rusqlite::Connection::open(dir.database()).unwrap();
    connection.pragma_update(None, "user_version", 3).unwrap();
    drop(connection);
    let key = temperature_in("boot-a");
    let mut reader = SqliteArchiveReader::new(&dir.database());
    let error = reader
        .archived_minutes(&minutes_of(&key, 10 * MINUTE))
        .unwrap_err();
    assert!(error.to_string().contains("another format"), "{error:?}");
    let version: i64 = rusqlite::Connection::open(dir.database())
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 3);
}

/// The reader checks the file like the writer does (D-85): an archive of
/// this schema but another format version, or none, is not read, and the
/// reader leaves it for the writer to recreate.
#[test]
fn an_archive_of_another_format_is_not_read() {
    let current = dgxtop_store::sqlite::SchemaKind::History.format_version();
    for (label, stamp) in [
        ("unstamped", 0),
        ("newer", format_stamp(current + 1)),
        ("older", format_stamp(current - 1)),
    ] {
        let mut fixture = Fixture::new(label);
        let now = fixture.now_utc();
        let key = temperature_in("boot-a");
        let run = Run {
            key: &key,
            segment_id: "seg-a",
            unit: CanonicalUnit::DegC,
            start_utc: now - 30 * MINUTE,
            start_boot: MINUTE,
        };
        fixture.write(&run, &[Some(40.0); 3]);
        rusqlite::Connection::open(fixture.dir.database())
            .unwrap()
            .pragma_update(None, "application_id", stamp)
            .unwrap();
        let error = fixture
            .reader()
            .archived_minutes(&minutes_of(&key, now))
            .unwrap_err();
        assert!(
            matches!(error, dgxtop_core::errors::InfraError::Unavailable(_)),
            "{label}: {error:?}"
        );
        assert!(
            error.to_string().contains("another format"),
            "{label}: {error}"
        );
        let left: i32 = rusqlite::Connection::open(fixture.dir.database())
            .unwrap()
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .unwrap();
        assert_eq!(left, stamp, "{label}: the reader never writes");
    }
}

/// Units never mix: a segment of the series in another unit is not read.
#[test]
fn other_units_are_never_read() {
    let mut fixture = Fixture::new("units");
    let now = fixture.now_utc();
    let key = temperature_in("boot-a");
    for (segment_id, unit, value) in [
        ("seg-c", CanonicalUnit::DegC, 50.0),
        ("seg-k", CanonicalUnit::Count, 9.0),
    ] {
        let run = Run {
            key: &key,
            segment_id,
            unit,
            start_utc: now - 30 * MINUTE,
            start_boot: MINUTE,
        };
        fixture.write(&run, &[Some(value); 3]);
    }
    let minutes = fixture
        .reader()
        .archived_minutes(&minutes_of(&key, now))
        .unwrap()
        .minutes;
    assert_eq!(minutes.len(), 3);
    assert!(minutes.iter().all(|minute| minute.mean() == Some(50.0)));
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
