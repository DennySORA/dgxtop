//! # dgxtop-store
//!
//! Storage mechanics, SQLite and files only (D-79): the versioned atomic
//! configuration repository, the SQLite action audit
//! (synchronous=FULL) and the optional SQLite history archive
//! (synchronous=NORMAL), the Overview History tiers database (D-82), the GPU
//! process history database, the instance lock
//! and the private state paths. The store never chooses sensors, runs
//! commands or keeps live or history data in memory: the live state and the
//! history are their own crates.

pub mod archive;
pub mod audit;
pub mod config;
pub mod fs;
pub mod history_tiers;
pub mod lock;
pub mod paths;
pub mod process_history;
pub mod sqlite;
