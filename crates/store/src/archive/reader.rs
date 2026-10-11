//! [`SqliteArchiveReader`]: the history view's reader of the archive
//! (D-51).
//!
//! It holds a read-only connection of its own (`query_only`), opened on the
//! first read after the writer has created the schema. A read finds the
//! series' segments in every boot (in the series' canonical unit, so units
//! never mix), reads each segment's gauge minutes that started in the
//! window with one short statement (no read pins the WAL for long), checks
//! the deadline and the cancel flag between rows, and merges the minutes
//! into points by the UTC grid; a point spans the minutes merged into it,
//! so it sits where they were recorded. Boot clocks differ between boots,
//! so the minutes are placed by their UTC stamps, which STORAGE.md §3
//! allows for display.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::errors::{ErrorCode, ErrorDetail};
use dgxtop_core::history::{ArchivePoint, ArchiveQuery, ArchiveResult};
use dgxtop_core::metric::descriptor;
use dgxtop_core::ports::{ArchiveReader, CancelFlag};
use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags, params};

use crate::sqlite::{FaultKind, SCHEMA_USER_VERSION, describe, fault_kind, parse_utc_rfc3339_ns};
use dgxtop_core::clock::format_utc_ns;

/// The writer holds the lock only for its short batches.
const BUSY_TIMEOUT: Duration = Duration::from_millis(50);
/// Rows read between two deadline and cancel checks.
const CHECK_INTERVAL: u64 = 512;
/// Segments one read merges: every boot and source generation of a week.
const MAX_SEGMENTS: usize = 256;
/// Minutes one read may return (a week of one series is 10 080).
const MAX_ROWS: u64 = 200_000;
const SELECT_SEGMENTS: &str = "SELECT segment_id, boot_id FROM history_segments \
     WHERE host_id = ?1 AND entity_id = ?2 AND metric_id = ?3 AND scope = ?4 AND unit = ?5 \
     LIMIT ?6";
/// `+start_utc` keeps the planner on the primary key: one segment's rows,
/// never every series' minutes of the window.
const SELECT_MINUTES: &str = "SELECT start_utc, end_utc, covered_ns, weighted_sum, maximum \
     FROM history_buckets \
     WHERE segment_id = ?1 AND +start_utc >= ?2 AND +start_utc < ?3 \
     AND method = 'time_weighted_step'";

/// What the reader knows of the archive file.
enum ArchiveFile {
    /// Not opened yet, or closed after a failure: opened on the next read.
    Closed,
    Open(Connection),
    /// A schema this build must not read (newer, or not the archive's).
    Refused(String),
}

/// The archive's reader for the history view.
pub struct SqliteArchiveReader {
    path: PathBuf,
    file: ArchiveFile,
}

impl std::fmt::Debug for SqliteArchiveReader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match &self.file {
            ArchiveFile::Closed => "closed".to_owned(),
            ArchiveFile::Open(_) => "open".to_owned(),
            ArchiveFile::Refused(reason) => format!("refused: {reason}"),
        };
        formatter
            .debug_struct("SqliteArchiveReader")
            .field("path", &self.path)
            .field("file", &state)
            .finish()
    }
}

/// How opening went.
enum Opened {
    Ready(Connection),
    /// No archive yet: the writer creates it.
    NotYet,
    Refused(String),
}

fn open(path: &Path) -> rusqlite::Result<Opened> {
    if !path.exists() {
        return Ok(Opened::NotYet);
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
        | OpenFlags::SQLITE_OPEN_EXRESCODE;
    let connection = Connection::open_with_flags(path, flags)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "query_only", true)?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    Ok(match version {
        SCHEMA_USER_VERSION => Opened::Ready(connection),
        // The writer applies the schema in one transaction right after
        // creating the file.
        0 => Opened::NotYet,
        other => Opened::Refused(format!(
            "history archive schema version {other} is not {SCHEMA_USER_VERSION}"
        )),
    })
}

