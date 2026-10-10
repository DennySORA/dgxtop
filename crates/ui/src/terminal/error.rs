//! Terminal runner errors (infrastructure layer: I/O of the terminal only).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum TuiError {
    #[error("terminal input failed: {0}")]
    Input(#[source] std::io::Error),
    /// The terminal hung up; no further input will arrive.
    #[error("terminal input closed (hang-up)")]
    InputClosed,
    #[error("terminal output failed: {0}")]
    Output(#[source] std::io::Error),
    #[error("terminal setup failed: {0}")]
    Setup(#[source] std::io::Error),
}
