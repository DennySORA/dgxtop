//! `BEGIN IMMEDIATE` transactions with a bounded busy deadline.
//!
//! Each attempt waits at most the connection's `busy_timeout`; attempts are
//! repeated until the partition's total deadline (audit 1000 ms, archive
//! 500 ms) and then reported as busy instead of blocking the worker forever.
//! The deadline uses the real monotonic clock because it bounds real waiting.

use std::time::{Duration, Instant};

use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::errors::{FaultKind, fault_kind};

/// Failure of one step inside the transaction.
#[derive(Debug)]
pub(crate) enum StepError<E> {
    /// Policy decided to stop (conflict, duplicate, ...): rolled back.
    Abort(E),
    Sqlite(rusqlite::Error),
}

impl<E> From<rusqlite::Error> for StepError<E> {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// Outcome of a failed transaction; nothing was committed except where noted.
#[derive(Debug)]
pub(crate) enum TxError<E> {
    Abort(E),
    /// Failed before COMMIT; rolled back.
    Sqlite(rusqlite::Error),
    /// COMMIT itself failed; for I/O errors durability is unknown.
    Commit(rusqlite::Error),
    /// The write lock was not obtained before the deadline.
    BusyTimeout {
        elapsed_ms: u64,
    },
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Runs `work` inside `BEGIN IMMEDIATE … COMMIT`, retrying on busy until
/// `deadline` has elapsed.
pub(crate) fn immediate_transaction<T, E>(
    connection: &mut Connection,
    deadline: Duration,
    mut work: impl FnMut(&Transaction<'_>) -> Result<T, StepError<E>>,
) -> Result<T, TxError<E>> {
    let started = Instant::now();
    loop {
        let retry = match attempt(connection, &mut work) {
            Ok(value) => return Ok(value),
            Err(Attempt::Busy) => started.elapsed() < deadline,
            Err(Attempt::Failed(error)) => return Err(error),
        };
        if !retry {
            return Err(TxError::BusyTimeout {
                elapsed_ms: elapsed_ms(started),
            });
        }
    }
}

enum Attempt<E> {
    Busy,
    Failed(TxError<E>),
}

fn attempt<T, E>(
    connection: &mut Connection,
    work: &mut impl FnMut(&Transaction<'_>) -> Result<T, StepError<E>>,
) -> Result<T, Attempt<E>> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| busy_or(error, TxError::Sqlite))?;
    let value = match work(&transaction) {
        Ok(value) => value,
        Err(StepError::Abort(reason)) => return Err(Attempt::Failed(TxError::Abort(reason))),
        Err(StepError::Sqlite(error)) => return Err(busy_or(error, TxError::Sqlite)),
    };
    // A failed COMMIT drops the transaction, which rolls it back.
    transaction
        .commit()
        .map_err(|error| busy_or(error, TxError::Commit))?;
    Ok(value)
}

fn busy_or<E>(error: rusqlite::Error, wrap: fn(rusqlite::Error) -> TxError<E>) -> Attempt<E> {
    if fault_kind(&error) == FaultKind::Busy {
        Attempt::Busy
    } else {
        Attempt::Failed(wrap(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_database() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "dgxtop-sqlite-tx-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tx.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(x INTEGER);")
            .unwrap();
        (dir, path)
    }

    #[test]
    fn commits_and_aborts() {
        let (dir, path) = temp_database();
        let mut connection = Connection::open(&path).unwrap();
        let inserted: Result<usize, TxError<&str>> =
            immediate_transaction(&mut connection, Duration::from_millis(100), |tx| {
                Ok(tx.execute("INSERT INTO t(x) VALUES (1)", [])?)
            });
        assert_eq!(inserted.unwrap(), 1);
        let aborted: Result<(), TxError<&str>> =
            immediate_transaction(&mut connection, Duration::from_millis(100), |tx| {
                tx.execute("INSERT INTO t(x) VALUES (2)", [])?;
                Err(StepError::Abort("policy"))
            });
        assert!(matches!(aborted, Err(TxError::Abort("policy"))));
        let count: i64 = connection
            .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "the aborted insert was rolled back");
        drop(connection);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn busy_writer_times_out_at_the_deadline() {
        let (dir, path) = temp_database();
        let holder = Connection::open(&path).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let mut connection = Connection::open(&path).unwrap();
        connection.busy_timeout(Duration::from_millis(20)).unwrap();
        let started = Instant::now();
        let result: Result<(), TxError<()>> =
            immediate_transaction(&mut connection, Duration::from_millis(120), |tx| {
                tx.execute("INSERT INTO t(x) VALUES (3)", [])?;
                Ok(())
            });
        assert!(matches!(result, Err(TxError::BusyTimeout { .. })));
        assert!(started.elapsed() >= Duration::from_millis(120));
        assert!(started.elapsed() < Duration::from_secs(5));
        holder.execute_batch("ROLLBACK").unwrap();
        drop(holder);
        drop(connection);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
