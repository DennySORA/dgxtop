//! Protected process-control contracts: signals, action states, capability
//! context, prepared handles, durable audit intents and the messages exchanged
//! with the action worker and the audit worker.
//!
//! The action path never shares types with telemetry: only the Manager's
//! ProcessService holds a `ProcessActionPort`, and only after a durable audit
//! intent may a signal be sent through a held pidfd.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::errors::{ErrorCode, ErrorDetail};
use crate::identity::{BootId, HostId, PidNamespace, ProcessKey, SessionId};

/// Signals the product may send. Nothing else is representable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Signal {
    #[serde(rename = "SIGTERM")]
    Sigterm,
    #[serde(rename = "SIGKILL")]
    Sigkill,
}

impl Signal {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sigterm => "SIGTERM",
            Self::Sigkill => "SIGKILL",
        }
    }

    /// Linux signal number.
    pub const fn number(self) -> i32 {
        match self {
            Self::Sigterm => 15,
            Self::Sigkill => 9,
        }
    }
}

/// Action lifecycle. `SignalSent` is not `ExitObserved`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionState {
    Requested,
    Prepared,
    Confirming,
    IntentPending,
    IntentCommitted,
    Sending,
    SignalSent,
    ExitObserved,
    StillRunning,
    Cancelled,
    Expired,
    Rejected,
    NotSent,
    Unknown,
}

impl ActionState {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Prepared => "prepared",
            Self::Confirming => "confirming",
            Self::IntentPending => "intent_pending",
            Self::IntentCommitted => "intent_committed",
            Self::Sending => "sending",
            Self::SignalSent => "signal_sent",
            Self::ExitObserved => "exit_observed",
            Self::StillRunning => "still_running",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
            Self::Rejected => "rejected",
            Self::NotSent => "not_sent",
            Self::Unknown => "unknown",
        }
    }

    /// States after which cancellation can no longer prevent the signal.
    pub const fn is_past_point_of_no_return(self) -> bool {
        matches!(
            self,
            Self::Sending
                | Self::SignalSent
                | Self::ExitObserved
                | Self::StillRunning
                | Self::Unknown
        )
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::ExitObserved
                | Self::StillRunning
                | Self::Cancelled
                | Self::Expired
                | Self::Rejected
                | Self::NotSent
                | Self::Unknown
        )
    }

    /// Projection onto the command-result `action_state` enum.
    pub const fn result_state(self) -> ActionState {
        match self {
            Self::Requested | Self::Prepared => Self::Prepared,
            Self::Confirming | Self::IntentPending => Self::IntentPending,
            Self::Expired | Self::NotSent => Self::Rejected,
            other => other,
        }
    }
}

/// Opaque one-time confirmation token (random, 64 hex characters).
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ConfirmationToken(String);

impl ConfirmationToken {
    pub fn new(value: impl Into<String>) -> Result<Self, ActionError> {
        let value = value.into();
        let valid = (32..=128).contains(&value.len())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
        if valid {
            Ok(Self(value))
        } else {
            Err(ActionError::InvalidToken)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ConfirmationToken {
    type Error = ActionError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ConfirmationToken> for String {
    fn from(value: ConfirmationToken) -> Self {
        value.0
    }
}

/// Tokens are secrets: never printed or logged.
impl fmt::Debug for ConfirmationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ConfirmationToken(<redacted>)")
    }
}

/// Caller context built by the Manager from the real euid of this process;
/// never deserialized from user-supplied JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityContext {
    pub host_id: HostId,
    pub boot_id: BootId,
    pub pid_namespace: PidNamespace,
    pub session_id: SessionId,
    pub euid: u32,
    pub policy_revision: u32,
    pub actions_enabled: bool,
    pub read_only: bool,
    pub allow_force: bool,
    /// dgxtop itself and every managed adapter host (protected targets).
    pub protected_pids: Vec<i32>,
}

/// Handle of a held pidfd inside the action worker. The OS descriptor itself
/// never leaves the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ActionHandle(pub u64);

/// Identity re-verified against /proc after the pidfd was opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedIdentity {
    pub key: ProcessKey,
    pub uid: u32,
    /// Raw command name as read from `/proc` (confirmation information, not
    /// identity), at most [`crate::text::MAX_BASENAME_BYTES`]. Untrusted:
    /// sanitize before display, escape before storage.
    pub basename: String,
}

/// Result of a successful prepare.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedProcess {
    pub handle: ActionHandle,
    pub verified: VerifiedIdentity,
    pub display_summary: String,
}

