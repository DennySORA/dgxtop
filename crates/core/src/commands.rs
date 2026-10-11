//! Typed commands and results (`command.schema.json`, `command-result.schema.json`).
//!
//! The Controller only produces these intents; the Manager re-validates every
//! semantic rule. There is no arbitrary execute/SQL command.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::action::{ActionState, ConfirmationToken, Signal};
use crate::config::Config;
use crate::errors::{ErrorCode, ErrorDetail, ResultCode};
use crate::identity::{EntityId, ModuleId, ProcessKey, SessionId};

/// Command/result schema version.
pub const COMMAND_SCHEMA_VERSION: u32 = 2;
/// Maximum filter text in bytes.
pub const MAX_FILTER_BYTES: usize = 256;

/// Logical views selectable through a view session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewKind {
    Overview,
    GpuDetail,
    Processes,
    History,
    Sources,
    Settings,
    Actions,
}

impl ViewKind {
    pub const ALL: [ViewKind; 7] = [
        ViewKind::Overview,
        ViewKind::GpuDetail,
        ViewKind::Processes,
        ViewKind::History,
        ViewKind::Sources,
        ViewKind::Settings,
        ViewKind::Actions,
    ];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::GpuDetail => "GPU",
            Self::Processes => "Processes",
            Self::History => "History",
            Self::Sources => "Sources",
            Self::Settings => "Settings",
            Self::Actions => "Actions",
        }
    }

    /// Next view in Tab order (wraps).
    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|view| *view == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// Previous view in Shift-Tab order (wraps).
    pub fn previous(self) -> Self {
        let index = Self::ALL.iter().position(|view| *view == self).unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// Process table sort column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    GpuMemory,
    GpuUtil,
    CpuPercent,
    HostMemory,
    Pid,
}

impl SortKey {
    pub const fn next(self) -> Self {
        match self {
            Self::GpuMemory => Self::GpuUtil,
            Self::GpuUtil => Self::CpuPercent,
            Self::CpuPercent => Self::HostMemory,
            Self::HostMemory => Self::Pid,
            Self::Pid => Self::GpuMemory,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::GpuMemory => "GPU memory",
            Self::GpuUtil => "GPU util",
            Self::CpuPercent => "CPU %",
            Self::HostMemory => "RSS",
            Self::Pid => "PID",
        }
    }
}

/// Command payloads. `UpdateViewSession` field semantics depend on `view`:
/// * `processes`: `selected_process` = selection, `selected_entity_id` = GPU
///   filter (None = all GPUs), `filter` = name filter.
/// * `gpu_detail`: `selected_entity_id` = shown GPU.
/// * `history`/`sources`: `selected_entity_id` = series selector
///   (`metric_id|scope|entity_id`), `filter` = the sources text filter
///   (empty on the history view).
/// * `overview_bar` / `process_bar` (D-80, a contract extension in
///   `contracts/command.schema.json`): the bar sizes of the Overview and the
///   Process History; unset is 1 minute and `auto`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum CommandBody {
    SetSamplingInterval {
        interval_ms: u32,
        expected_config_revision: u32,
    },
    ReplaceConfig {
        expected_config_revision: u32,
        candidate: Box<Config>,
    },
    UpdateViewSession {
        view: ViewKind,
        selected_entity_id: Option<EntityId>,
        selected_process: Option<ProcessKey>,
        filter: String,
        sort: SortKey,
        ascending: bool,
        #[serde(with = "crate::wire::u64_decimal")]
        frame_revision: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overview_bar: Option<crate::tiers::BarSize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        process_bar: Option<crate::tiers::BarSize>,
    },
    PrepareSignal {
        target: ProcessKey,
        signal: Signal,
    },
    ConfirmSignal {
        confirmation_token: ConfirmationToken,
    },
    CancelSignal {
        confirmation_token: ConfirmationToken,
    },
    SetModuleEnabled {
        module_id: ModuleId,
        enabled: bool,
        expected_config_revision: u32,
    },
    Shutdown {
        reason: String,
    },
}

impl CommandBody {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::SetSamplingInterval { .. } => "SetSamplingInterval",
            Self::ReplaceConfig { .. } => "ReplaceConfig",
            Self::UpdateViewSession { .. } => "UpdateViewSession",
            Self::PrepareSignal { .. } => "PrepareSignal",
            Self::ConfirmSignal { .. } => "ConfirmSignal",
            Self::CancelSignal { .. } => "CancelSignal",
            Self::SetModuleEnabled { .. } => "SetModuleEnabled",
            Self::Shutdown { .. } => "Shutdown",
        }
    }

    /// Pure navigation that may be coalesced under input bursts.
    pub const fn is_coalescible_navigation(&self) -> bool {
        matches!(self, Self::UpdateViewSession { .. })
    }
}

/// Envelope carried from a Controller to the Manager.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandEnvelope {
    pub schema_version: u32,
    pub request_id: Uuid,
    pub session_id: SessionId,
    pub body: CommandBody,
}

