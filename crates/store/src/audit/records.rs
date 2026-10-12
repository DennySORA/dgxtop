//! Row construction and validation for `audit.sql`, plus the mapping between
//! action states and the `action_intents.status` column.
//!
//! Everything here is pure so the privacy and validation rules are unit
//! tested without a database.

use dgxtop_core::action::{
    ActionAttempt, ActionIntent, ActionOutcomeRecord, ActionState, AuditFailure, Signal,
};
use dgxtop_core::errors::ErrorCode;
use dgxtop_core::identity::ProcessKey;
use dgxtop_core::text::{audit_command_name, sanitize_message};
use dgxtop_core::wire::format_fixed_width;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::sqlite::is_rfc3339_utc;

/// `CHECK(length(process_key_json)<=4096)`.
pub(crate) const MAX_TARGET_JSON_BYTES: usize = 4096;
/// `CHECK(length(detail_json)<=4096)`.
pub(crate) const MAX_DETAIL_JSON_BYTES: usize = 4096;
const MAX_SESSION_DIGEST_BYTES: usize = 256;
const MAX_RESULT_CODE_BYTES: usize = 64;

pub(crate) fn failure(code: ErrorCode, message: impl AsRef<str>) -> AuditFailure {
    AuditFailure {
        code,
        message: sanitize_message(message.as_ref()),
        commit_uncertain: false,
    }
}

pub(crate) fn invalid(message: impl AsRef<str>) -> AuditFailure {
    failure(ErrorCode::ValidationFailed, message)
}

/// Content of `process_key_json`: the stable identity plus the uid and the
/// escaped command name needed for an audit trail, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredTarget {
    pub key: ProcessKey,
    pub uid: u32,
    pub basename: String,
}

pub(crate) fn uuid_text(id: Uuid) -> String {
    id.hyphenated().to_string()
}

fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validated_digest(text: &str) -> Result<String, AuditFailure> {
    if is_sha256_hex(text) {
        Ok(text.to_owned())
    } else {
        Err(invalid(
            "payload_sha256 must be 64 lower-case hex characters",
        ))
    }
}

fn validated_session_digest(text: &str) -> Result<String, AuditFailure> {
    let valid = !text.is_empty()
        && text.len() <= MAX_SESSION_DIGEST_BYTES
        && !text.chars().any(char::is_control);
    if valid {
        Ok(text.to_owned())
    } else {
        Err(invalid(
            "session_digest must be 1..=256 bytes without control characters",
        ))
    }
}

fn validated_utc(field: &str, text: &str) -> Result<String, AuditFailure> {
    if is_rfc3339_utc(text) {
        Ok(text.to_owned())
    } else {
        Err(invalid(format!(
            "{field} must be RFC 3339 UTC like 2026-10-04T12:00:00Z"
        )))
    }
}

fn validated_result_code(text: &str) -> Result<String, AuditFailure> {
    let valid = !text.is_empty()
        && text.len() <= MAX_RESULT_CODE_BYTES
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if valid {
        Ok(text.to_owned())
    } else {
        Err(invalid(
            "result_code must be 1..=64 ASCII letters, digits, '_', '-' or '.'",
        ))
    }
}

/// `{"detail": "<sanitized>"}` plus optional extra string fields.
fn detail_json(detail: &str, extra: &[(&str, &str)]) -> Result<String, AuditFailure> {
    let mut object = serde_json::Map::new();
    object.insert(
        "detail".to_owned(),
        serde_json::Value::String(sanitize_message(detail)),
    );
    for (key, value) in extra {
        object.insert(
            (*key).to_owned(),
            serde_json::Value::String((*value).to_owned()),
        );
    }
    let text = serde_json::to_string(&object)
        .map_err(|error| invalid(format!("detail cannot be encoded: {error}")))?;
    if text.len() > MAX_DETAIL_JSON_BYTES {
        return Err(invalid("detail_json exceeds 4096 bytes"));
    }
    Ok(text)
}

pub(crate) fn signal_name(signal: Signal) -> &'static str {
    signal.name()
}

pub(crate) fn parse_signal(text: &str) -> Option<Signal> {
    match text {
        "SIGTERM" => Some(Signal::Sigterm),
        "SIGKILL" => Some(Signal::Sigkill),
        _ => None,
    }
}

/// One `action_intents` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IntentRow {
    pub request_id: String,
    pub intent_id: String,
    pub session_digest: String,
    pub target_json: String,
    pub signal: &'static str,
    pub policy_revision: i64,
    pub created_utc: String,
    pub payload_sha256: String,
    pub boot_id: String,
    pub created_boot_ns: String,
}

