//! # dgxtop-state
//!
//! The live state of dgxtop (D-79): what the collectors committed, held in
//! memory only and gone at restart, like a small Redis. It serves the
//! Manager's real-time reads (the latest snapshot, collector state for the
//! next delta, candidate evidence, short trends) and hands every committed
//! batch on to the history. No persistence, no history beyond the trends,
//! no I/O.

#![forbid(unsafe_code)]