/// Syntax-level rejection performed by the Controller or at submission.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CommandSyntaxError {
    #[error("schema_version must be 2")]
    Version,
    #[error("filter exceeds {MAX_FILTER_BYTES} bytes")]
    FilterTooLong,
    #[error("shutdown reason exceeds 256 bytes")]
    ReasonTooLong,
    #[error("interval_ms must be 100..=60000")]
    Interval,
    #[error("revision must be 1..=2147483647")]
    Revision,
    #[error("command payload exceeds {0} bytes")]
    TooLarge(u64),
}

impl CommandEnvelope {
    pub fn new(session_id: SessionId, body: CommandBody) -> Self {
        Self {
            schema_version: COMMAND_SCHEMA_VERSION,
            request_id: Uuid::new_v4(),
            session_id,
            body,
        }
    }

    /// Schema-level checks (the Manager re-validates semantics).
    pub fn validate_syntax(&self) -> Result<(), CommandSyntaxError> {
        if self.schema_version != COMMAND_SCHEMA_VERSION {
            return Err(CommandSyntaxError::Version);
        }
        let revision_ok = |revision: u32| revision >= 1 && i32::try_from(revision).is_ok();
        match &self.body {
            CommandBody::SetSamplingInterval {
                interval_ms,
                expected_config_revision,
            } => {
                if !(100..=60_000).contains(interval_ms) {
                    return Err(CommandSyntaxError::Interval);
                }
                if !revision_ok(*expected_config_revision) {
                    return Err(CommandSyntaxError::Revision);
                }
            }
            CommandBody::ReplaceConfig {
                expected_config_revision,
                ..
            }
            | CommandBody::SetModuleEnabled {
                expected_config_revision,
                ..
            } => {
                if !revision_ok(*expected_config_revision) {
                    return Err(CommandSyntaxError::Revision);
                }
            }
            CommandBody::UpdateViewSession { filter, .. } => {
                if filter.len() > MAX_FILTER_BYTES {
                    return Err(CommandSyntaxError::FilterTooLong);
                }
            }
            CommandBody::Shutdown { reason } => {
                if reason.len() > 256 {
                    return Err(CommandSyntaxError::ReasonTooLong);
                }
            }
            CommandBody::PrepareSignal { .. }
            | CommandBody::ConfirmSignal { .. }
            | CommandBody::CancelSignal { .. } => {}
        }
        Ok(())
    }

    /// Payload digest for "same request_id, different payload" detection.
    pub fn payload_digest(&self) -> String {
        crate::digest::sha256_json(&(&self.session_id, &self.body)).unwrap_or_default()
    }

    /// Serialized size, used for the per-command byte cap.
    pub fn encoded_len(&self) -> u64 {
        serde_json::to_vec(self)
            .map(|bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            .unwrap_or(u64::MAX)
    }
}

/// Command status (`command-result.schema.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Accepted,
    Completed,
    Rejected,
    InProgress,
    Unknown,
}

/// Result delivered on the session's result channel (never coalesced away).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandResult {
    pub schema_version: u32,
    pub request_id: Uuid,
    pub status: CommandStatus,
    pub code: ResultCode,
    pub message: String,
    pub config_revision: Option<u32>,
    pub effective_interval_ms: Option<u32>,
    pub action_state: Option<ActionState>,
    pub confirmation_token: Option<ConfirmationToken>,
    #[serde(with = "crate::wire::opt_u64_decimal")]
    pub expires_boot_ns: Option<u64>,
}

/// Message marker of a `commit_uncertain` error (INTERFACES.md §9).
pub const COMMIT_UNCERTAIN_MARKER: &str = "(commit_uncertain)";

impl CommandResult {
    fn base(request_id: Uuid, status: CommandStatus, code: ResultCode, message: &str) -> Self {
        Self {
            schema_version: COMMAND_SCHEMA_VERSION,
            request_id,
            status,
            code,
            message: crate::text::sanitize_message(message),
            config_revision: None,
            effective_interval_ms: None,
            action_state: None,
            confirmation_token: None,
            expires_boot_ns: None,
        }
    }

    pub fn completed(request_id: Uuid, message: &str) -> Self {
        Self::base(
            request_id,
            CommandStatus::Completed,
            ResultCode::OK,
            message,
        )
    }

    pub fn accepted(request_id: Uuid) -> Self {
        Self::base(
            request_id,
            CommandStatus::Accepted,
            ResultCode::OK,
            "accepted",
        )
    }

    pub fn in_progress(request_id: Uuid, message: &str) -> Self {
        Self::base(
            request_id,
            CommandStatus::InProgress,
            ResultCode::OK,
            message,
        )
    }

    pub fn rejected(request_id: Uuid, code: ErrorCode, message: &str) -> Self {
        Self::base(
            request_id,
            CommandStatus::Rejected,
            ResultCode::from(code),
            message,
        )
    }

