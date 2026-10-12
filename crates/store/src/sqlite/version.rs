//! Runtime SQLite version gate (AT31).
//!
//! 3.51.3 is the conservative floor that contains the confirmed WAL-reset
//! fix; it is not a claim that newer versions are bug free. Older runtimes
//! (including distro builds with unverifiable backports) are refused so the
//! test matrix stays small.

use serde::Serialize;
use thiserror::Error;

/// Minimum accepted `sqlite3_libversion_number()` (3.51.3).
pub const MIN_SQLITE_VERSION_NUMBER: i32 = 3_051_003;

/// Linked SQLite library facts for diagnostics (`--version --verbose`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SqliteVersion {
    pub runtime_number: i32,
    pub runtime_text: String,
    pub built_number: i32,
    /// `sqlite_source_id()`: check-in date and amalgamation hash.
    pub source_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SqliteGateError {
    #[error(
        "SQLite runtime {} is older than the required {}",
        version_text(*.found),
        version_text(*.required)
    )]
    TooOld { found: i32, required: i32 },
    #[error(
        "SQLite runtime {} differs from the build-time headers {}",
        version_text(*.runtime),
        version_text(*.built)
    )]
    BuildMismatch { built: i32, runtime: i32 },
}

/// Formats `3051003` as `3.51.3`.
pub(crate) fn version_text(number: i32) -> String {
    let major = number / 1_000_000;
    let minor = (number / 1_000) % 1_000;
    let patch = number % 1_000;
    format!("{major}.{minor}.{patch}")
}

/// Accepts `number >= 3051003`.
pub fn check_version_number(number: i32) -> Result<(), SqliteGateError> {
    if number >= MIN_SQLITE_VERSION_NUMBER {
        Ok(())
    } else {
        Err(SqliteGateError::TooOld {
            found: number,
            required: MIN_SQLITE_VERSION_NUMBER,
        })
    }
}

/// Floor check plus build/runtime consistency.
pub fn check_versions(runtime: i32, built: i32) -> Result<(), SqliteGateError> {
    check_version_number(runtime)?;
    if runtime == built {
        Ok(())
    } else {
        Err(SqliteGateError::BuildMismatch { built, runtime })
    }
}

/// Gate on the actually linked library; call before opening any database.
pub fn ensure_sqlite_version() -> Result<SqliteVersion, SqliteGateError> {
    let runtime_number = rusqlite::version_number();
    let built_number = rusqlite::ffi::SQLITE_VERSION_NUMBER;
    check_versions(runtime_number, built_number)?;
    Ok(SqliteVersion {
        runtime_number,
        runtime_text: rusqlite::version().to_owned(),
        built_number,
        source_id: source_id(),
    })
}

fn source_id() -> Option<String> {
    let connection = rusqlite::Connection::open_in_memory().ok()?;
    connection
        .query_row("SELECT sqlite_source_id()", [], |row| {
            row.get::<_, String>(0)
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_is_exactly_3_51_3() {
        assert_eq!(check_version_number(3_051_003), Ok(()));
        assert_eq!(
            check_version_number(3_051_002),
            Err(SqliteGateError::TooOld {
                found: 3_051_002,
                required: 3_051_003
            })
        );
        assert!(check_version_number(3_046_001).is_err());
        assert!(check_version_number(0).is_err());
    }
}
