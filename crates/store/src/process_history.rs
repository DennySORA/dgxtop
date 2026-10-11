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
//! A failed call maps like every partition's (`sqlite::Writer`):
//! corruption stops the database until the next start recreates it. A
//! bucket packs 12 bytes a metric.
//!
//! Nothing here decides what is kept (D-94): the history's journal says
//! which buckets and runs go and its requests what to read. The one bound
//! of the store's own is the small database's page cap (`max_page_count`):
//! a write past it fails with `DiskFull` and deletes nothing. No
//! `CapacityPolicy::admit` runs before a write: the database is best
//! effort and reserves room for no outcome, so admission would protect
//! nothing the page cap does not refuse already, and the history's byte
//! budget gives runs way below it.

mod database;
mod recent;

use std::path::{Path, PathBuf};

use dgxtop_core::errors::InfraError;
use dgxtop_core::identity::ProcessKey;
use dgxtop_core::ports::ProcessHistoryStore;
use dgxtop_core::process_history::{
    BucketRequest, ProcessHistoryJournal, ProcessHistoryLoad, ProcessHistoryLoadRequest,
    RecentHistory, RunBucketRow, StoreUsage,
};
use rusqlite::Connection;

use crate::fs::{FileSystemOps, RealFileSystem};
use crate::sqlite::{
    DatabaseFailure, SchemaKind, VersionGate, Writer, ensure_sqlite_version, open_small_database,
    path_problem,
};

/// The process history's stores in dgxtop's state directory.
pub struct ProcessHistoryFiles {
    database: Writer,
    recreated: Option<String>,
    /// The short-term file, or why its path cannot be used.
    recent_path: Result<PathBuf, String>,
    fs: Box<dyn FileSystemOps>,
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
    pub fn open(database_path: &Path, recent_path: &Path) -> Self {
        Self::open_with(
            database_path,
            recent_path,
            Box::new(RealFileSystem),
            ensure_sqlite_version,
        )
    }

    pub(crate) fn open_with(
        database_path: &Path,
        recent_path: &Path,
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
}

impl ProcessHistoryStore for ProcessHistoryFiles {
    fn load(
        &mut self,
        request: &ProcessHistoryLoadRequest,
    ) -> Result<ProcessHistoryLoad, InfraError> {
        let (runs, buckets) =
            self.with_database(|connection| database::load(connection, request))?;
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

    fn append(&mut self, journal: &ProcessHistoryJournal) -> Result<StoreUsage, InfraError> {
        self.with_database(|connection| database::append(connection, journal))
    }

    fn save_recent(&mut self, recent: &RecentHistory) -> Result<(), InfraError> {
        recent::write(self.fs.as_ref(), self.recent_path()?, recent)
    }

    fn read_run(
        &mut self,
        key: &ProcessKey,
        buckets: &BucketRequest,
    ) -> Result<Vec<RunBucketRow>, InfraError> {
        self.with_database(|connection| database::read_run(connection, key, buckets))
    }
}

#[cfg(test)]
mod tests;
