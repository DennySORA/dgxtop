//! SQLite policy shared by every partition: the audit, the history
//! archive, the process history and the history tiers (D-85).
//!
//! * Version gate: runtime `sqlite3_libversion_number() >= 3051003` and equal
//!   to the build-time headers ([`ensure_sqlite_version`]).
//! * Connections: WAL, `synchronous` FULL (audit) / NORMAL (the archive,
//!   the tiers and the process history), `cache_size` -2048 / -4096 KiB,
//!   `busy_timeout` 100 / 50 ms, foreign keys
//!   on, defensive mode, untrusted schema, no URI parsing, no extension
//!   loading, `O_NOFOLLOW` on the database path, and only prepared
//!   statements with bound parameters for data.
//! * Schema: the DDL is applied verbatim and idempotently and the file is
//!   stamped with its partition's format version
//!   ([`SchemaKind::format_version`], in `PRAGMA application_id`). A file of
//!   another or no format version, another `user_version` or other
//!   objects, or one that is unreadable, corrupt or unsafe, is deleted with
//!   its `-wal`/`-shm` and created afresh (D-85): no migration, no backup,
//!   no read-only diagnostic.
//! * Capacity: db + `-wal` + `-shm`, `max_page_count` on the main file,
//!   PASSIVE checkpoints only from the writer connection, never FULL/TRUNCATE
//!   checkpoints or VACUUM on the hot path.
//! * Failures: one classification (`FaultKind`) and one mapping to
//!   `InfraError` for every partition (`Writer`); corruption stops the
//!   partition until the next start recreates the file. The audit maps the
//!   same classes to its own `AuditFailure` codes.
//!
//! The shared statements (`drain`, `BucketTable::keep_latest`) only
//! execute what their callers decide to keep or delete.

mod capacity;
mod connection;
mod database;
mod errors;
mod free_space;
mod schema;
mod statements;
mod time;
mod transaction;
mod version;
mod writer;

pub use capacity::{StorageFootprint, shm_path, wal_path};
pub use free_space::{FreeSpaceProbe, StatvfsFreeSpace};
pub use schema::{AUDIT_DDL, HISTORY_DDL, SchemaKind, format_stamp};
pub use time::{format_utc_rfc3339, is_rfc3339_utc, parse_utc_rfc3339_ns};
pub use version::{
    MIN_SQLITE_VERSION_NUMBER, SqliteGateError, SqliteVersion, check_version_number,
    check_versions, ensure_sqlite_version,
};

pub(crate) use capacity::{
    CapacityPolicy, CapacityView, PageUsage, Refusal, WriteClass, pages_and_backlog,
};
pub(crate) use connection::ConnectionProfile;
pub(crate) use database::{DatabaseSpec, Unusable, VersionGate, open_database, path_problem};
pub(crate) use errors::{FaultKind, describe, fault_kind, fault_message, tx_sqlite_error};
#[cfg(test)]
pub(crate) use schema::PROCESS_HISTORY_DDL;
pub(crate) use schema::{SchemaPlan, plan};
pub(crate) use statements::{
    BucketTable, Drained, checkpoint, drain, open_small_database, used_bytes,
};
pub(crate) use transaction::{StepError, TxError, immediate_transaction};
pub(crate) use writer::{DatabaseFailure, Writer};
