//! Connection opening and per-partition PRAGMA profiles.

use std::path::Path;
use std::time::Duration;

use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags};

/// `PRAGMA synchronous` level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Synchronous {
    /// Audit: an intent is durable once COMMIT returns.
    Full,
    /// The archive, the tiers and the process history: power loss may drop
    /// the most recent transactions (declared).
    Normal,
}

impl Synchronous {
    pub(crate) const fn pragma_value(self) -> i64 {
        match self {
            Self::Full => 2,
            Self::Normal => 1,
        }
    }
}

/// PRAGMA settings of one partition's writer connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectionProfile {
    pub synchronous: Synchronous,
    /// Page cache size in KiB (applied as a negative `cache_size`).
    pub cache_size_kib: u32,
    pub busy_timeout: Duration,
    /// WAL size SQLite truncates back to after a checkpoint reset.
    pub journal_size_limit_bytes: u64,
}

impl ConnectionProfile {
    /// Audit: FULL, 2 MiB cache, 100 ms busy timeout.
    pub(crate) const fn audit(journal_size_limit_bytes: u64) -> Self {
        Self {
            synchronous: Synchronous::Full,
            cache_size_kib: 2048,
            busy_timeout: Duration::from_millis(100),
            journal_size_limit_bytes,
        }
    }

    /// The archive, the tiers and the process history: NORMAL, 4 MiB cache,
    /// 50 ms busy timeout.
    pub(crate) const fn best_effort(journal_size_limit_bytes: u64) -> Self {
        Self {
            synchronous: Synchronous::Normal,
            cache_size_kib: 4096,
            busy_timeout: Duration::from_millis(50),
            journal_size_limit_bytes,
        }
    }
}

/// Read-write, existing file only (we pre-create it 0600), no symlink, no
/// URI interpretation, no per-connection mutex (the writer is single
/// threaded), extended result codes.
pub(crate) fn open_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
        | OpenFlags::SQLITE_OPEN_EXRESCODE
}

/// Opens the writer connection with hardening that needs no file access.
pub(crate) fn open_connection(path: &Path, busy_timeout: Duration) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(path, open_flags())?;
    // Defensive mode forbids SQL that could deliberately corrupt the file;
    // an untrusted schema keeps functions out of schema objects.
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    connection.busy_timeout(busy_timeout)?;
    Ok(connection)
}

/// Why a profile could not be applied.
#[derive(Debug)]
pub(crate) enum ProfileError {
    Sqlite(rusqlite::Error),
    /// A PRAGMA silently kept another value (e.g. WAL unsupported).
    NotApplied {
        pragma: &'static str,
        found: String,
    },
}

impl From<rusqlite::Error> for ProfileError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "{error}"),
            Self::NotApplied { pragma, found } => {
                write!(formatter, "PRAGMA {pragma} was not applied (found {found})")
            }
        }
    }
}

/// Applies WAL, synchronous, cache, foreign keys, manual checkpoints, the
/// journal size limit and the main-file page cap; verifies what it can.
/// Returns the effective `max_page_count`.
pub(crate) fn apply_profile(
    connection: &Connection,
    profile: &ConnectionProfile,
    max_page_count: u64,
) -> Result<u64, ProfileError> {
    let journal_mode: String =
        connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(ProfileError::NotApplied {
            pragma: "journal_mode",
            found: journal_mode,
        });
    }
    connection.pragma_update(None, "synchronous", profile.synchronous.pragma_value())?;
    verify_integer(
        connection,
        "synchronous",
        profile.synchronous.pragma_value(),
    )?;
    connection.pragma_update(None, "cache_size", -i64::from(profile.cache_size_kib))?;
    connection.pragma_update(None, "foreign_keys", true)?;
    verify_integer(connection, "foreign_keys", 1)?;
    // Checkpoints are issued explicitly by the writer at our thresholds.
    connection.pragma_update(None, "wal_autocheckpoint", 0)?;
    let journal_limit = i64::try_from(profile.journal_size_limit_bytes).unwrap_or(i64::MAX);
    connection.pragma_update(None, "journal_size_limit", journal_limit)?;
    let requested_pages = i64::try_from(max_page_count).unwrap_or(i64::MAX);
    let effective: i64 = connection.query_row(
        &format!("PRAGMA max_page_count = {requested_pages}"),
        [],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(effective).unwrap_or(0))
}

fn verify_integer(
    connection: &Connection,
    pragma: &'static str,
    expected: i64,
) -> Result<(), ProfileError> {
    let found: i64 = connection.pragma_query_value(None, pragma, |row| row.get(0))?;
    if found == expected {
        Ok(())
    } else {
        Err(ProfileError::NotApplied {
            pragma,
            found: found.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{FileSystemOps, RealFileSystem};

    fn temp_database(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "dgxtop-sqlite-conn-{}-{label}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.sqlite3");
        RealFileSystem.create_private_file(&path).unwrap();
        (dir, path)
    }

    #[test]
    fn extension_loading_and_symlinks_are_refused() {
        let (dir, path) = temp_database("hardening");
        let connection = open_connection(&path, Duration::from_millis(100)).unwrap();
        let loaded: rusqlite::Result<i64> =
            connection.query_row("SELECT load_extension('libnothing')", [], |row| row.get(0));
        assert!(loaded.is_err());
        assert!(
            connection
                .db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE)
                .unwrap()
        );
        drop(connection);
        let link = dir.join("link.sqlite3");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(open_connection(&link, Duration::from_millis(100)).is_err());
        assert!(open_connection(&dir.join("missing.sqlite3"), Duration::from_millis(100)).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
