//! SQLite policy shared by the audit and the archive partitions
//! (`STORAGE.md` §5–§8, ADR16/ADR17).
//!
//! * Version gate: runtime `sqlite3_libversion_number() >= 3051003` and equal
//!   to the build-time headers ([`ensure_sqlite_version`]).
//! * Connections: WAL, `synchronous` FULL (audit) / NORMAL (archive),
//!   `cache_size` -2048 / -4096 KiB, `busy_timeout` 100 / 50 ms, foreign keys
//!   on, defensive mode, untrusted schema, no URI parsing, no extension
//!   loading, `O_NOFOLLOW` on the database path, and only prepared
//!   statements with bound parameters for data.
//! * Schema: the spec DDL is applied verbatim and idempotently; a newer
//!   `user_version` is a read-only diagnostic; a migration (applying the DDL
//!   to a non-empty file) runs `integrity_check` and a backup first; a corrupt
//!   or unopenable file is kept and reported, never replaced by an empty one.
//! * Capacity: db + `-wal` + `-shm`, `max_page_count` on the main file,
//!   PASSIVE checkpoints only from the writer connection, never FULL/TRUNCATE
//!   checkpoints or VACUUM on the hot path.

mod capacity;
mod connection;
mod database;
mod errors;
mod free_space;
mod schema;
mod time;
mod transaction;
mod version;

pub use capacity::{StorageFootprint, shm_path, wal_path};
pub use connection::{ConnectionProfile, Synchronous};
pub use free_space::{FreeSpaceProbe, StatvfsFreeSpace};
pub use schema::{
    AUDIT_DDL, HISTORY_DDL, HISTORY_TIERS_DDL, PROCESS_HISTORY_DDL, SCHEMA_USER_VERSION, SchemaKind,
};
pub use time::{format_utc_rfc3339, is_rfc3339_utc, parse_utc_rfc3339_ns, utc_cutoff};
pub use version::{
    MIN_SQLITE_VERSION_NUMBER, SqliteGateError, SqliteVersion, check_version_number,
    check_versions, ensure_sqlite_version,
};

pub(crate) use capacity::{
    CapacityPolicy, CapacityView, PageUsage, Refusal, WriteClass, pages_and_backlog,
};
pub(crate) use database::{DatabaseSpec, Unusable, VersionGate, open_database};
pub(crate) use errors::{FaultKind, describe, fault_kind};
pub(crate) use transaction::{StepError, TxError, immediate_transaction};
