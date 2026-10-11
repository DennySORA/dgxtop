//! Classification of SQLite failures into the few classes persistence
//! policy distinguishes (busy, full, corrupt, I/O, constraint, ...).

use dgxtop_core::errors::InfraError;
use rusqlite::ErrorCode;

use super::transaction::TxError;

/// What a SQLite failure means for the partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FaultKind {
    /// Another connection holds the lock (`SQLITE_BUSY`/`SQLITE_LOCKED`).
    Busy,
    /// `SQLITE_FULL`: `ENOSPC` or `max_page_count` reached.
    Full,
    /// `SQLITE_CORRUPT`/`SQLITE_NOTADB`: the file must be kept and the
    /// partition stopped.
    Corrupt,
    /// `SQLITE_IOERR`: during COMMIT the outcome is uncertain.
    Io,
    /// A schema constraint rejected the row.
    Constraint,
    /// Read-only file system or permissions.
    ReadOnly,
    /// The database file could not be opened.
    CannotOpen,
    Other,
}

pub(crate) fn fault_kind(error: &rusqlite::Error) -> FaultKind {
    let rusqlite::Error::SqliteFailure(failure, _) = error else {
        return FaultKind::Other;
    };
    match failure.code {
        ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => FaultKind::Busy,
        ErrorCode::DiskFull => FaultKind::Full,
        ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => FaultKind::Corrupt,
        ErrorCode::SystemIoFailure => FaultKind::Io,
        ErrorCode::ConstraintViolation => FaultKind::Constraint,
        ErrorCode::ReadOnly | ErrorCode::PermissionDenied => FaultKind::ReadOnly,
        ErrorCode::CannotOpen => FaultKind::CannotOpen,
        _ => FaultKind::Other,
    }
}

/// Sanitized one-line description for statuses and error messages.
pub(crate) fn describe(error: &rusqlite::Error) -> String {
    dgxtop_core::text::sanitize_message(&error.to_string())
}

/// A failed statement of `operation`.
pub(crate) fn infra_error(operation: &'static str, error: &rusqlite::Error) -> InfraError {
    InfraError::Database(format!("{operation}: {}", describe(error)))
}

/// A failed transaction of `operation`.
pub(crate) fn tx_infra_error(operation: &'static str, error: TxError<()>) -> InfraError {
    match error {
        TxError::Sqlite(error) | TxError::Commit(error) => infra_error(operation, &error),
        TxError::BusyTimeout { elapsed_ms } => InfraError::Timeout {
            operation,
            elapsed_ms,
        },
        TxError::Abort(()) => InfraError::Database(format!("{operation}: aborted")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(code: i32) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), Some("detail".to_owned()))
    }

    #[test]
    fn maps_primary_result_codes() {
        assert_eq!(
            fault_kind(&failure(rusqlite::ffi::SQLITE_BUSY)),
            FaultKind::Busy
        );
        assert_eq!(
            fault_kind(&failure(rusqlite::ffi::SQLITE_LOCKED)),
            FaultKind::Busy
        );
        assert_eq!(
            fault_kind(&failure(rusqlite::ffi::SQLITE_FULL)),
            FaultKind::Full
        );
        assert_eq!(
            fault_kind(&failure(rusqlite::ffi::SQLITE_CORRUPT)),
            FaultKind::Corrupt
        );
        assert_eq!(
            fault_kind(&failure(rusqlite::ffi::SQLITE_NOTADB)),
            FaultKind::Corrupt
        );
        assert_eq!(
            fault_kind(&failure(rusqlite::ffi::SQLITE_IOERR_FSYNC)),
            FaultKind::Io
        );
        assert_eq!(
            fault_kind(&failure(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE)),
            FaultKind::Constraint
        );
        assert_eq!(
            fault_kind(&failure(rusqlite::ffi::SQLITE_READONLY)),
            FaultKind::ReadOnly
        );
        assert_eq!(
            fault_kind(&failure(rusqlite::ffi::SQLITE_CANTOPEN)),
            FaultKind::CannotOpen
        );
        assert_eq!(
            fault_kind(&rusqlite::Error::QueryReturnedNoRows),
            FaultKind::Other
        );
    }

    #[test]
    fn descriptions_are_sanitized() {
        let error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some("bad\u{1b}[31m thing".to_owned()),
        );
        assert!(!describe(&error).contains('\u{1b}'));
    }
}
