//! Optional SQLite history archive (`history.sqlite3`, WAL,
//! `synchronous=NORMAL`, off by default; ADR16).
//!
//! The archive receives a bounded copy of finished minute buckets, counter
//! bridges, segments and gaps. It is a best-effort durability layer: power
//! loss may drop the most recent transactions, and any failure (low disk,
//! budget, WAL backlog, corruption) is reported as an `InfraError` while the
//! caller keeps running from memory and records an archive gap. When an
//! append would exceed the hard budget, the oldest data is evicted first;
//! age-based retention (default 7 days) removes old buckets and cascades to
//! empty segments.
//!
//! The history view reads the archive back through [`SqliteArchiveReader`]
//! (D-51), a read-only connection of its own, for the time before this run.

mod backfill;
mod limits;
mod queries;
mod reader;
mod rows;
mod sink;

pub use limits::{ArchiveLimits, ArchiveOpenError};
pub use reader::SqliteArchiveReader;
pub use sink::{ArchiveRetentionReport, ArchiveStats, SqliteArchive};
