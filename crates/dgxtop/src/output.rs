//! The one place dgxtop writes lines to its standard output and error.
//!
//! A mode that prints (a report, a warning, a bench's summary) calls these,
//! so no other module holds a print macro. A closed stream ends nothing:
//! `dgxtop --version | head -0` must not panic, and what could not be
//! written has nowhere else to go.

use std::io::Write;

/// Writes `text` and a line break to the standard output.
pub fn line(text: &str) {
    writeln!(std::io::stdout().lock(), "{text}").unwrap_or_default();
}

/// Writes `text` and a line break to the standard error.
pub fn error_line(text: &str) {
    writeln!(std::io::stderr().lock(), "{text}").unwrap_or_default();
}
