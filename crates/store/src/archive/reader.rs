//! [`SqliteArchiveReader`]: the reader of the archive (D-51) through which
//! the history compresses the Overview series' archived minutes at startup
//! (D-82, [`super::backfill`]): the series earlier runs left, then each
//! one's minutes oldest first, a bounded page a read.
//!
//! It holds a read-only connection of its own (`query_only`), opened on the
//! first read after the writer has created the schema; each read is one
//! short statement, so no read pins the WAL for long. Boot clocks differ
//! between boots, so the minutes are placed by their UTC stamps, which
//! STORAGE.md §3 allows for display.

use std::path::{Path, PathBuf};
use std::time::Duration;

use dgxtop_core::errors::InfraError;
use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::overview_history::{ArchivedMinutes, ArchivedMinutesRead, ArchivedSeriesList};
use dgxtop_core::ports::ArchiveReader;
use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags};

use crate::sqlite::{FaultKind, SCHEMA_USER_VERSION, describe, fault_kind};

/// The writer holds the lock only for its short batches.
const BUSY_TIMEOUT: Duration = Duration::from_millis(50);

/// What the reader knows of the archive file.
enum ArchiveFile {
    /// Not opened yet, or closed after a failure: opened on the next read.
    Closed,
    Open(Connection),
    /// A schema this build must not read (newer, or not the archive's).
    Refused(String),
}

/// The archive's reader.
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

/// Why a read failed.
enum Failure {
    Sqlite(rusqlite::Error),
    Refused(String),
}

impl Failure {
    fn into_infra(self) -> InfraError {
        match self {
            Self::Sqlite(error) if fault_kind(&error) == FaultKind::Busy => {
                InfraError::Unavailable(format!(
                    "history archive read failed: {}",
                    describe(&error)
                ))
            }
            Self::Sqlite(error) => {
                InfraError::Database(format!("history archive read failed: {}", describe(&error)))
            }
            Self::Refused(reason) => InfraError::Unavailable(reason),
        }
    }

    /// A SQLite failure other than a busy lock reopens the file next time.
    fn closes_connection(&self) -> bool {
        matches!(self, Self::Sqlite(error) if fault_kind(error) != FaultKind::Busy)
    }
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

    /// Runs `read` on the open archive (`empty` without one); a broken
    /// connection is opened again next time.
    fn with_archive<T>(
        &mut self,
        empty: T,
        read: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> Result<T, InfraError> {
        let result = match self.connection() {
            Ok(Some(connection)) => read(connection).map_err(Failure::Sqlite),
            Ok(None) => Ok(empty),
            Err(failure) => Err(failure),
        };
        result.map_err(|failure| {
            if failure.closes_connection() {
                self.file = ArchiveFile::Closed;
            }
            failure.into_infra()
        })
    }
}

impl ArchiveReader for SqliteArchiveReader {
    fn archived_series(
        &mut self,
        list: &ArchivedSeriesList,
    ) -> Result<Vec<SeriesInEveryBoot>, InfraError> {
        self.with_archive(Vec::new(), |connection| {
            super::backfill::series(connection, list)
        })
    }

    fn archived_minutes(
        &mut self,
        read: &ArchivedMinutesRead,
    ) -> Result<ArchivedMinutes, InfraError> {
        let empty = ArchivedMinutes {
            minutes: Vec::new(),
            more: false,
        };
        self.with_archive(empty, |connection| {
            super::backfill::minutes(connection, read)
        })
    }
}