/// Prepare/send failures from the action adapter (all fail closed).
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum ActionError {
    #[error("process actions are disabled: {0}")]
    Disabled(String),
    #[error("protected target: {0}")]
    Protected(String),
    #[error("target belongs to another user")]
    UidMismatch,
    #[error("root sessions may not send process signals")]
    RootSession,
    #[error("target is in another pid namespace, host or boot")]
    NamespaceMismatch,
    #[error("process identity changed since selection")]
    IdentityChanged,
    #[error("process already exited")]
    Exited,
    #[error("SIGKILL requires allow_force")]
    ForceNotAllowed,
    #[error("pidfd is not supported by this kernel")]
    PidfdUnsupported,
    #[error("unknown action handle")]
    UnknownHandle,
    #[error("invalid confirmation token format")]
    InvalidToken,
    #[error("action adapter unavailable: {0}")]
    Unavailable(String),
}

impl ActionError {
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Disabled(_)
            | Self::Protected(_)
            | Self::UidMismatch
            | Self::RootSession
            | Self::ForceNotAllowed => ErrorCode::CapabilityDenied,
            Self::NamespaceMismatch => ErrorCode::NamespaceMismatch,
            Self::IdentityChanged => ErrorCode::ProcessIdentityChanged,
            Self::Exited => ErrorCode::ProcessExited,
            Self::PidfdUnsupported => ErrorCode::Unsupported,
            Self::UnknownHandle | Self::InvalidToken => ErrorCode::ValidationFailed,
            Self::Unavailable(_) => ErrorCode::Unavailable,
        }
    }
}

impl From<ActionError> for ErrorDetail {
    fn from(error: ActionError) -> Self {
        ErrorDetail::new(error.code(), error.to_string())
    }
}

/// Classified result of the single `pidfd_send_signal` call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SignalOutcome {
    Sent,
    PermissionDenied,
    Exited,
    Changed,
    Unavailable {
        reason: String,
    },
    /// Deadline or revocation checked by the worker right before sending.
    NotSent {
        reason: String,
    },
    Error {
        errno: i32,
    },
}

/// Non-blocking exit observation through pidfd readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitObservation {
    Running,
    ExitObserved,
    Unknown,
}

/// Durable audit intent; must be committed (FULL) before any signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionIntent {
    pub request_id: Uuid,
    pub intent_id: Uuid,
    pub session_digest: String,
    pub process_key: ProcessKey,
    pub uid: u32,
    /// Raw verified command name; the audit store keeps it escaped.
    pub basename: String,
    pub signal: Signal,
    pub policy_revision: u32,
    pub payload_sha256: String,
    pub created_boot_ns: u64,
    pub created_utc: String,
}

/// Proof that an intent reached durable storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableReceipt {
    pub request_id: Uuid,
    pub intent_id: Uuid,
    pub committed_boot_ns: u64,
}

/// Outcome event appended for an intent (idempotent per `event_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionOutcomeRecord {
    pub event_id: Uuid,
    pub request_id: Uuid,
    pub intent_id: Uuid,
    pub state: ActionState,
    pub result_code: String,
    pub detail: String,
    pub occurred_boot_ns: u64,
    pub occurred_utc: String,
}

/// Record for prepare/reject/cancel events that never produced an intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionAttempt {
    pub request_id: Uuid,
    pub payload_sha256: String,
    pub session_digest: String,
    pub result_code: String,
    pub detail: String,
    pub occurred_utc: String,
}

/// Intent found without a provable outcome at startup; marked unknown, never resent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredIntent {
    pub request_id: Uuid,
    pub intent_id: Uuid,
    pub process_key: ProcessKey,
    pub signal: Signal,
    pub previous_status: String,
}

/// Summary of a stored action for idempotent lookups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRecordSummary {
    pub request_id: Uuid,
    pub intent_id: Option<Uuid>,
    pub payload_sha256: String,
    pub state: ActionState,
    pub result_code: String,
}

/// Which persistent partition a status describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistenceKind {
    Config,
    Audit,
    Archive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistenceState {
    Disabled,
    Healthy,
    /// Working but near a cap (WAL/disk/quota) or recovering.
    Degraded,
    /// Writes refused (disk full, corrupt, version gate, locked).
    Failed,
    /// Unknown future schema: diagnostics only, never rewritten.
    ReadOnlyDiagnostic,
}

/// Independent status of one persistent partition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistenceStatus {
    pub kind: PersistenceKind,
    pub state: PersistenceState,
    pub detail: Option<String>,
    pub bytes_used: u64,
    pub bytes_limit: u64,
    pub wal_bytes: u64,
}

impl PersistenceStatus {
    pub fn healthy(kind: PersistenceKind) -> Self {
        Self {
            kind,
            state: PersistenceState::Healthy,
            detail: None,
            bytes_used: 0,
            bytes_limit: 0,
            wal_bytes: 0,
        }
    }

