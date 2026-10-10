//! Read-back of the audit database after a stack stopped, through the
//! store's public repository API (the suite has no direct SQLite access).

use std::path::Path;
use std::sync::Arc;

use dgxtop_core::action::{ActionRecordSummary, RecoveredIntent};
use dgxtop_core::clock::{TimeProvider, format_utc_rfc3339};
use dgxtop_core::ports::AuditRepository;
use dgxtop_store::audit::{AuditLimits, SqliteAuditRepository};
use dgxtop_store::sqlite::StatvfsFreeSpace;
use uuid::Uuid;

pub struct AuditTrail {
    repository: SqliteAuditRepository,
    clock: Arc<dyn TimeProvider>,
}

impl AuditTrail {
    pub fn open(path: &Path, clock: Arc<dyn TimeProvider>) -> Self {
        let mut repository = SqliteAuditRepository::open(
            path,
            AuditLimits::default(),
            Arc::clone(&clock),
            Arc::new(StatvfsFreeSpace),
        )
        .expect("reopen the audit database");
        let status = repository.status();
        assert!(
            status.accepts_writes(),
            "the audit database must reopen healthy: {status:?}"
        );
        Self { repository, clock }
    }

    /// What the audit holds for one request: its intent (with the latest
    /// outcome) or, for requests that never produced an intent, its attempt.
    pub fn record(&mut self, request_id: Uuid) -> Option<ActionRecordSummary> {
        self.repository
            .lookup(request_id)
            .expect("audit lookup succeeds")
    }

    /// Marks and returns every intent still lacking an outcome: exactly
    /// what the next start would recover as `unknown`.
    pub fn recover_unresolved(&mut self) -> Vec<RecoveredIntent> {
        let now_utc = format_utc_rfc3339(self.clock.now_utc_ns());
        self.repository
            .recover_unknown(self.clock.now_boot_ns(), &now_utc)
            .expect("audit recovery succeeds")
    }
}
