//! The Overview History's store (D-82): a SQLite database of dgxtop's own
//! (`history_tiers.sqlite3`, its own `user_version` 1) holding each
//! Overview series' closed buckets of every bar size, the latest
//! [`dgxtop_core::process_history::TIER_BUCKETS`] of each (series, size), as
//! the history asks ([`TierJournal::keep_latest`]).
//!
//! Opened like the other partitions (0600, no symlinks, format, schema and
//! integrity checked before any write, a file of another format recreated
//! (D-85), `max_page_count`); one still unusable makes every call fail with
//! its reason and the history goes on in memory.
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
    CapacityPolicy, ConnectionProfile, DatabaseSpec, SchemaKind, VersionGate,
    ensure_sqlite_version, open_database, path_problem,
};

const MIB: u64 = 1024 * 1024;
/// The database, its WAL and shared memory together.
const LIMIT_BYTES: u64 = 64 * MIB;
const WAL_CHECKPOINT_BYTES: u64 = 4 * MIB;
const WAL_STOP_BYTES: u64 = 16 * MIB;
const MIN_FREE_DISK_BYTES: u64 = 64 * MIB;
/// The pages in use past which the series written longest ago give way:
/// room for the WAL below the limit. A full series (every size at 300
/// buckets) takes about a quarter of a megabyte.
const MAX_USED_BYTES: u64 = 48 * MIB;

/// The tiers database in dgxtop's state directory.
pub struct SqliteHistoryTiers {
    database: Result<Connection, String>,
    recreated: Option<String>,
    max_used_bytes: u64,
}

impl std::fmt::Debug for SqliteHistoryTiers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteHistoryTiers")
            .field("database", &self.database.as_ref().map(|_| "open"))
            .finish_non_exhaustive()
    }
}

impl SqliteHistoryTiers {
    /// Opens (creating when missing) the database at `path`. Never fails:
    /// an unusable database is reported by every call.
    pub fn open(path: &Path) -> Self {
        Self::open_with(path, &RealFileSystem, ensure_sqlite_version)
    }

    pub(crate) fn open_with(path: &Path, fs: &dyn FileSystemOps, gate: VersionGate) -> Self {
        let spec = DatabaseSpec {
            path,
            kind: SchemaKind::HistoryTiers,
            profile: ConnectionProfile::best_effort(WAL_CHECKPOINT_BYTES),
            policy: CapacityPolicy {
                limit_bytes: LIMIT_BYTES,
                reserve_bytes: 0,
                wal_checkpoint_bytes: WAL_CHECKPOINT_BYTES,
                wal_stop_bytes: WAL_STOP_BYTES,
                min_free_disk_bytes: MIN_FREE_DISK_BYTES,
            },
        };
        let opened = match path_problem(path) {
            Some(problem) => Err(format!("{}: {problem}", path.display())),
            None => open_database(&spec, fs, gate).map_err(|unusable| unusable.reason),
        };
        let (database, recreated) = match opened {
            Ok(opened) => (Ok(opened.connection), opened.recreated),
            Err(reason) => (Err(reason), None),
        };
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
        self.database
            .as_mut()
            .map_err(|reason| InfraError::Unavailable(format!("history tiers database: {reason}")))
    }

    #[cfg(test)]
    fn with_max_used_bytes(mut self, bytes: u64) -> Self {
        self.max_used_bytes = bytes;
        self
    }
}

impl HistoryTierStore for SqliteHistoryTiers {
    fn load(&mut self) -> Result<Vec<StoredTierSeries>, InfraError> {
        database::load(self.connection()?)
    }

    fn append(&mut self, journal: &TierJournal) -> Result<(), InfraError> {
        let max_used_bytes = self.max_used_bytes;
        let connection = self.connection()?;
        database::append(connection, journal)?;
        database::retain(connection, max_used_bytes)
    }
}

#[cfg(test)]
mod tests;
