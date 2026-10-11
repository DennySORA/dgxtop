//! # dgxtop-compression
//!
//! Compression of dgxtop (D-79): pure functions and bounded structures that
//! merge samples into buckets of every bar size (1 minute to 24 hours,
//! D-77, D-78), each size fed by a finer one and keeping only its latest
//! buckets, and that lay buckets out as the bars of a chart or cut a
//! window into cells of absolute means (D-97). No I/O, no clock, no
//! threads.

#![forbid(unsafe_code)]

mod bars;
mod bucket;
mod cells;
mod run;
mod series;
mod span;
mod whole;

pub use bars::{Bars, bars_in, bars_of};
pub use bucket::{Bucket, Divisible};
pub use cells::cells_between;
pub use run::{RunSample, RunSeries};
pub use series::TieredSeries;
pub use span::merged_between;
pub use whole::whole_series;
