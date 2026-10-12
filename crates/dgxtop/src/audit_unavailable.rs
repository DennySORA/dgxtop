//! Audit repository used when the SQLite runtime gate fails: every operation
//! fails closed, so no process action can ever proceed without a durable
//! intent.

use dgxtop_core::action::{
    ActionAttempt, ActionIntent, ActionOutcomeRecord, AuditExpiryReport, AuditFailure,
    DurableReceipt, PersistenceKind, PersistenceState, PersistenceStatus, RecoveredIntent,
};
use dgxtop_core::errors::ErrorCode;
use dgxtop_core::ports::AuditRepository;

/// Fail-closed audit stand-in with a fixed reason.
#[derive(Debug, Clone)]
pub struct UnavailableAudit {
    reason: String,
}

impl UnavailableAudit {
    pub fn new(reason: &str) -> Self {
        Self {
            reason: dgxtop_core::text::sanitize_message(reason),
        }
    }

    fn failure(&self) -> AuditFailure {
        AuditFailure {
            code: ErrorCode::Unavailable,
            message: format!("audit unavailable: {}", self.reason),
            commit_uncertain: false,
        }
    }
}

impl AuditRepository for UnavailableAudit {
    fn append_intent(&mut self, _intent: &ActionIntent) -> Result<DurableReceipt, AuditFailure> {
        Err(self.failure())
    }

    fn append_outcome(&mut self, _outcome: &ActionOutcomeRecord) -> Result<(), AuditFailure> {
        Err(self.failure())
    }

    fn append_attempt(&mut self, _attempt: &ActionAttempt) -> Result<(), AuditFailure> {
        Err(self.failure())
    }

    fn recover_unknown(
        &mut self,
        _now_boot_ns: u64,
        _now_utc: &str,
    ) -> Result<Vec<RecoveredIntent>, AuditFailure> {
        Err(self.failure())
    }

    fn expire(&mut self, _before_utc: &str) -> Result<AuditExpiryReport, AuditFailure> {
        Err(self.failure())
    }

    fn status(&mut self) -> PersistenceStatus {
        PersistenceStatus {
            kind: PersistenceKind::Audit,
            state: PersistenceState::Failed,
            detail: Some(self.reason.clone()),
            bytes_used: 0,
            bytes_limit: 0,
            wal_bytes: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt() -> ActionAttempt {
        ActionAttempt {
            request_id: uuid::Uuid::nil(),
            payload_sha256: String::new(),
            session_digest: String::new(),
            result_code: "Ok".to_owned(),
            detail: String::new(),
            occurred_utc: "1970-01-01T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn every_operation_fails_closed() {
        let mut audit = UnavailableAudit::new("gate failed");
        assert!(audit.recover_unknown(0, "1970-01-01T00:00:00Z").is_err());
        assert!(audit.append_attempt(&attempt()).is_err());
        assert!(audit.expire("1970-01-01T00:00:00Z").is_err());
        assert!(!audit.status().accepts_writes());
    }
}
