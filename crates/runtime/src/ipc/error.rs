//! Typed IPC failures.

use dgxtop_core::errors::InfraError;
use dgxtop_core::text::sanitize_message;
use thiserror::Error;

/// Every way a frame or message can be rejected. Messages are sanitized
/// because they may quote untrusted host output.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IpcError {
    #[error("frame length {declared} exceeds limit {limit}")]
    FrameTooLarge { declared: u64, limit: usize },
    #[error("zero-length frame")]
    EmptyFrame,
    #[error("stream ended inside a frame ({received} of {expected} bytes)")]
    TruncatedFrame { received: usize, expected: usize },
    #[error("frame buffer of {bytes} bytes could not be allocated")]
    AllocationFailed { bytes: usize },
    #[error("payload is not UTF-8 (valid up to byte {valid_up_to})")]
    InvalidUtf8 { valid_up_to: usize },
    #[error("JSON nesting depth exceeds {limit}")]
    TooDeep { limit: usize },
    #[error("JSON element count exceeds {limit}")]
    TooManyValues { limit: usize },
    #[error("malformed message: {message}")]
    Malformed { message: String },
    #[error("{count} records exceed the limit of {limit}")]
    TooManyRecords { count: usize, limit: usize },
    #[error("estimated decoded size {estimated} exceeds budget {limit}")]
    DecodedTooLarge { estimated: u64, limit: usize },
    #[error("hello rejected: {field} does not match")]
    HelloMismatch { field: &'static str },
    #[error("contract violation: {message}")]
    Contract { message: String },
    #[error("frame decoder unusable after an earlier error")]
    Poisoned,
    #[error("frame body decision pending")]
    DecisionPending,
    #[error("invalid limits: {0}")]
    InvalidLimits(String),
    #[error("encoding failed: {message}")]
    Encode { message: String },
    #[error("i/o failure: {message}")]
    Io { message: String },
}

impl IpcError {
    pub(crate) fn malformed(message: &str) -> Self {
        Self::Malformed {
            message: sanitize_message(message),
        }
    }

    pub(crate) fn contract(message: &str) -> Self {
        Self::Contract {
            message: sanitize_message(message),
        }
    }

    pub(crate) fn io(error: &std::io::Error) -> Self {
        Self::Io {
            message: sanitize_message(&error.to_string()),
        }
    }

    /// Stable short code used in diagnostics labels and host stderr lines.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::FrameTooLarge { .. } => "FRAME_TOO_LARGE",
            Self::EmptyFrame => "EMPTY_FRAME",
            Self::TruncatedFrame { .. } => "TRUNCATED_FRAME",
            Self::AllocationFailed { .. } => "ALLOCATION_FAILED",
            Self::InvalidUtf8 { .. } => "INVALID_UTF8",
            Self::TooDeep { .. } => "TOO_DEEP",
            Self::TooManyValues { .. } => "TOO_MANY_VALUES",
            Self::Malformed { .. } => "MALFORMED",
            Self::TooManyRecords { .. } => "TOO_MANY_RECORDS",
            Self::DecodedTooLarge { .. } => "DECODED_TOO_LARGE",
            Self::HelloMismatch { .. } => "HELLO_MISMATCH",
            Self::Contract { .. } => "CONTRACT_VIOLATION",
            Self::Poisoned => "DECODER_POISONED",
            Self::DecisionPending => "DECISION_PENDING",
            Self::InvalidLimits(_) => "INVALID_LIMITS",
            Self::Encode { .. } => "ENCODE_FAILED",
            Self::Io { .. } => "IO_FAILURE",
        }
    }
}

impl From<IpcError> for InfraError {
    fn from(error: IpcError) -> Self {
        InfraError::Protocol(sanitize_message(&error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_sanitized_and_map_to_protocol_errors() {
        let error = IpcError::malformed("bad\u{1b}[2J json");
        assert_eq!(error.to_string(), "malformed message: bad json");
        assert_eq!(error.code(), "MALFORMED");
        let infra: InfraError = error.into();
        assert!(matches!(infra, InfraError::Protocol(_)));
    }
}
