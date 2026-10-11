//! Classification of SQLite failures into the few classes persistence
//! policy distinguishes (busy, full, corrupt, I/O, constraint, ...), and
//! the one mapping of a failed statement or transaction to an
//! [`InfraError`] that every partition reporting `InfraError` uses (the
//! audit maps the same classes to its own `AuditFailure` codes).

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
    /// `SQLITE_CORRUPT`/`SQLITE_NOTADB`: the partition stops writing and
    /// the file is recreated at the next start (D-85).
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

/// `operation: <sanitized SQLite message>`, for errors and statuses.
pub(crate) fn fault_message(operation: &str, error: &rusqlite::Error) -> String {
    format!("{operation}: {}", describe(error))
}

/// A failed statement of `operation`: a full database or disk is
/// `DiskFull`, a held lock `Unavailable`, an I/O error `Io`, anything else
/// (corruption included) `Database`.
pub(crate) fn infra_error(operation: &'static str, error: &rusqlite::Error) -> InfraError {
    let message = fault_message(operation, error);
    match fault_kind(error) {
        FaultKind::Full => InfraError::DiskFull { operation },
        FaultKind::Busy => InfraError::Unavailable(message),
        FaultKind::Io => InfraError::Io { operation, message },
        FaultKind::Corrupt
        | FaultKind::Constraint
        | FaultKind::ReadOnly
        | FaultKind::CannotOpen
        | FaultKind::Other => InfraError::Database(message),
    }
}

/// A failed transaction of `operation`: like [`infra_error`], except that
/// an I/O error of COMMIT itself is `Uncertain` (it may have reached the
/// disk) and the lock deadline is a `Timeout`.
pub(crate) fn tx_infra_error<E>(operation: &'static str, error: &TxError<E>) -> InfraError {
    match error {
        TxError::Sqlite(error) => infra_error(operation, error),
        TxError::Commit(error) if fault_kind(error) == FaultKind::Io => InfraError::Uncertain {
            operation,
            message: fault_message(operation, error),
        },
        TxError::Commit(error) => infra_error(operation, error),
        TxError::BusyTimeout { elapsed_ms } => InfraError::Timeout {
            operation,
            elapsed_ms: *elapsed_ms,
        },
        TxError::Abort(_) => InfraError::Database(format!("{operation}: aborted")),
    }
}

/// The SQLite error inside a failed transaction, if there is one.
pub(crate) fn tx_sqlite_error<E>(error: &TxError<E>) -> Option<&rusqlite::Error> {
    match error {
        TxError::Sqlite(error) | TxError::Commit(error) => Some(error),
        TxError::Abort(_) | TxError::BusyTimeout { .. } => None,
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
