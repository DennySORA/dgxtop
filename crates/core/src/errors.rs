//! Shared error vocabulary.
//!
//! Domain errors (validation, policy, state machine) and infrastructure errors
//! (I/O, IPC, SQLite, driver) are distinct types. Both convert into the wire
//! level [`ErrorDetail`] at the boundary, which is what commands, events and
//! views carry. Messages are sanitized; secrets and control characters never
//! leave through an error.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::text::sanitize_message;

/// Every error code allowed by `contracts/INTERFACES.md` and the command-result schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ErrorCode {
    ValidationFailed,
    Conflict,
    Unsupported,
    Unavailable,
    PermissionDenied,
    CapabilityDenied,
    Busy,
    TimedOut,
    StaleView,
    SourceConflict,
    PinnedSourceUnavailable,
    CapacityExceeded,
    PersistFailed,
    RequiresRestart,
    ProcessExited,
    ProcessIdentityChanged,
    NamespaceMismatch,
    ConfirmationExpired,
    ConfirmationUsed,
    UnknownOutcome,
    InternalError,
    QueryTooLarge,
    Cancelled,
}

impl ErrorCode {
    /// Whether a caller may retry the same request unchanged.
    /// `UnknownOutcome` is never retryable for destructive operations.
    pub const fn default_retryable(self) -> bool {
        matches!(
            self,
            Self::Busy | Self::TimedOut | Self::Unavailable | Self::CapacityExceeded
        )
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ValidationFailed => "ValidationFailed",
            Self::Conflict => "Conflict",
            Self::Unsupported => "Unsupported",
            Self::Unavailable => "Unavailable",
            Self::PermissionDenied => "PermissionDenied",
            Self::CapabilityDenied => "CapabilityDenied",
            Self::Busy => "Busy",
            Self::TimedOut => "TimedOut",
            Self::StaleView => "StaleView",
            Self::SourceConflict => "SourceConflict",
            Self::PinnedSourceUnavailable => "PinnedSourceUnavailable",
            Self::CapacityExceeded => "CapacityExceeded",
            Self::PersistFailed => "PersistFailed",
            Self::RequiresRestart => "RequiresRestart",
            Self::ProcessExited => "ProcessExited",
            Self::ProcessIdentityChanged => "ProcessIdentityChanged",
            Self::NamespaceMismatch => "NamespaceMismatch",
            Self::ConfirmationExpired => "ConfirmationExpired",
            Self::ConfirmationUsed => "ConfirmationUsed",
            Self::UnknownOutcome => "UnknownOutcome",
            Self::InternalError => "InternalError",
            Self::QueryTooLarge => "QueryTooLarge",
            Self::Cancelled => "Cancelled",
        }
    }
}

/// `code` field of a command result: `Ok` or an [`ErrorCode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResultCode {
    Ok(OkCode),
    Error(ErrorCode),
}

/// Unit marker serialized as `"Ok"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OkCode {
    Ok,
}

impl ResultCode {
    pub const OK: Self = Self::Ok(OkCode::Ok);

    pub const fn is_ok(self) -> bool {
        matches!(self, Self::Ok(_))
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok(_) => "Ok",
            Self::Error(code) => code.as_str(),
        }
    }
}

impl From<ErrorCode> for ResultCode {
    fn from(code: ErrorCode) -> Self {
        Self::Error(code)
    }
}

/// Traceable error payload shared by results, events and views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[error("{code:?}: {message}")]
pub struct ErrorDetail {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    pub request_id: Option<Uuid>,
    pub affected_ids: Vec<String>,
    /// fsync/rename or audit outcome could not be determined; never auto-retry.
    #[serde(default)]
    pub commit_uncertain: bool,
}

/// Upper bound on `affected_ids` entries kept in one error.
pub const MAX_AFFECTED_IDS: usize = 16;

impl ErrorDetail {
    pub fn new(code: ErrorCode, message: impl AsRef<str>) -> Self {
        Self {
            code,
            message: sanitize_message(message.as_ref()),
            retryable: code.default_retryable(),
            request_id: None,
            affected_ids: Vec::new(),
            commit_uncertain: false,
        }
    }

    pub fn with_request(mut self, request_id: Uuid) -> Self {
        self.request_id = Some(request_id);
        self
    }