impl IntentRow {
    /// `boot_id` is the boot whose clock produced `created_boot_ns`.
    pub(crate) fn build(intent: &ActionIntent, boot_id: &str) -> Result<Self, AuditFailure> {
        if intent.policy_revision == 0 {
            return Err(invalid("policy_revision must be at least 1"));
        }
        let target = StoredTarget {
            key: intent.process_key.clone(),
            uid: intent.uid,
            basename: audit_command_name(&intent.basename),
        };
        let target_json = serde_json::to_string(&target)
            .map_err(|error| invalid(format!("process identity cannot be encoded: {error}")))?;
        if target_json.len() > MAX_TARGET_JSON_BYTES {
            return Err(invalid("process identity exceeds 4096 bytes"));
        }
        Ok(Self {
            request_id: uuid_text(intent.request_id),
            intent_id: uuid_text(intent.intent_id),
            session_digest: validated_session_digest(&intent.session_digest)?,
            target_json,
            signal: signal_name(intent.signal),
            policy_revision: i64::from(intent.policy_revision),
            created_utc: validated_utc("created_utc", &intent.created_utc)?,
            payload_sha256: validated_digest(&intent.payload_sha256)?,
            boot_id: boot_id.to_owned(),
            created_boot_ns: format_fixed_width(intent.created_boot_ns),
        })
    }
}

/// One `action_events` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EventRow {
    pub event_id: String,
    pub request_id: String,
    pub occurred_utc: String,
    pub boot_id: String,
    pub occurred_boot_ns: String,
    pub event_kind: &'static str,
    pub result_code: String,
    pub detail_json: String,
    /// Status the intent moves to when the transition is allowed.
    pub next_status: &'static str,
}

impl EventRow {
    pub(crate) fn from_outcome(
        outcome: &ActionOutcomeRecord,
        boot_id: &str,
    ) -> Result<Self, AuditFailure> {
        let next_status = intent_status(outcome.state).ok_or_else(|| {
            invalid(format!(
                "{} is not an outcome of a committed intent",
                outcome.state.name()
            ))
        })?;
        Ok(Self {
            event_id: uuid_text(outcome.event_id),
            request_id: uuid_text(outcome.request_id),
            occurred_utc: validated_utc("occurred_utc", &outcome.occurred_utc)?,
            boot_id: boot_id.to_owned(),
            occurred_boot_ns: format_fixed_width(outcome.occurred_boot_ns),
            event_kind: outcome.state.name(),
            result_code: validated_result_code(&outcome.result_code)?,
            detail_json: detail_json(&outcome.detail, &[])?,
            next_status,
        })
    }

    /// The event written when a restart finds an intent without outcome.
    pub(crate) fn recovery(
        request_id: &str,
        previous_status: &str,
        now_boot_ns: u64,
        now_utc: &str,
        boot_id: &str,
    ) -> Result<Self, AuditFailure> {
        Ok(Self {
            event_id: uuid_text(Uuid::new_v4()),
            request_id: request_id.to_owned(),
            occurred_utc: validated_utc("now_utc", now_utc)?,
            boot_id: boot_id.to_owned(),
            occurred_boot_ns: format_fixed_width(now_boot_ns),
            event_kind: ActionState::Unknown.name(),
            result_code: ErrorCode::UnknownOutcome.as_str().to_owned(),
            detail_json: detail_json(
                "restarted before a provable outcome; marked unknown and never resent",
                &[("previous_status", previous_status)],
            )?,
            next_status: ActionState::Unknown.name(),
        })
    }

    /// Same event content as a stored row (the boot id is not compared: a
    /// retry after a restart carries the same facts under another boot).
    pub(crate) fn same_facts(&self, stored: &StoredEvent) -> bool {
        self.request_id == stored.request_id
            && self.occurred_utc == stored.occurred_utc
            && self.occurred_boot_ns == stored.occurred_boot_ns
            && self.event_kind == stored.event_kind
            && self.result_code == stored.result_code
            && self.detail_json == stored.detail_json
    }
}

/// An `action_events` row read back for idempotency checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredEvent {
    pub request_id: String,
    pub occurred_utc: String,
    pub occurred_boot_ns: String,
    pub event_kind: String,
    pub result_code: String,
    pub detail_json: String,
}

/// One `action_attempts` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttemptRow {
    pub request_id: String,
    pub payload_sha256: String,
    pub session_digest: String,
    pub occurred_utc: String,
    pub result_code: String,
    pub detail_json: String,
}

