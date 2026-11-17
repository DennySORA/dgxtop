//! Optional SQLite history archive (`history.sqlite3`, WAL,
//! `synchronous=NORMAL`, on by default since D-51).
//!
//! The archive receives a bounded copy of finished minute buckets, counter
//! bridges, segments and gaps. It is a best-effort durability layer: power
//! loss may drop the most recent transactions, and any failure (low disk,
//! budget, WAL backlog, corruption) is reported as an `InfraError` while the
//! caller keeps running from memory and records an archive gap. The store
//! deletes only what a journal asks for (D-96): the Overview series'
//! minutes the compression's coarser sizes hold (D-82), the rows past the
//! age the history keeps (`store.archive_retention_days`, with the rows
//! that only existed for them), and, when the journal says so, the oldest
//! minutes an append over the hard budget needs room from.
//!
//! The history reads the archive back through [`SqliteArchiveReader`]
//! (D-51, D-82), a read-only connection of its own, to compress the minutes
//! earlier runs left into the Overview's sizes.

mod backfill;
mod limits;
mod queries;
mod reader;
mod rows;
mod sink;

pub use limits::{ArchiveLimits, ArchiveOpenError};
pub use reader::SqliteArchiveReader;
pub use sink::{ArchiveExpiryReport, ArchiveStats, SqliteArchive};
