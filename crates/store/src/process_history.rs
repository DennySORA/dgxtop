//! The process history's stores (D-76, D-77): a long-term SQLite database
//! of runs with the latest buckets of each size, 1 minute to 24 hours
//! ([`database`]), and a short-term JSON file of the live runs' 5 s
//! buckets and the buckets being filled, replaced atomically every 30 s
//! ([`recent`]).
//!
//! The database is opened like the other partitions (0600, no symlinks,
//! format, schema and integrity checked before any write, a file of
//! another format recreated (D-85), `max_page_count`); one still unusable
//! makes every call fail with its reason, and the history stays in memory.
//! A failed call maps like every partition's ([`crate::sqlite::Writer`]):
//! corruption stops the database until the next start recreates it. A
//! bucket packs 12 bytes a metric; a run keeps at most 300 buckets a size,
//! so at most about a megabyte however long it runs.
//! No run expires by age: past [`MAX_USED_BYTES`] the ended runs last seen
//! longest ago give way.

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
    DatabaseFailure, SchemaKind, VersionGate, Writer, ensure_sqlite_version, open_small_database,
    path_problem,
};

const MIB: u64 = 1024 * 1024;
/// The pages in use past which ended runs give way: room left for the WAL
/// and the live runs' growth below the 64 MiB of a small database.
const MAX_USED_BYTES: u64 = 48 * MIB;
/// Retention runs at most this often (boot clock).
const RETENTION_EVERY_NS: u64 = 3_600 * 1_000_000_000;

/// The process history's stores in dgxtop's state directory.
pub struct ProcessHistoryFiles {
    database: Writer,
    recreated: Option<String>,
    /// The short-term file, or why its path cannot be used.
    recent_path: Result<PathBuf, String>,
    fs: Box<dyn FileSystemOps>,
    clock: Arc<dyn TimeProvider>,
    last_retention_boot_ns: Option<u64>,
    max_used_bytes: u64,
}

impl std::fmt::Debug for ProcessHistoryFiles {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessHistoryFiles")
            .field("database", &self.database)
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
        let opened =
            open_small_database(database_path, SchemaKind::ProcessHistory, fs.as_ref(), gate);
        let (database, recreated) = Writer::new("process history", opened);
        let recent_path = match path_problem(recent_path) {
            Some(problem) => Err(format!("{}: {problem}", recent_path.display())),
            None => Ok(recent_path.to_path_buf()),
        };
        Self {
            database,
            recreated,
            recent_path,
            fs,
            clock,
            last_retention_boot_ns: None,
            max_used_bytes: MAX_USED_BYTES,
        }
    }

    /// Why the database found at open was deleted and created afresh
    /// (D-85), if it was.
    pub fn recreated(&self) -> Option<&str> {
        self.recreated.as_deref()
    }

    fn connection(&mut self) -> Result<&mut Connection, InfraError> {
        self.database.connection().map_err(|unusable| {
            InfraError::Unavailable(format!("process history database: {}", unusable.reason))
        })
    }

    /// Runs `call` on the database, mapping its failure (D-85: corruption
    /// stops the database).
    fn with_database<T>(
        &mut self,
        call: impl FnOnce(&mut Connection) -> Result<T, DatabaseFailure>,
    ) -> Result<T, InfraError> {
        let result = call(self.connection()?);
        result.map_err(|failure| self.database.fail(&failure))
    }

    fn recent_path(&self) -> Result<&Path, InfraError> {
        self.recent_path
            .as_deref()
            .map_err(|reason| InfraError::Unavailable(format!("process history file: {reason}")))
    }

    #[cfg(test)]
    fn with_max_used_bytes(mut self, bytes: u64) -> Self {
        self.max_used_bytes = bytes;
        self
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
        let max_used_bytes = self.max_used_bytes;
        self.with_database(|connection| database::retain(connection, max_used_bytes))?;
        self.last_retention_boot_ns = Some(now);
        Ok(())
    }
}

impl ProcessHistoryStore for ProcessHistoryFiles {
    fn load(&mut self) -> Result<ProcessHistoryLoad, InfraError> {
        self.maybe_retain()?;
        let (runs, buckets) = self.with_database(database::load)?;
        // A short-term file that cannot be read only loses the last minutes.
        let recent = self
            .recent_path()
            .and_then(|path| recent::read(self.fs.as_ref(), path))
            .ok();
        Ok(ProcessHistoryLoad {
            runs,
            buckets,
            recent,
        })
    }

    fn append(&mut self, journal: &ProcessHistoryJournal) -> Result<(), InfraError> {
        self.with_database(|connection| database::append(connection, journal))?;
        self.maybe_retain()
    }

    fn save_recent(&mut self, recent: &RecentHistory) -> Result<(), InfraError> {
        recent::write(self.fs.as_ref(), self.recent_path()?, recent)
    }

    fn read_run(&mut self, key: &ProcessKey) -> Result<Vec<RunBucketRow>, InfraError> {
        self.with_database(|connection| database::read_run(connection, key))
    }
}

#[cfg(test)]
mod tests;