impl AttemptRow {
    pub(crate) fn build(attempt: &ActionAttempt) -> Result<Self, AuditFailure> {
        Ok(Self {
            request_id: uuid_text(attempt.request_id),
            payload_sha256: validated_digest(&attempt.payload_sha256)?,
            session_digest: validated_session_digest(&attempt.session_digest)?,
            occurred_utc: validated_utc("occurred_utc", &attempt.occurred_utc)?,
            result_code: validated_result_code(&attempt.result_code)?,
            detail_json: detail_json(&attempt.detail, &[])?,
        })
    }
}

/// `action_intents.status` for an outcome state; `None` for states that
/// precede the intent. `not_sent` is stored as `rejected` (its result-state
/// projection) because the DDL status list has no `not_sent`; the event row
/// keeps `event_kind = 'not_sent'`.
pub(crate) fn intent_status(state: ActionState) -> Option<&'static str> {
    match state {
        ActionState::Requested
        | ActionState::Prepared
        | ActionState::Confirming
        | ActionState::IntentPending => None,
        ActionState::NotSent => Some(ActionState::Rejected.name()),
        other => Some(other.name()),
    }
}

pub(crate) fn state_from_status(status: &str) -> Option<ActionState> {
    [
        ActionState::IntentCommitted,
        ActionState::Sending,
        ActionState::SignalSent,
        ActionState::ExitObserved,
        ActionState::StillRunning,
        ActionState::Cancelled,
        ActionState::Expired,
        ActionState::Rejected,
        ActionState::Unknown,
    ]
    .into_iter()
    .find(|state| state.name() == status)
}

/// Whether the stored status may move from `current` to `next`. The status
/// only moves forward along the action state machine; in particular nothing
/// past the point of no return can become `cancelled`/`expired`, and
/// terminal states never change. Disallowed outcomes are still recorded as
/// events; only the status projection keeps its value.
pub(crate) fn transition_allowed(current: &str, next: &str) -> bool {
    let Some(current) = state_from_status(current) else {
        return false;
    };
    let Some(next) = state_from_status(next) else {
        return false;
    };
    match current {
        ActionState::IntentCommitted => next != ActionState::IntentCommitted,
        ActionState::Sending => matches!(
            next,
            ActionState::SignalSent
                | ActionState::ExitObserved
                | ActionState::StillRunning
                | ActionState::Rejected
                | ActionState::Unknown
        ),
        ActionState::SignalSent => matches!(
            next,
            ActionState::ExitObserved | ActionState::StillRunning | ActionState::Unknown
        ),
        _ => false,
    }
}

