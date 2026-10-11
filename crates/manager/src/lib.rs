//! # dgxtop-manager
//!
//! Application layer: sampling scheduler, device catalog, quality policy,
//! ingestion (single logical writer), history, configuration, protected
//! process actions, presentation pacing and view sessions — all driven by
//! one control thread that never blocks on I/O. Depends only on
//! `dgxtop-core` ports; the composition root supplies the implementations.
//!
//! Entry points: [`Manager::new`] (with [`ManagerSetup`] and
//! [`ManagerPorts`]), [`Manager::open_session`] (a [`SessionHandle`]
//! implementing `dgxtop_core::ports::SessionPort`),
//! [`Manager::shutdown_trigger`] and [`Manager::run`].

mod archive;
mod catalog;
mod config;
mod control;
mod effects;
mod error;
mod events;
mod history;
mod hosts;
mod ingestion;
mod manager;
mod overload;
mod pacing;
mod policy;
pub mod presentation;
mod process;
pub mod process_history;
mod process_memory;
mod projection;
mod results;
mod scheduler;
mod session;
mod setup;
mod shutdown;
#[cfg(test)]
mod test_support;
mod time_format;
mod views;

pub use error::ManagerError;
pub use events::EventFeed;
pub use manager::{Manager, ManagerChannels};
pub use session::SessionHandle;
pub use setup::{
    CONTROL_INBOX_CAPACITY, INGESTION_INBOX_CAPACITY, ManagerInboxes, ManagerPorts, ManagerSetup,
};
pub use shutdown::{ShutdownSummary, ShutdownTrigger};
