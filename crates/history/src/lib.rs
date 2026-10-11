//! # dgxtop-history
//!
//! The history of dgxtop (D-79): it takes what the state committed, keeps
//! each series' recent samples and minutes in memory, decides what is
//! saved and for how long, answers the Manager's history reads, and
//! follows GPU process runs (D-76). Compression is the compression
//! component's; the SQLite files are the store's, reached through ports.

#![forbid(unsafe_code)]
