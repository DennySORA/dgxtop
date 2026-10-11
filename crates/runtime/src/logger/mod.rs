//! Bounded, rate-limited logging (`RUNTIME.md` §5 log row, `OPERATIONS.md` §3).
//!
//! * [`FileLogger`]: a dedicated writer thread fed through a byte-accounted
//!   queue (≤ 1 MiB). Callers never block: when the queue is under pressure
//!   debug/info records are dropped first and counted; errors and warnings
//!   keep the last quarter of the queue and are counted separately when even
//!   that is full. Identical `(target, message)` pairs within 30 s collapse
//!   into one line plus a summary. Files are `0600` inside a `0700`
//!   directory and rotate by size.
//!
//! Adapter hosts do not log through this module: they write bounded protocol
//! diagnostics to their stderr, which the supervisor drains.

mod coalesce;
mod file;
mod format;

pub use file::{FileLogger, LoggerStats};
pub use format::{format_utc, level_name};

/// Window inside which identical records are coalesced.
pub const COALESCE_WINDOW_NS: u64 = 30 * dgxtop_core::clock::NANOS_PER_SECOND;
/// Hard cap on the logger queue.
pub const MAX_QUEUE_BYTES: usize = 1 << 20;