#[cfg(any(test, feature = "test-support"))]
/// State reported for a request that only has an `action_attempts` row.
pub(crate) fn attempt_state(result_code: &str) -> ActionState {
    match result_code {
        "Ok" => ActionState::Prepared,
        "Cancelled" => ActionState::Cancelled,
        "ConfirmationExpired" => ActionState::Expired,
        _ => ActionState::Rejected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::identity::{BootId, HostId, Pid, PidNamespace};

    fn intent() -> ActionIntent {
        ActionIntent {
            request_id: Uuid::from_u128(1),
            intent_id: Uuid::from_u128(2),
            session_digest: "session-digest".to_owned(),
            process_key: ProcessKey {
                host_id: HostId::new("host").unwrap(),
                boot_id: BootId::new("boot").unwrap(),
                pid_namespace: PidNamespace::new("pid:[4026531836]").unwrap(),
                pid: Pid::new(4242).unwrap(),
                start_time_ticks: 90_100,
            },
            uid: 1000,
            basename: "python3\u{1b}[31m".to_owned(),
            signal: Signal::Sigterm,
            policy_revision: 3,
            payload_sha256: "a".repeat(64),
            created_boot_ns: 7,
            created_utc: "2026-10-04T12:00:00Z".to_owned(),
        }
    }

    #[test]
    fn intent_row_keeps_only_identity_uid_and_basename() {
        let row = IntentRow::build(&intent(), "boot").unwrap();
        assert_eq!(row.created_boot_ns, "00000000000000000007");
        assert_eq!(row.signal, "SIGTERM");
        let target: StoredTarget = serde_json::from_str(&row.target_json).unwrap();
        assert_eq!(target.basename, r"python3\u{1b}[31m");
        assert_eq!(target.uid, 1000);
        assert_eq!(target.key, intent().process_key);
        let value: serde_json::Value = serde_json::from_str(&row.target_json).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["basename", "key", "uid"]);
    }

    #[test]
    fn intent_rows_keep_an_escaped_command_name_with_its_slashes() {
        let mut hostile = intent();
        hostile.basename = "kworker/0:1H\u{1b}[2J".to_owned();
        let row = IntentRow::build(&hostile, "boot").unwrap();
        let target: StoredTarget = serde_json::from_str(&row.target_json).unwrap();
        // The Store keeps a bounded, auditable escaped form.
        assert_eq!(target.basename, r"kworker/0:1H\u{1b}[2J");
        assert!(!row.target_json.contains('\u{1b}'));
    }

    #[test]
    fn intent_validation_rejects_bad_fields() {
        let mut bad = intent();
        bad.payload_sha256 = "A".repeat(64);
        assert_eq!(
            IntentRow::build(&bad, "boot").unwrap_err().code,
            ErrorCode::ValidationFailed
        );
        let mut bad = intent();
        bad.created_utc = "yesterday".to_owned();
        assert!(IntentRow::build(&bad, "boot").is_err());
        let mut bad = intent();
        bad.policy_revision = 0;
        assert!(IntentRow::build(&bad, "boot").is_err());
        let mut bad = intent();
        bad.session_digest = String::new();
        assert!(IntentRow::build(&bad, "boot").is_err());
        let mut bad = intent();
        bad.session_digest = "x\u{7}".to_owned();
        assert!(IntentRow::build(&bad, "boot").is_err());
    }

    #[test]
    fn outcome_rows_map_states_and_sanitize_detail() {
        let outcome = ActionOutcomeRecord {
            event_id: Uuid::from_u128(9),
            request_id: Uuid::from_u128(1),
            intent_id: Uuid::from_u128(2),
            state: ActionState::NotSent,
            result_code: "ConfirmationExpired".to_owned(),
            detail: "late \u{1b}]0;title\u{7}receipt".to_owned(),
            occurred_boot_ns: u64::MAX,
            occurred_utc: "2026-10-04T12:00:01.250Z".to_owned(),
        };
        let row = EventRow::from_outcome(&outcome, "boot").unwrap();
        assert_eq!(row.event_kind, "not_sent");
        assert_eq!(row.next_status, "rejected");
        assert_eq!(row.occurred_boot_ns, "18446744073709551615");
        let detail: serde_json::Value = serde_json::from_str(&row.detail_json).unwrap();
        assert_eq!(detail["detail"], "late receipt");
        let mut early = outcome.clone();
        early.state = ActionState::Confirming;
        assert!(EventRow::from_outcome(&early, "boot").is_err());
        let mut bad_code = outcome;
        bad_code.result_code = "no spaces".to_owned();
        assert!(EventRow::from_outcome(&bad_code, "boot").is_err());
    }

    #[test]
    fn recovery_event_is_unknown_outcome() {
        let row = EventRow::recovery("req", "sending", 5, "2026-10-04T12:00:00Z", "boot2").unwrap();
        assert_eq!(row.event_kind, "unknown");
        assert_eq!(row.result_code, "UnknownOutcome");
        assert_eq!(row.next_status, "unknown");
        assert!(row.detail_json.contains("\"previous_status\":\"sending\""));
        assert!(EventRow::recovery("req", "sending", 5, "not-a-time", "boot2").is_err());
    }

    #[test]
    fn status_only_moves_forward() {
        assert!(transition_allowed("intent_committed", "sending"));
        assert!(transition_allowed("intent_committed", "cancelled"));
        assert!(transition_allowed("intent_committed", "unknown"));
        assert!(transition_allowed("sending", "signal_sent"));
        assert!(transition_allowed("sending", "rejected"));
        assert!(!transition_allowed("sending", "cancelled"));
        assert!(!transition_allowed("sending", "expired"));
        assert!(!transition_allowed("signal_sent", "sending"));
        assert!(!transition_allowed("signal_sent", "cancelled"));
        assert!(transition_allowed("signal_sent", "exit_observed"));
        assert!(!transition_allowed("exit_observed", "unknown"));
        assert!(!transition_allowed("unknown", "signal_sent"));
        assert!(!transition_allowed("intent_committed", "intent_committed"));
        assert!(!transition_allowed("bogus", "sending"));
    }

    #[test]
    fn attempts_derive_their_state_from_the_result_code() {
        assert_eq!(attempt_state("Ok"), ActionState::Prepared);
        assert_eq!(attempt_state("Cancelled"), ActionState::Cancelled);
        assert_eq!(attempt_state("ConfirmationExpired"), ActionState::Expired);
        assert_eq!(attempt_state("CapabilityDenied"), ActionState::Rejected);
        let attempt = ActionAttempt {
            request_id: Uuid::from_u128(5),
            payload_sha256: "b".repeat(64),
            session_digest: "s".to_owned(),
            result_code: "CapabilityDenied".to_owned(),
            detail: "x".repeat(5000),
            occurred_utc: "2026-10-04T12:00:00Z".to_owned(),
        };
        let row = AttemptRow::build(&attempt).unwrap();
        assert!(row.detail_json.len() <= MAX_DETAIL_JSON_BYTES);
    }
}
