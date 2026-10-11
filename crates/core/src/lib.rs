//! # dgxtop-core
//!
//! Cross-layer contracts shared by the six components (Adapter, Collector,
//! Manager, Store, Viewer, Controller) of the dgxtop v2 rewrite
//! (`spec/dgxtop-v2/`). This crate holds typed identities, units, quality
//! axes, commands/results, events, configuration, byte budgets and the ports
//! between components. It performs no I/O and depends only on std plus small
//! DTO/serialization crates.

#![forbid(unsafe_code)]

pub mod action;
pub mod budget;
pub mod canonical;
pub mod clock;
pub mod commands;
pub mod config;
pub mod diagnostics;
pub mod digest;
pub mod errors;
pub mod event;
pub mod gpu_bandwidth;
pub mod gpu_state;
pub mod history;
pub mod identity;
pub mod metric;
pub mod naming;
pub mod observation;
pub mod ports;
pub mod process_history;
pub mod process_probe;
pub mod quality;
pub mod sampling;
pub mod shared_text;
pub mod text;
pub mod view;
pub mod wire;

#[cfg(any(test, feature = "test-support"))]
pub mod testing;

/// Package version shared by every crate of the workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
