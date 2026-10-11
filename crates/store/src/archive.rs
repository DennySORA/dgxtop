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
//! empty segments. The history's compression also asks, through the
//! journals, to delete the Overview series' minutes its coarser sizes hold
//! (D-82), so for those the retention is only an upper bound.
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
pub use sink::{ArchiveRetentionReport, ArchiveStats, SqliteArchive};