    pub fn with_affected(mut self, id: impl AsRef<str>) -> Self {
        if self.affected_ids.len() < MAX_AFFECTED_IDS {
            self.affected_ids.push(sanitize_message(id.as_ref()));
        }
        self
    }

    pub fn uncertain(mut self) -> Self {
        self.commit_uncertain = true;
        self.retryable = false;
        self
    }
}

/// Infrastructure-layer failures: filesystem, IPC, database, driver.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InfraError {
    #[error("io error during {operation}: {message}")]
    Io {
        operation: &'static str,
        message: String,
    },
    #[error("disk full during {operation}")]
    DiskFull { operation: &'static str },
    #[error("ipc protocol error: {0}")]
    Protocol(String),
    #[error("database error: {0}")]
    Database(String),
    #[error("timed out after {elapsed_ms} ms during {operation}")]
    Timeout {
        operation: &'static str,
        elapsed_ms: u64,
    },
    #[error("resource unavailable: {0}")]
    Unavailable(String),
    #[error("commit outcome uncertain during {operation}: {message}")]
    Uncertain {
        operation: &'static str,
        message: String,
    },
}

impl InfraError {
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Io { .. } | Self::DiskFull { .. } | Self::Database(_) => ErrorCode::PersistFailed,
            Self::Protocol(_) | Self::Unavailable(_) => ErrorCode::Unavailable,
            Self::Timeout { .. } => ErrorCode::TimedOut,
            Self::Uncertain { .. } => ErrorCode::PersistFailed,
        }
    }

    /// Builds an I/O error, mapping `ENOSPC` to [`InfraError::DiskFull`].
    pub fn from_io(operation: &'static str, error: &std::io::Error) -> Self {
        if error.raw_os_error() == Some(28) {
            return Self::DiskFull { operation };
        }
        Self::Io {
            operation,
            message: sanitize_message(&error.to_string()),
        }
    }
}

impl From<InfraError> for ErrorDetail {
    fn from(error: InfraError) -> Self {
        let uncertain = matches!(error, InfraError::Uncertain { .. });
        let detail = ErrorDetail::new(error.code(), error.to_string());
        if uncertain {
            detail.uncertain()
        } else {
            detail
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_code_serializes_ok_and_errors() {
        assert_eq!(serde_json::to_string(&ResultCode::OK).unwrap(), "\"Ok\"");
        assert_eq!(
            serde_json::to_string(&ResultCode::from(ErrorCode::StaleView)).unwrap(),
            "\"StaleView\""
        );
        let back: ResultCode = serde_json::from_str("\"QueryTooLarge\"").unwrap();
        assert_eq!(back, ResultCode::Error(ErrorCode::QueryTooLarge));
        let ok: ResultCode = serde_json::from_str("\"Ok\"").unwrap();
        assert!(ok.is_ok());
    }

    #[test]
    fn error_messages_are_sanitized_and_bounded() {
        let detail = ErrorDetail::new(ErrorCode::Unavailable, "bad\u{1b}[31m driver");
        assert_eq!(detail.message, "bad driver");
        let long = ErrorDetail::new(ErrorCode::Unavailable, "x".repeat(5000));
        assert!(long.message.len() <= crate::text::MAX_MESSAGE_BYTES);
        let mut many = ErrorDetail::new(ErrorCode::Conflict, "m");
        for index in 0..40 {
            many = many.with_affected(index.to_string());
        }
        assert_eq!(many.affected_ids.len(), MAX_AFFECTED_IDS);
    }

    #[test]
    fn unknown_outcome_is_not_retryable() {
        assert!(!ErrorCode::UnknownOutcome.default_retryable());
        assert!(ErrorCode::Busy.default_retryable());
        let uncertain: ErrorDetail = InfraError::Uncertain {
            operation: "config.rename",
            message: "dir fsync failed".into(),
        }
        .into();
        assert!(uncertain.commit_uncertain);
        assert!(!uncertain.retryable);
    }

    #[test]
    fn enospc_maps_to_disk_full() {
        let error = std::io::Error::from_raw_os_error(28);
        assert_eq!(
            InfraError::from_io("archive.write", &error),
            InfraError::DiskFull {
                operation: "archive.write"
            }
        );
    }
}
