//! # dgxtop-ui
//!
//! Viewer and Controller of the dgxtop v2 rewrite (`spec/dgxtop-v2/UI.md`,
//! `ARCHITECTURE.md` §8–§10, `contracts/INTERFACES.md` §6).
//!
//! * **Viewer** ([`viewer`], [`registry`], [`modules`], [`format`],
//!   [`widgets`], [`theme`]): formats one immutable
//!   [`ViewModel`](dgxtop_core::view::ViewModel) into a Ratatui frame plus a
//!   [`HitMap`](viewer::HitMap). It never samples hardware, never queries a
//!   store and never recomputes authoritative values; a missing value is
//!   always rendered as its state text, never as `0` or blank.
//! * **Controller** ([`controller`]): turns key/mouse input into typed
//!   [`CommandBody`](dgxtop_core::commands::CommandBody) intents. Selection is
//!   expressed by `ProcessKey`/entity identity, never by row index, and the
//!   one-time confirmation token lives only in controller memory.
//! * **Terminal runner** ([`terminal`]): the terminal main-thread loop with
//!   injectable input, dirty-only rendering capped by fps, and a bounded
//!   (1 MiB) output buffer flushed non-blockingly through the injected
//!   [`TerminalTransport`](dgxtop_core::ports::TerminalTransport).
//!
//! The crate depends only on `dgxtop-core`, Ratatui, crossterm, thiserror and
//! unicode-width; it never touches libc, drivers or storage.

#![forbid(unsafe_code)]

pub mod controller;
pub mod format;
pub mod modules;
mod num;
pub mod registry;
pub mod terminal;
pub mod theme;
pub mod viewer;
pub mod views;
pub mod widgets;

#[cfg(test)]
mod test_fixtures;

pub use controller::{CommandSink, Controller, ControllerContext, UiState};
pub use registry::{
    DISPLAY_MODULE_API_VERSION, DisplayContext, DisplayModule, DisplayModuleManifest, HitTarget,
    ManifestError, ModuleRegistry, RegistryError, RenderOutput, Slot, Topic,
};
pub use terminal::{
    FrameStats, FrameStatsSummary, InputSource, LoopClock, OUTPUT_BUFFER_CAPACITY, SystemClock,
    TuiError, TuiExit, TuiExitReason, TuiLoop, TuiOptions, install_panic_hook, restore_terminal,
    run_tui,
};
pub use theme::ThemePalette;
pub use viewer::{FrameReport, HitMap, RenderInput, SizeTier, Viewer};
