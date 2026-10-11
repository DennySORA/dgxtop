//! Bounded canonical history (`STORAGE.md` §2–4).
//!
//! Per series (`MetricKey`): raw gauge steps and exact counter intervals kept
//! apart in fixed-width chunked logs (`raw_history_seconds`), minute rollups
//! derived from them once the finalization horizon passes (covered time,
//! time-weighted sum, min/max, sample count, exact `u128` counter totals),
//! counter bridges for intervals crossing a minute boundary, segments cut on
//! source/generation/policy/kind changes, and a merged gap index. Raw history
//! lives in the `raw_history` partition, rollups/bridges/segments/gaps in the
//! `aggregates` partition; when a partition is full the oldest data is
//! evicted (raw first, then rollups are shortened) and the truncation is
//! recorded as an `evicted` gap.

pub(crate) mod chunk;
pub(crate) mod gaps;
mod index;
pub(crate) mod output;
pub(crate) mod pin;
pub(crate) mod records;
pub(crate) mod rollup;
pub(crate) mod segment;
pub(crate) mod series;
pub(crate) mod slots;
pub(crate) mod state;
pub(crate) mod steps;

pub use index::HistoryIndex;
pub(crate) use index::HistoryWriter;
