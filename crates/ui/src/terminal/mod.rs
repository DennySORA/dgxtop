//! Terminal runner: the terminal main-thread loop with injectable input and
//! clock, dirty-only rendering capped by fps, and bounded (1 MiB),
//! frame-atomic output flushed non-blockingly through the injected
//! [`TerminalTransport`](dgxtop_core::ports::TerminalTransport).

mod backend;
mod clock;
mod error;
mod input;
mod output;
mod runner;
mod scheduler;
mod setup;
mod stats;

pub use backend::AnsiBackend;
pub use clock::{LoopClock, SystemClock};
pub use error::TuiError;
pub use input::{CrosstermEvents, GatedInput, InputSource, ReadyEvents};
pub use output::{
    CONTROL_RESERVE, FLUSH_BYTES_PER_ITERATION, FLUSH_TIME_PER_ITERATION, FrameRecord,
    OUTPUT_BUFFER_CAPACITY,
};
pub use runner::{
    LONGEST_IDLE_POLL, MAX_EXIT_WAIT, MAX_IDLE_POLL, MIN_POLL, RESULT_POLL, SLOW_FRAME, TuiExit,
    TuiExitReason, TuiLoop, TuiOptions,
};
pub use scheduler::{AGE_TICK, INPUT_REDRAW_INTERVAL};
pub use setup::{install_panic_hook, restore_terminal, run_tui};
pub use stats::{DurationSamples, FrameStats, FrameStatsSummary, STATS_SAMPLES};
