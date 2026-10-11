//! The Overview History's store (D-82): a SQLite database of dgxtop's own
//! (`history_tiers.sqlite3`, its own `user_version` 1) holding each
//! Overview series' closed buckets of every bar size, the latest
//! [`dgxtop_core::process_history::TIER_BUCKETS`] of each (series, size), as
//! the history asks ([`TierJournal::keep_latest`]).
//!
//! Opened like the other partitions (0600, no symlinks, format, schema and
//! integrity checked before any write, a file of another format recreated
//! (D-85), `max_page_count`); one still unusable makes every call fail with
//! its reason and the history goes on in memory. A failed call maps like
//! every partition's ([`crate::sqlite::Writer`]): corruption stops the
//! database until the next start recreates it.
//! No series expires by age: past [`MAX_USED_BYTES`] the series written
//! longest ago give way.

mod database;

use dgxtop_core::errors::InfraError;
use dgxtop_core::overview_history::{StoredTierSeries, TierJournal};
use dgxtop_core::ports::HistoryTierStore;
use rusqlite::Connection;
use std::path::Path;

use crate::fs::{FileSystemOps, RealFileSystem};
use crate::sqlite::{
    DatabaseFailure, SchemaKind, VersionGate, Writer, ensure_sqlite_version, open_small_database,
};

const MIB: u64 = 1024 * 1024;
/// The pages in use past which the series written longest ago give way:
/// room for the WAL below the 64 MiB of a small database. A full series
/// (every size at 300 buckets) takes about a quarter of a megabyte.
const MAX_USED_BYTES: u64 = 48 * MIB;

/// The tiers database in dgxtop's state directory.
#[derive(Debug)]
pub struct SqliteHistoryTiers {
    database: Writer,
    recreated: Option<String>,
    max_used_bytes: u64,
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

    #[cfg(test)]
    fn with_max_used_bytes(mut self, bytes: u64) -> Self {
        self.max_used_bytes = bytes;
        self
    }
}

impl HistoryTierStore for SqliteHistoryTiers {
    fn load(&mut self) -> Result<Vec<StoredTierSeries>, InfraError> {
        self.with_database(database::load)
    }

    fn append(&mut self, journal: &TierJournal) -> Result<(), InfraError> {
        let max_used_bytes = self.max_used_bytes;
        self.with_database(|connection| {
            database::append(connection, journal)?;
            database::retain(connection, max_used_bytes)
        })
    }
}

#[cfg(test)]
mod tests;
