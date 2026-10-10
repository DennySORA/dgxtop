//! Manager construction and lifecycle errors (application layer).
//!
//! Command-level failures never use this type: they travel as
//! [`dgxtop_core::errors::ErrorDetail`] / [`dgxtop_core::commands::CommandResult`]
//! so that every accepted command still receives exactly one typed result.

use dgxtop_core::errors::ErrorCode;
use thiserror::Error;

/// Failures of [`crate::Manager::new`] and [`crate::Manager::open_session`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ManagerError {
    /// The composition root handed over an inconsistent setup or port set.
    #[error("invalid manager setup: {0}")]
    InvalidSetup(String),
    /// `viewer.max_sessions` sessions are already open.
    #[error("session limit reached: at most {max} sessions")]
    SessionLimit { max: u32 },
    /// Shutdown was requested; no new sessions are accepted.
    #[error("manager is shutting down")]
    ShuttingDown,
}

impl ManagerError {
    /// Wire error code used when the failure is surfaced to a frontend.
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidSetup(_) => ErrorCode::ValidationFailed,
            Self::SessionLimit { .. } => ErrorCode::CapacityExceeded,
            Self::ShuttingDown => ErrorCode::Unavailable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map_to_wire_codes() {
        assert_eq!(
            ManagerError::InvalidSetup("x".into()).code(),
            ErrorCode::ValidationFailed
        );
        assert_eq!(
            ManagerError::SessionLimit { max: 2 }.code(),
            ErrorCode::CapacityExceeded
        );
        assert_eq!(ManagerError::ShuttingDown.code(), ErrorCode::Unavailable);
        assert!(
            ManagerError::SessionLimit { max: 2 }
                .to_string()
                .contains("at most 2")
        );
    }
}
