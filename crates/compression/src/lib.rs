//! # dgxtop-compression
//!
//! Compression of dgxtop (D-79): pure functions and bounded structures that
//! merge samples into buckets of every bar size (1 minute to 24 hours,
//! D-77, D-78), each size fed by a finer one and keeping only its latest
//! buckets, and that lay buckets out as the bars of a chart. No I/O, no
//! clock, no threads.

#![forbid(unsafe_code)]

mod bucket;
mod run;
mod series;

pub use bucket::Bucket;
pub use run::{RunSample, RunSeries};
pub use series::TieredSeries;