    pub fn unknown(request_id: Uuid, message: &str) -> Self {
        Self::base(
            request_id,
            CommandStatus::Unknown,
            ResultCode::from(ErrorCode::UnknownOutcome),
            message,
        )
    }

    pub fn from_error(request_id: Uuid, error: &ErrorDetail) -> Self {
        if error.commit_uncertain && !error.message.contains(COMMIT_UNCERTAIN_MARKER) {
            let message = format!("{} {COMMIT_UNCERTAIN_MARKER}", error.message);
            return Self::rejected(request_id, error.code, &message);
        }
        Self::rejected(request_id, error.code, &error.message)
    }

    /// The outcome of a persistent write could not be determined (the result
    /// schema is closed, so the marker travels in the message): never report
    /// such a change as "not made".
    pub fn commit_uncertain(&self) -> bool {
        self.message.contains(COMMIT_UNCERTAIN_MARKER)
    }

    /// Action state projected onto the result enum.
    pub fn with_action_state(mut self, state: ActionState) -> Self {
        self.action_state = Some(state.result_state());
        self
    }
}

/// Immediate submission rejection (queue full or invalid syntax).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("command {request_id} rejected: {code:?} {message}")]
pub struct CommandRejection {
    pub request_id: Uuid,
    pub code: ErrorCode,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncertain_commits_stay_recognizable_in_the_closed_result_schema() {
        let request_id = Uuid::nil();
        let plain = ErrorDetail::new(ErrorCode::PersistFailed, "disk full");
        assert!(!CommandResult::from_error(request_id, &plain).commit_uncertain());
        let uncertain = ErrorDetail::new(ErrorCode::PersistFailed, "dir fsync failed").uncertain();
        let result = CommandResult::from_error(request_id, &uncertain);
        assert!(result.commit_uncertain(), "{}", result.message);
        // A message that already names the marker carries it once.
        let named = ErrorDetail::new(
            ErrorCode::PersistFailed,
            "commit outcome uncertain (commit_uncertain): x",
        )
        .uncertain();
        let message = CommandResult::from_error(request_id, &named).message;
        assert_eq!(message.matches(COMMIT_UNCERTAIN_MARKER).count(), 1);
    }

    #[test]
    fn spec_prepare_signal_example_parses() {
        let text = include_str!("../../../spec/dgxtop-v2/examples/command.prepare-signal.json");
        let envelope: CommandEnvelope = serde_json::from_str(text).unwrap();
        envelope.validate_syntax().unwrap();
        assert!(matches!(
            envelope.body,
            CommandBody::PrepareSignal {
                signal: Signal::Sigterm,
                ..
            }
        ));
        assert_eq!(
            serde_json::to_value(&envelope).unwrap(),
            serde_json::from_str::<serde_json::Value>(text).unwrap()
        );
    }

    #[test]
    fn spec_interval_result_example_parses() {
        let text = include_str!("../../../spec/dgxtop-v2/examples/command-result.interval.json");
        let result: CommandResult = serde_json::from_str(text).unwrap();
        assert_eq!(result.status, CommandStatus::Completed);
        assert!(result.code.is_ok());
        assert_eq!(result.effective_interval_ms, Some(1000));
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::from_str::<serde_json::Value>(text).unwrap()
        );
    }

    #[test]
    fn syntax_rejections() {
        let session = SessionId::new("s").unwrap();
        let too_long = CommandEnvelope::new(
            session.clone(),
            CommandBody::UpdateViewSession {
                view: ViewKind::Processes,
                selected_entity_id: None,
                selected_process: None,
                filter: "x".repeat(257),
                sort: SortKey::Pid,
                ascending: true,
                frame_revision: 1,
                overview_bar: None,
                process_bar: None,
            },
        );
        assert_eq!(
            too_long.validate_syntax(),
            Err(CommandSyntaxError::FilterTooLong)
        );
        let interval = CommandEnvelope::new(
            session,
            CommandBody::SetSamplingInterval {
                interval_ms: 50,
                expected_config_revision: 1,
            },
        );
        assert_eq!(
            interval.validate_syntax(),
            Err(CommandSyntaxError::Interval)
        );
    }

    #[test]
    fn unknown_command_type_is_rejected() {
        let bad = serde_json::json!({
            "schema_version": 2,
            "request_id": "d31ce8b6-8a16-4c34-875b-d9d999312239",
            "session_id": "s",
            "body": {"type": "Execute", "command": "rm -rf /"}
        });
        assert!(serde_json::from_value::<CommandEnvelope>(bad).is_err());
    }

    #[test]
    fn payload_digest_detects_changes() {
        let session = SessionId::new("s").unwrap();
        let first = CommandEnvelope::new(
            session.clone(),
            CommandBody::Shutdown { reason: "a".into() },
        );
        let mut same = first.clone();
        same.request_id = first.request_id;
        assert_eq!(first.payload_digest(), same.payload_digest());
        let different = CommandEnvelope::new(session, CommandBody::Shutdown { reason: "b".into() });
        assert_ne!(first.payload_digest(), different.payload_digest());
    }
}