impl SqliteArchiveReader {
    /// A reader of the archive at `path`; nothing is opened before a read.
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            file: ArchiveFile::Closed,
        }
    }

    /// The open connection, `None` while there is no archive yet.
    fn connection(&mut self) -> Result<Option<&Connection>, Failure> {
        if matches!(self.file, ArchiveFile::Closed) {
            match open(&self.path).map_err(Failure::Sqlite)? {
                Opened::Ready(connection) => self.file = ArchiveFile::Open(connection),
                Opened::NotYet => return Ok(None),
                Opened::Refused(reason) => self.file = ArchiveFile::Refused(reason),
            }
        }
        match &self.file {
            ArchiveFile::Open(connection) => Ok(Some(connection)),
            ArchiveFile::Refused(reason) => Err(Failure::Refused(reason.clone())),
            ArchiveFile::Closed => Ok(None),
        }
    }

    fn read_minutes(
        &mut self,
        query: &ArchiveQuery,
        guard: &mut ReadGuard<'_>,
    ) -> Result<ArchiveResult, Failure> {
        guard.checkpoint()?;
        let Some(connection) = self.connection()? else {
            return Ok(ArchiveResult::empty(query.request_id));
        };
        let segments = segments_of(connection, query)?;
        let bounds = utc_bounds(query);
        let mut points = PointSums::new(query);
        let mut statement = connection.prepare_cached(SELECT_MINUTES)?;
        for (segment_id, boot_id) in &segments.found {
            guard.checkpoint()?;
            let mut rows = statement.query(params![segment_id, bounds.0, bounds.1])?;
            while let Some(row) = rows.next()? {
                guard.charge()?;
                let minute = Minute {
                    start_utc: row.get(0)?,
                    end_utc: row.get(1)?,
                    covered_ns: row.get(2)?,
                    weighted_sum: row.get(3)?,
                    maximum: row.get(4)?,
                };
                points.add(&minute, boot_id);
            }
        }
        Ok(points.finish(query, segments.truncated))
    }
}

impl ArchiveReader for SqliteArchiveReader {
    fn read(
        &mut self,
        query: &ArchiveQuery,
        deadline_boot_ns: u64,
        cancel: &CancelFlag,
        clock: &dyn TimeProvider,
    ) -> Result<ArchiveResult, ErrorDetail> {
        let selector = format!(
            "{}|{}|{}",
            query.series.metric_id, query.series.scope, query.series.entity_id
        );
        let error = |code: ErrorCode, message: String| {
            ErrorDetail::new(code, message)
                .with_request(query.request_id)
                .with_affected(&selector)
        };
        query
            .validate()
            .map_err(|invalid| error(ErrorCode::ValidationFailed, invalid.to_string()))?;
        let mut guard = ReadGuard {
            deadline_ns: deadline_boot_ns,
            cancel,
            clock,
            rows: 0,
            since_check: 0,
        };
        self.read_minutes(query, &mut guard).map_err(|failure| {
            let (code, message) = failure.describe();
            if failure.closes_connection() {
                self.file = ArchiveFile::Closed;
            }
            error(code, message)
        })
    }
}

/// Why a read failed.
enum Failure {
    Sqlite(rusqlite::Error),
    Refused(String),
    Cancelled,
    TimedOut { rows: u64 },
    TooLarge,
}

impl From<rusqlite::Error> for Failure {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl Failure {
    fn describe(&self) -> (ErrorCode, String) {
        match self {
            Self::Sqlite(error) => {
                let code = if fault_kind(error) == FaultKind::Busy {
                    ErrorCode::Busy
                } else {
                    ErrorCode::Unavailable
                };
                (
                    code,
                    format!("history archive read failed: {}", describe(error)),
                )
            }
            Self::Refused(reason) => (ErrorCode::Unavailable, reason.clone()),
            Self::Cancelled => (
                ErrorCode::Cancelled,
                "history archive read cancelled".to_owned(),
            ),
            Self::TimedOut { rows } => (
                ErrorCode::TimedOut,
                format!("history archive read passed its deadline after {rows} minutes"),
            ),
            Self::TooLarge => (
                ErrorCode::QueryTooLarge,
                format!("history archive read would return more than {MAX_ROWS} minutes"),
            ),
        }
    }

    /// A SQLite failure other than a busy lock reopens the file next time.
    fn closes_connection(&self) -> bool {
        matches!(self, Self::Sqlite(error) if fault_kind(error) != FaultKind::Busy)
    }
}

/// Deadline, cancel flag and row budget of one read.
struct ReadGuard<'a> {
    deadline_ns: u64,
    cancel: &'a CancelFlag,
    clock: &'a dyn TimeProvider,
    rows: u64,
    since_check: u64,
}

impl ReadGuard<'_> {
    /// Fails with `Cancelled` or `TimedOut` (cancellation wins).
    fn checkpoint(&mut self) -> Result<(), Failure> {
        self.since_check = 0;
        if self.cancel.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        if self.clock.now_boot_ns() >= self.deadline_ns {
            return Err(Failure::TimedOut { rows: self.rows });
        }
        Ok(())
    }

    fn charge(&mut self) -> Result<(), Failure> {
        self.rows = self.rows.saturating_add(1);
        if self.rows > MAX_ROWS {
            return Err(Failure::TooLarge);
        }
        self.since_check = self.since_check.saturating_add(1);
        if self.since_check >= CHECK_INTERVAL {
            self.checkpoint()?;
        }
        Ok(())
    }
}

/// The series' segments in every boot.
struct Segments {
    found: Vec<(String, String)>,
    truncated: bool,
}

