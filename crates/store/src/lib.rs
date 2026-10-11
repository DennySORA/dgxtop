//! # dgxtop-store
//!
//! Storage mechanics: the single-writer hot store with immutable snapshots
//! (it forwards history to the `HistoryIngest` port, D-79), the
//! versioned atomic configuration repository, the SQLite action audit
//! (synchronous=FULL) and the optional SQLite history archive
//! (synchronous=NORMAL). The store never chooses sensors or runs commands.

pub mod archive;
pub mod audit;
pub mod candidates;
pub mod config;
pub mod fs;
pub mod hot;
pub mod lock;
pub mod paths;
pub mod process_history;
pub mod sqlite;
