//! The process history's stores (D-76): a long-term SQLite database of runs
//! with their minute buckets for 24 hours and ten-minute buckets for 7 days
//! ([`database`]), and a short-term JSON file of the live runs' 5 s
//! buckets, replaced atomically every 30 s ([`recent`]).
//!
//! The database is opened like the other partitions (0600, no symlinks,
//! schema and integrity checked before any write, `max_page_count`); an
//! unusable one makes every call fail with its reason, and the history
//! stays in memory. A bucket packs its eight aggregates into 96 bytes, so a
//! run costs about 170 KB a day however busy it is.

mod database;
mod recent;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::errors::InfraError;
use dgxtop_core::identity::ProcessKey;
use dgxtop_core::ports::ProcessHistoryStore;
use dgxtop_core::process_history::{
    ProcessHistoryJournal, ProcessHistoryLoad, RecentHistory, RunBucketRow,
};
use rusqlite::Connection;

use crate::fs::{FileSystemOps, RealFileSystem};
use crate::sqlite::{
    CapacityPolicy, ConnectionProfile, DatabaseSpec, SchemaKind, VersionGate,
    ensure_sqlite_version, open_database,
};

const MIB: u64 = 1024 * 1024;
/// The database, its WAL and shared memory together.
const LIMIT_BYTES: u64 = 64 * MIB;
const WAL_CHECKPOINT_BYTES: u64 = 4 * MIB;
const WAL_STOP_BYTES: u64 = 16 * MIB;
const MIN_FREE_DISK_BYTES: u64 = 64 * MIB;
/// Retention runs at most this often (boot clock).
const RETENTION_EVERY_NS: u64 = 3_600 * 1_000_000_000;

/// The process history's stores in dgxtop's state directory.
pub struct ProcessHistoryFiles {
    database: Result<Connection, String>,
    recent_path: PathBuf,
    fs: Box<dyn FileSystemOps>,
    clock: Arc<dyn TimeProvider>,
    last_retention_boot_ns: Option<u64>,
}

impl std::fmt::Debug for ProcessHistoryFiles {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessHistoryFiles")
            .field("database", &self.database.as_ref().map(|_| "open"))
            .field("recent_path", &self.recent_path)
            .finish_non_exhaustive()
    }
}

impl ProcessHistoryFiles {
    /// Opens (creating when missing) the database at `database_path`; the
    /// short-term file lives at `recent_path`. Never fails: an unusable
    /// database is reported by every call.
    pub fn open(database_path: &Path, recent_path: &Path, clock: Arc<dyn TimeProvider>) -> Self {
        Self::open_with(
            database_path,
            recent_path,
            clock,
            Box::new(RealFileSystem),
            ensure_sqlite_version,
        )
    }

    pub(crate) fn open_with(
        database_path: &Path,
        recent_path: &Path,
        clock: Arc<dyn TimeProvider>,
        fs: Box<dyn FileSystemOps>,
        gate: VersionGate,
    ) -> Self {
        let spec = DatabaseSpec {
            path: database_path,
            kind: SchemaKind::ProcessHistory,
            profile: ConnectionProfile::archive(WAL_CHECKPOINT_BYTES),
            policy: CapacityPolicy {
                limit_bytes: LIMIT_BYTES,
                reserve_bytes: 0,
                wal_checkpoint_bytes: WAL_CHECKPOINT_BYTES,
                wal_stop_bytes: WAL_STOP_BYTES,
                min_free_disk_bytes: MIN_FREE_DISK_BYTES,
            },
        };
        let database = open_database(&spec, fs.as_ref(), clock.as_ref(), gate)
            .map(|opened| opened.connection)
            .map_err(|unusable| unusable.reason);
        Self {
            database,
            recent_path: recent_path.to_path_buf(),
            fs,
            clock,
            last_retention_boot_ns: None,
        }
    }

    fn connection(&mut self) -> Result<&mut Connection, InfraError> {
        self.database.as_mut().map_err(|reason| {
            InfraError::Unavailable(format!("process history database: {reason}"))
        })
    }

    fn now_utc_ns(&self) -> i64 {
        i64::try_from(self.clock.now_utc_ns()).unwrap_or(i64::MAX)
    }

    /// Retention when it is due: at the first call and then hourly.
    fn maybe_retain(&mut self) -> Result<(), InfraError> {
        let now = self.clock.now_boot_ns();
        let due = self
            .last_retention_boot_ns
            .is_none_or(|last| now.saturating_sub(last) >= RETENTION_EVERY_NS);
        if !due {
            return Ok(());
        }
        let utc = self.now_utc_ns();
        database::retain(self.connection()?, utc)?;
        self.last_retention_boot_ns = Some(now);
        Ok(())
    }
}

impl ProcessHistoryStore for ProcessHistoryFiles {
    fn load(&mut self) -> Result<ProcessHistoryLoad, InfraError> {
        self.maybe_retain()?;
        let (runs, buckets) = database::load(self.connection()?)?;
        // A short-term file that cannot be read only loses the last minutes.
        let recent = recent::read(self.fs.as_ref(), &self.recent_path).ok();
        Ok(ProcessHistoryLoad {
            runs,
            buckets,
            recent,
        })
    }

    fn append(&mut self, journal: &ProcessHistoryJournal) -> Result<(), InfraError> {
        database::append(self.connection()?, journal)?;
        self.maybe_retain()
    }

    fn save_recent(&mut self, recent: &RecentHistory) -> Result<(), InfraError> {
        recent::write(self.fs.as_ref(), &self.recent_path, recent)
    }

    fn read_run(&mut self, key: &ProcessKey) -> Result<Vec<RunBucketRow>, InfraError> {
        database::read_run(self.connection()?, key)
    }
}

#[cfg(test)]
mod tests;
