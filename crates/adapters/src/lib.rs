//! # dgxtop-adapters
//!
//! Read-only telemetry adapters (one per driver interface) and the separate
//! process action adapter. Telemetry adapters implement
//! `dgxtop_core::ports::TelemetryPort`; `process_action` implements
//! `dgxtop_core::ports::ProcessActionPort` and is never reachable from the
//! telemetry path.

pub mod common;
pub mod filesystem;
pub mod hwmon;
pub mod network;
pub mod nvml;
pub mod powercap;
pub mod process_action;
pub mod procfs;
pub mod registry;
pub mod replay;
pub mod thermal;