    pub fn disabled(kind: PersistenceKind) -> Self {
        Self {
            state: PersistenceState::Disabled,
            ..Self::healthy(kind)
        }
    }

    /// Whether new destructive actions may rely on this partition.
    pub const fn accepts_writes(&self) -> bool {
        matches!(
            self.state,
            PersistenceState::Healthy | PersistenceState::Degraded
        )
    }
}

/// Audit failure classification returned to the Manager.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[error("{code:?}: {message}")]
pub struct AuditFailure {
    pub code: ErrorCode,
    pub message: String,
    pub commit_uncertain: bool,
}

impl From<AuditFailure> for ErrorDetail {
    fn from(failure: AuditFailure) -> Self {
        let detail = ErrorDetail::new(failure.code, &failure.message);
        if failure.commit_uncertain {
            detail.uncertain()
        } else {
            detail
        }
    }
}

/// Manager -> action worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOp {
    Prepare {
        request_id: Uuid,
        key: ProcessKey,
        signal: Signal,
        context: CapabilityContext,
    },
    /// Executes one approved intent; the worker re-checks the deadline first.
    Send {
        request_id: Uuid,
        intent_id: Uuid,
        handle: ActionHandle,
        signal: Signal,
        receipt: DurableReceipt,
        send_deadline_boot_ns: u64,
    },
    ObserveExit {
        request_id: Uuid,
        handle: ActionHandle,
    },
    Release {
        handle: ActionHandle,
    },
}

/// Action worker -> Manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionCompletion {
    Prepared {
        request_id: Uuid,
        result: Result<PreparedProcess, ActionError>,
    },
    Sent {
        request_id: Uuid,
        intent_id: Uuid,
        outcome: SignalOutcome,
        sent_boot_ns: u64,
    },
    ExitObserved {
        request_id: Uuid,
        observation: ExitObservation,
    },
    Released {
        handle: ActionHandle,
    },
}

/// Manager -> audit worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditOp {
    AppendIntent(ActionIntent),
    AppendOutcome(ActionOutcomeRecord),
    AppendAttempt(ActionAttempt),
    RecoverUnknown {
        now_boot_ns: u64,
        now_utc: String,
    },
    /// Delete the rows older than `before_utc` (RFC 3339 UTC); the owner of
    /// the audit decides the age and when (D-96).
    Expire {
        before_utc: String,
    },
    Status,
}

/// Rows one audit expiry deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuditExpiryReport {
    pub deleted_events: u64,
    pub deleted_attempts: u64,
    pub deleted_intents: u64,
    /// False when the per-request budget stopped before every expired row
    /// was deleted (the next request continues).
    pub complete: bool,
}

/// Audit worker -> Manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditCompletion {
    IntentDurable {
        request_id: Uuid,
        intent_id: Uuid,
        result: Result<DurableReceipt, AuditFailure>,
    },
    OutcomeRecorded {
        request_id: Uuid,
        result: Result<(), AuditFailure>,
    },
    AttemptRecorded {
        request_id: Uuid,
        result: Result<(), AuditFailure>,
    },
    Recovered {
        result: Result<Vec<RecoveredIntent>, AuditFailure>,
    },
    Expired {
        result: Result<AuditExpiryReport, AuditFailure>,
    },
    Status(PersistenceStatus),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_validated_and_redacted() {
        assert!(ConfirmationToken::new("short").is_err());
        assert!(ConfirmationToken::new("a".repeat(129)).is_err());
        assert!(ConfirmationToken::new(format!("{}!", "a".repeat(40))).is_err());
        let token = ConfirmationToken::new("ab".repeat(32)).unwrap();
        assert_eq!(format!("{token:?}"), "ConfirmationToken(<redacted>)");
    }

    #[test]
    fn sent_is_not_exit() {
        assert!(ActionState::SignalSent.is_past_point_of_no_return());
        assert!(!ActionState::SignalSent.is_terminal());
        assert!(ActionState::ExitObserved.is_terminal());
        assert!(!ActionState::IntentCommitted.is_past_point_of_no_return());
        assert_eq!(ActionState::NotSent.result_state(), ActionState::Rejected);
    }

    #[test]
    fn action_errors_map_to_spec_codes() {
        assert_eq!(
            ActionError::NamespaceMismatch.code(),
            ErrorCode::NamespaceMismatch
        );
        assert_eq!(
            ActionError::IdentityChanged.code(),
            ErrorCode::ProcessIdentityChanged
        );
        assert_eq!(ActionError::Exited.code(), ErrorCode::ProcessExited);
        assert_eq!(ActionError::RootSession.code(), ErrorCode::CapabilityDenied);
    }
}
