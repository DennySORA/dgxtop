//! A partition's writer connection, or why it has none, and the one rule
//! every partition applies to a failed call: corruption stops the
//! partition for this run, without checkpointing possibly damaged pages
//! into the file, and the open sequence recreates the file at the next
//! start (D-85).

use dgxtop_core::errors::InfraError;
use rusqlite::Connection;
use rusqlite::config::DbConfig;

use super::database::{OpenedDatabase, Unusable};
use super::errors::{FaultKind, fault_kind, infra_error, tx_infra_error, tx_sqlite_error};
use super::transaction::TxError;

/// A failed call of a partition's rows, mapped by [`Writer::fail`].
#[derive(Debug)]
pub(crate) enum DatabaseFailure {
    Statement {
        operation: &'static str,
        error: rusqlite::Error,
    },
    Transaction {
        operation: &'static str,
        error: TxError<()>,
    },
}

impl DatabaseFailure {
    pub(crate) fn statement(operation: &'static str) -> impl Fn(rusqlite::Error) -> Self {
        move |error| Self::Statement { operation, error }
    }

    pub(crate) fn transaction(operation: &'static str) -> impl Fn(TxError<()>) -> Self {
        move |error| Self::Transaction { operation, error }
    }
}

/// The writer connection of one partition.
pub(crate) struct Writer {
    /// "archive", "audit", ...: names the partition in what it reports.
    label: &'static str,
    state: Result<Connection, Unusable>,
}

impl std::fmt::Debug for Writer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Writer")
            .field("label", &self.label)
            .field("state", &self.state.as_ref().map(|_| "open"))
            .finish()
    }
}

impl Writer {
    /// The writer of what the open sequence returned, and why the file it
    /// found was recreated, if it was.
    pub(crate) fn new(
        label: &'static str,
        opened: Result<OpenedDatabase, Unusable>,
    ) -> (Self, Option<String>) {
        let (state, recreated) = match opened {
            Ok(opened) => (Ok(opened.connection), opened.recreated),
            Err(unusable) => (Err(unusable), None),
        };
        (Self { label, state }, recreated)
    }

    pub(crate) fn connection(&mut self) -> Result<&mut Connection, &Unusable> {
        self.state.as_mut().map_err(|unusable| &*unusable)
    }

    /// Why the partition must not be written, if it must not.
    pub(crate) fn unusable(&self) -> Option<&Unusable> {
        self.state.as_ref().err()
    }

    /// Stops the partition when `error` is corruption; returns whether it
    /// did. `message` says what failed.
    pub(crate) fn stop_if_corrupt(&mut self, error: &rusqlite::Error, message: &str) -> bool {
        if fault_kind(error) != FaultKind::Corrupt {
            return false;
        }
        if self.state.is_err() {
            // Already stopped: the first reason stays.
            return true;
        }
        let reason = Unusable::failed(format!(
            "{message} ({} stopped; the file is recreated at the next start, D-85)",
            self.label
        ));
        if let Ok(connection) = std::mem::replace(&mut self.state, Err(reason)) {
            // Closing must not checkpoint possibly damaged pages into the file.
            let _ignored =
                connection.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true);
        }
        true
    }

    /// The `InfraError` of a failed statement, stopping the partition on
    /// corruption.
    pub(crate) fn statement_failed(
        &mut self,
        operation: &'static str,
        error: &rusqlite::Error,
    ) -> InfraError {
        let mapped = infra_error(operation, error);
        self.stop_if_corrupt(error, &mapped.to_string());
        mapped
    }

    /// The `InfraError` of a failed transaction, stopping the partition on
    /// corruption.
    pub(crate) fn transaction_failed<E>(
        &mut self,
        operation: &'static str,
        error: &TxError<E>,
    ) -> InfraError {
        let mapped = tx_infra_error(operation, error);
        if let Some(sqlite) = tx_sqlite_error(error) {
            self.stop_if_corrupt(sqlite, &mapped.to_string());
        }
        mapped
    }

    pub(crate) fn fail(&mut self, failure: &DatabaseFailure) -> InfraError {
        match failure {
            DatabaseFailure::Statement { operation, error } => {
                self.statement_failed(operation, error)
            }
            DatabaseFailure::Transaction { operation, error } => {
                self.transaction_failed(operation, error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(code: i32) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), Some("detail".to_owned()))
    }

    fn open_writer() -> Writer {
        let opened = OpenedDatabase {
            connection: Connection::open_in_memory().unwrap(),
            recreated: None,
        };
        Writer::new("acme", Ok(opened)).0
    }

    #[test]
    fn every_class_maps_to_one_infra_error() {
        let mut writer = open_writer();
        let operation = "acme.write";
        let cases = [
            (rusqlite::ffi::SQLITE_FULL, "DiskFull"),
            (rusqlite::ffi::SQLITE_BUSY, "Unavailable"),
            (rusqlite::ffi::SQLITE_IOERR_WRITE, "Io"),
            (rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE, "Database"),
            (rusqlite::ffi::SQLITE_READONLY, "Database"),
            (rusqlite::ffi::SQLITE_ERROR, "Database"),
        ];
        for (code, expected) in cases {
            let mapped = writer.statement_failed(operation, &failure(code));
            assert!(format!("{mapped:?}").starts_with(expected), "{mapped:?}");
            assert!(writer.unusable().is_none(), "{mapped:?} keeps the writer");
        }
    }

    #[test]
    fn a_failed_commit_of_unknown_outcome_is_uncertain() {
        let mut writer = open_writer();
        let io: TxError<()> = TxError::Commit(failure(rusqlite::ffi::SQLITE_IOERR_FSYNC));
        assert!(matches!(
            writer.transaction_failed("acme.write", &io),
            InfraError::Uncertain {
                operation: "acme.write",
                ..
            }
        ));
        let before_commit: TxError<()> = TxError::Sqlite(failure(rusqlite::ffi::SQLITE_IOERR));
        assert!(matches!(
            writer.transaction_failed("acme.write", &before_commit),
            InfraError::Io { .. }
        ));
        let full: TxError<()> = TxError::Commit(failure(rusqlite::ffi::SQLITE_FULL));
        assert!(matches!(
            writer.transaction_failed("acme.write", &full),
            InfraError::DiskFull { .. }
        ));
        let busy: TxError<()> = TxError::BusyTimeout { elapsed_ms: 7 };
        assert!(matches!(
            writer.transaction_failed("acme.write", &busy),
            InfraError::Timeout { elapsed_ms: 7, .. }
        ));
        assert!(writer.unusable().is_none());
    }

    #[test]
    fn corruption_stops_the_partition_with_its_reason() {
        let mut writer = open_writer();
        let corrupt: TxError<()> = TxError::Sqlite(failure(rusqlite::ffi::SQLITE_CORRUPT));
        let mapped = writer.transaction_failed("acme.write", &corrupt);
        assert!(matches!(mapped, InfraError::Database(_)), "{mapped:?}");
        let reason = &writer.unusable().unwrap().reason;
        assert!(reason.contains("acme.write"), "{reason}");
        assert!(reason.contains("acme stopped"), "{reason}");
        assert!(writer.connection().is_err());
        // A second failure keeps the first reason.
        writer.statement_failed("acme.read", &failure(rusqlite::ffi::SQLITE_NOTADB));
        assert!(writer.unusable().unwrap().reason.contains("acme.write"));
    }
}
