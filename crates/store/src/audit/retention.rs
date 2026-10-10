//! Age-based audit retention.
//!
//! Only rows older than the cutoff are deleted, in small bounded
//! transactions: events by their own `occurred_utc`, attempts by theirs, and
//! intents by `created_utc` once no event references them (an old intent
//! with a recent recovery event lives until that event expires). Retention
//! never deletes unexpired rows to make room; a full audit refuses new
//! actions instead.

use std::time::Duration;

use rusqlite::{Connection, params};

use crate::sqlite::{TxError, immediate_transaction};

/// Rows deleted per transaction.
pub(crate) const RETENTION_BATCH_ROWS: i64 = 512;
/// Transactions per retention run; the next run continues where this one
/// stopped.
pub(crate) const RETENTION_MAX_ROUNDS: u32 = 64;

const DELETE_EXPIRED_EVENTS: &str = "DELETE FROM action_events WHERE rowid IN (
    SELECT rowid FROM action_events WHERE occurred_utc < ?1 LIMIT ?2)";
const DELETE_EXPIRED_ATTEMPTS: &str = "DELETE FROM action_attempts WHERE rowid IN (
    SELECT rowid FROM action_attempts WHERE occurred_utc < ?1 LIMIT ?2)";
const DELETE_EXPIRED_INTENTS: &str = "DELETE FROM action_intents WHERE rowid IN (
    SELECT rowid FROM action_intents
    WHERE created_utc < ?1
      AND request_id NOT IN (SELECT request_id FROM action_events)
    LIMIT ?2)";

/// Rows removed by one retention run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RetentionCounts {
    pub events: u64,
    pub attempts: u64,
    pub intents: u64,
    /// False when the round budget stopped the run before every expired row
    /// was removed.
    pub complete: bool,
}

struct Drained {
    deleted: u64,
    finished: bool,
}

/// Repeats one bounded DELETE until it removes less than a full batch or
/// the shared round budget is spent.
fn drain(
    connection: &mut Connection,
    statement: &str,
    cutoff_utc: &str,
    deadline: Duration,
    rounds_left: &mut u32,
) -> Result<Drained, TxError<()>> {
    let mut deleted_total = 0u64;
    while *rounds_left > 0 {
        *rounds_left -= 1;
        let deleted = immediate_transaction(connection, deadline, |transaction| {
            Ok(transaction.execute(statement, params![cutoff_utc, RETENTION_BATCH_ROWS])?)
        })?;
        deleted_total = deleted_total.saturating_add(u64::try_from(deleted).unwrap_or(u64::MAX));
        if i64::try_from(deleted).unwrap_or(i64::MAX) < RETENTION_BATCH_ROWS {
            return Ok(Drained {
                deleted: deleted_total,
                finished: true,
            });
        }
    }
    Ok(Drained {
        deleted: deleted_total,
        finished: false,
    })
}

/// Deletes expired rows (`*_utc < cutoff_utc`).
pub(crate) fn delete_expired(
    connection: &mut Connection,
    cutoff_utc: &str,
    deadline: Duration,
) -> Result<RetentionCounts, TxError<()>> {
    let mut rounds_left = RETENTION_MAX_ROUNDS;
    let mut counts = RetentionCounts::default();
    // Events first: intents are only deleted once nothing references them.
    let events = drain(
        connection,
        DELETE_EXPIRED_EVENTS,
        cutoff_utc,
        deadline,
        &mut rounds_left,
    )?;
    counts.events = events.deleted;
    if !events.finished {
        return Ok(counts);
    }
    let attempts = drain(
        connection,
        DELETE_EXPIRED_ATTEMPTS,
        cutoff_utc,
        deadline,
        &mut rounds_left,
    )?;
    counts.attempts = attempts.deleted;
    if !attempts.finished {
        return Ok(counts);
    }
    let intents = drain(
        connection,
        DELETE_EXPIRED_INTENTS,
        cutoff_utc,
        deadline,
        &mut rounds_left,
    )?;
    counts.intents = intents.deleted;
    counts.complete = intents.finished;
    Ok(counts)
}
