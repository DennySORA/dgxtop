//! Statements and loops more than one partition runs: the pages in use, a
//! PASSIVE checkpoint, cutting a bucket table to its latest rows, bounded
//! deletes repeated in short transactions, and the budget the tiers and
//! the process history share. What to delete and when is the caller's
//! decision; these only execute it.

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

use super::capacity::{CapacityPolicy, PageUsage, passive_checkpoint};
use super::connection::ConnectionProfile;
use super::database::{
    DatabaseSpec, OpenedDatabase, Unusable, VersionGate, open_database, path_problem,
};
use super::schema::SchemaKind;
use super::transaction::{TxError, immediate_transaction};
use crate::fs::FileSystemOps;

const MIB: u64 = 1024 * 1024;
/// The WAL size that triggers a PASSIVE checkpoint in a small database.
const SMALL_WAL_CHECKPOINT_BYTES: u64 = 4 * MIB;

/// db + wal + shm of a small database (the tiers, the process history).
const SMALL_DATABASE_POLICY: CapacityPolicy = CapacityPolicy {
    limit_bytes: 64 * MIB,
    reserve_bytes: 0,
    wal_checkpoint_bytes: SMALL_WAL_CHECKPOINT_BYTES,
    wal_stop_bytes: 16 * MIB,
    min_free_disk_bytes: 64 * MIB,
};

/// Opens a small database: [`SMALL_DATABASE_POLICY`] with the best-effort
/// profile, at an absolute path only.
pub(crate) fn open_small_database(
    path: &Path,
    kind: SchemaKind,
    fs: &dyn FileSystemOps,
    gate: VersionGate,
) -> Result<OpenedDatabase, Unusable> {
    if let Some(problem) = path_problem(path) {
        return Err(Unusable::failed(format!("{}: {problem}", path.display())));
    }
    let spec = DatabaseSpec {
        path,
        kind,
        profile: ConnectionProfile::best_effort(SMALL_WAL_CHECKPOINT_BYTES),
        policy: SMALL_DATABASE_POLICY,
    };
    open_database(&spec, fs, gate)
}

/// Bytes of the pages in use (the free ones are reused before it grows).
pub(crate) fn used_bytes(connection: &Connection) -> rusqlite::Result<u64> {
    let pages = PageUsage::read(connection)?;
    Ok(pages.used_pages().saturating_mul(pages.page_size))
}

/// A PASSIVE checkpoint from the writer: the best-effort profile turns
/// automatic checkpoints off.
pub(crate) fn checkpoint(connection: &Connection) -> rusqlite::Result<()> {
    passive_checkpoint(connection).map(|_progress| ())
}

/// A table of buckets keyed by (owner, `tier`, `start_utc_ns`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct BucketTable {
    pub table: &'static str,
    pub owner_column: &'static str,
}

impl BucketTable {
    /// Deletes all but the latest `keep` buckets of one owner's size (at
    /// least one stays).
    pub(crate) fn keep_latest(
        self,
        connection: &Connection,
        owner: i64,
        tier: i64,
        keep: u64,
    ) -> rusqlite::Result<()> {
        let Self {
            table,
            owner_column,
        } = self;
        let offset = i64::try_from(keep.max(1) - 1).unwrap_or(i64::MAX);
        connection
            .prepare_cached(&format!(
                "DELETE FROM {table} WHERE {owner_column} = ?1 AND tier = ?2 AND \
                 start_utc_ns < (SELECT start_utc_ns FROM {table} \
                 WHERE {owner_column} = ?1 AND tier = ?2 \
                 ORDER BY start_utc_ns DESC LIMIT 1 OFFSET ?3)"
            ))?
            .execute(rusqlite::params![owner, tier, offset])?;
        Ok(())
    }
}

/// What a [`drain`] removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Drained {
    pub removed: u64,
    /// False when the round budget ran out before a short batch.
    pub finished: bool,
}

/// Runs one bounded delete in its own transaction again and again until
/// it removes less than `batch_rows` or `rounds_left` is spent, so no
/// transaction holds the write lock for long.
pub(crate) fn drain(
    connection: &mut Connection,
    deadline: Duration,
    batch_rows: u64,
    rounds_left: &mut u32,
    work: impl Fn(&Connection) -> rusqlite::Result<u64>,
) -> Result<Drained, TxError<()>> {
    let mut removed = 0u64;
    while *rounds_left > 0 {
        *rounds_left -= 1;
        let batch =
            immediate_transaction(connection, deadline, |transaction| Ok(work(transaction)?))?;
        removed = removed.saturating_add(batch);
        if batch < batch_rows {
            return Ok(Drained {
                removed,
                finished: true,
            });
        }
    }
    Ok(Drained {
        removed,
        finished: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buckets() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE b (owner INTEGER, tier INTEGER, start_utc_ns INTEGER, \
                 PRIMARY KEY (owner, tier, start_utc_ns));",
            )
            .unwrap();
        for owner in [1, 2] {
            for start in 0..10 {
                connection
                    .execute(
                        "INSERT INTO b VALUES (?1, 1, ?2)",
                        rusqlite::params![owner, start],
                    )
                    .unwrap();
            }
        }
        connection
    }

    fn starts(connection: &Connection, owner: i64) -> Vec<i64> {
        let mut statement = connection
            .prepare("SELECT start_utc_ns FROM b WHERE owner = ?1 ORDER BY start_utc_ns")
            .unwrap();
        statement
            .query_map([owner], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    const TABLE: BucketTable = BucketTable {
        table: "b",
        owner_column: "owner",
    };

    #[test]
    fn keep_latest_cuts_one_owner_to_its_newest_buckets() {
        let connection = buckets();
        TABLE.keep_latest(&connection, 1, 1, 3).unwrap();
        assert_eq!(starts(&connection, 1), vec![7, 8, 9]);
        assert_eq!(starts(&connection, 2).len(), 10, "other owners stay");
        TABLE.keep_latest(&connection, 1, 1, 0).unwrap();
        assert_eq!(starts(&connection, 1), vec![9], "at least one stays");
        TABLE.keep_latest(&connection, 2, 1, 50).unwrap();
        assert_eq!(starts(&connection, 2).len(), 10, "fewer than kept stay");
    }

    #[test]
    fn drain_repeats_until_a_short_batch_or_the_round_budget() {
        let mut connection = buckets();
        let delete_four = |connection: &Connection| {
            connection
                .execute(
                    "DELETE FROM b WHERE rowid IN (SELECT rowid FROM b LIMIT 4)",
                    [],
                )
                .map(|removed| u64::try_from(removed).unwrap())
        };
        let deadline = Duration::from_millis(100);
        let mut rounds = 2;
        let first = drain(&mut connection, deadline, 4, &mut rounds, delete_four).unwrap();
        assert_eq!(
            first,
            Drained {
                removed: 8,
                finished: false
            }
        );
        assert_eq!(rounds, 0);
        let mut rounds = 10;
        let rest = drain(&mut connection, deadline, 4, &mut rounds, delete_four).unwrap();
        assert_eq!(
            rest,
            Drained {
                removed: 12,
                finished: true
            }
        );
        assert_eq!(rounds, 6, "three full batches, then the empty one");
    }

    #[test]
    fn used_bytes_counts_pages_in_use() {
        let connection = buckets();
        let pages = PageUsage::read(&connection).unwrap();
        assert_eq!(
            used_bytes(&connection).unwrap(),
            pages.used_pages() * pages.page_size
        );
        assert!(used_bytes(&connection).unwrap() > 0);
    }
}