fn segments_of(connection: &Connection, query: &ArchiveQuery) -> Result<Segments, Failure> {
    let series = &query.series;
    let Some(known) = descriptor(series.metric_id.as_str()) else {
        return Ok(Segments {
            found: Vec::new(),
            truncated: false,
        });
    };
    let unit = known.unit.to_string();
    let limit = i64::try_from(MAX_SEGMENTS + 1).unwrap_or(i64::MAX);
    let mut statement = connection.prepare_cached(SELECT_SEGMENTS)?;
    let rows = statement.query_map(
        params![
            series.host_id.as_str(),
            series.entity_id.as_str(),
            series.metric_id.as_str(),
            series.scope.as_str(),
            unit,
            limit
        ],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;
    let mut found = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let truncated = found.len() > MAX_SEGMENTS;
    found.truncate(MAX_SEGMENTS);
    Ok(Segments { found, truncated })
}

/// The window as the archive's UTC text (`…T12:00:00.000000000Z`).
fn utc_bounds(query: &ArchiveQuery) -> (String, String) {
    let text = |utc_ns: u64| format_utc_ns(i128::from(utc_ns)).unwrap_or_default();
    (text(query.start_utc_ns), text(query.end_utc_ns))
}

/// One archived gauge minute as stored.
struct Minute {
    start_utc: String,
    end_utc: String,
    covered_ns: String,
    weighted_sum: Option<f64>,
    maximum: Option<f64>,
}

/// Running sums of one point: its minutes' extent, weighted sum, covered
/// time and maximum.
#[derive(Debug)]
struct PointSum {
    start_utc_ns: u64,
    end_utc_ns: u64,
    weighted_sum: f64,
    covered_ns: u64,
    maximum: Option<f64>,
}

/// Minutes merged into points on the UTC grid.
struct PointSums {
    point_ns: u64,
    sums: BTreeMap<u64, PointSum>,
    boots: BTreeSet<String>,
}

impl PointSums {
    fn new(query: &ArchiveQuery) -> Self {
        Self {
            point_ns: query.point_ns,
            sums: BTreeMap::new(),
            boots: BTreeSet::new(),
        }
    }

    /// Adds a minute to the point of the grid its start falls in; the point
    /// grows to span it. A minute without covered time or with a malformed
    /// stamp adds nothing.
    fn add(&mut self, minute: &Minute, boot_id: &str) {
        let utc = |text: &str| parse_utc_rfc3339_ns(text).and_then(|ns| u64::try_from(ns).ok());
        let (Some(start), Some(end)) = (utc(&minute.start_utc), utc(&minute.end_utc)) else {
            return;
        };
        let covered_ns = minute.covered_ns.parse::<u64>().unwrap_or(0);
        let Some(weighted_sum) = minute.weighted_sum.filter(|sum| sum.is_finite()) else {
            return;
        };
        if covered_ns == 0 || end <= start {
            return;
        }
        self.boots.insert(boot_id.to_owned());
        let sum = self
            .sums
            .entry(start - start % self.point_ns.max(1))
            .or_insert(PointSum {
                start_utc_ns: start,
                end_utc_ns: end,
                weighted_sum: 0.0,
                covered_ns: 0,
                maximum: None,
            });
        sum.start_utc_ns = sum.start_utc_ns.min(start);
        sum.end_utc_ns = sum.end_utc_ns.max(end);
        sum.weighted_sum += weighted_sum;
        sum.covered_ns = sum.covered_ns.saturating_add(covered_ns);
        if let Some(maximum) = minute.maximum.filter(|maximum| maximum.is_finite()) {
            sum.maximum = Some(sum.maximum.map_or(maximum, |known| known.max(maximum)));
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn finish(self, query: &ArchiveQuery, segments_truncated: bool) -> ArchiveResult {
        let cap = usize::try_from(query.max_points).unwrap_or(usize::MAX);
        let skip = self.sums.len().saturating_sub(cap);
        let points: Vec<ArchivePoint> = self
            .sums
            .into_values()
            .skip(skip)
            .map(|sum| ArchivePoint {
                start_utc_ns: sum.start_utc_ns,
                end_utc_ns: sum.end_utc_ns,
                mean: Some(sum.weighted_sum / sum.covered_ns as f64),
                max: sum.maximum,
                // Boots overlapping on the wall clock never claim more time
                // than the point spans.
                covered_ns: sum.covered_ns.min(sum.end_utc_ns - sum.start_utc_ns),
            })
            .collect();
        ArchiveResult {
            request_id: query.request_id,
            covered_ns: points
                .iter()
                .map(|point| point.covered_ns)
                .fold(0, u64::saturating_add),
            points,
            boots: u32::try_from(self.boots.len()).unwrap_or(u32::MAX),
            truncated: skip > 0 || segments_truncated,
        }
    }
}
