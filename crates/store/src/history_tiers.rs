//! The Overview History's store (D-82): a SQLite database of dgxtop's own
//! (`history_tiers.sqlite3`, its own `user_version` 1, format 2) holding each
//! Overview series' closed buckets of every bar size, the latest
//! [`dgxtop_core::tiers::TIER_BUCKETS`] of each (series, size), as
//! the history asks ([`TierJournal::keep_latest`]).
//!
//! Opened like the other partitions (0600, no symlinks, format, schema and
//! integrity checked before any write, a file of another format recreated
//! (D-85), `max_page_count`); one still unusable makes every call fail with
//! its reason and the history goes on in memory. A failed call maps like
//! every partition's (`sqlite::Writer`): corruption stops the
//! database until the next start recreates it.
//!
//! Nothing here decides what is kept (D-95): the history's journal says
//! which series go, and each append returns the pages in use and the
//! series held for the history's byte budget. The one bound of the store's
//! own is the small database's page cap (`max_page_count`): a write past it
//! fails with `DiskFull` and deletes nothing. No `CapacityPolicy::admit`
//! runs before a write: the database is best effort and reserves room for
//! no outcome, so admission would protect nothing the page cap does not
//! refuse already, and the history's budget lets series go below it.

mod database;

use dgxtop_core::errors::InfraError;
use dgxtop_core::overview_history::{StoredTierSeries, TierJournal, TierStoreUsage};
use dgxtop_core::ports::HistoryTierStore;
use rusqlite::Connection;
use std::path::Path;

use crate::fs::{FileSystemOps, RealFileSystem};
use crate::sqlite::{
    DatabaseFailure, SchemaKind, VersionGate, Writer, ensure_sqlite_version, open_small_database,
};

/// The tiers database in dgxtop's state directory.
#[derive(Debug)]
pub struct SqliteHistoryTiers {
    database: Writer,
    recreated: Option<String>,
}

impl SqliteHistoryTiers {
    /// Opens (creating when missing) the database at `path`. Never fails:
    /// an unusable database is reported by every call.
    pub fn open(path: &Path) -> Self {
        Self::open_with(path, &RealFileSystem, ensure_sqlite_version)
    }

    pub(crate) fn open_with(path: &Path, fs: &dyn FileSystemOps, gate: VersionGate) -> Self {
        let opened = open_small_database(path, SchemaKind::HistoryTiers, fs, gate);
        let (database, recreated) = Writer::new("history tiers", opened);
        Self {
            database,
            recreated,
        }
    }

    /// Why the database found at open was deleted and created afresh
    /// (D-85), if it was.
    pub fn recreated(&self) -> Option<&str> {
        self.recreated.as_deref()
    }

    fn connection(&mut self) -> Result<&mut Connection, InfraError> {
        self.database.connection().map_err(|unusable| {
            InfraError::Unavailable(format!("history tiers database: {}", unusable.reason))
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
}

impl HistoryTierStore for SqliteHistoryTiers {
    fn load(&mut self) -> Result<Vec<StoredTierSeries>, InfraError> {
        self.with_database(database::load)
    }

    fn append(&mut self, journal: &TierJournal) -> Result<TierStoreUsage, InfraError> {
        self.with_database(|connection| database::append(connection, journal))
    }
}

#[cfg(test)]
mod tests;
