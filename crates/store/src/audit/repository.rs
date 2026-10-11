//! [`SqliteAuditRepository`], the `AuditRepository` used by the audit worker.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dgxtop_core::action::{
    ActionAttempt, ActionIntent, ActionOutcomeRecord, ActionRecordSummary, ActionState,
    AuditExpiryReport, AuditFailure, DurableReceipt, PersistenceKind, PersistenceState,
    PersistenceStatus, RecoveredIntent,
};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::errors::ErrorCode;
use dgxtop_core::ports::AuditRepository;
use dgxtop_core::wire::parse_fixed_width;
use rusqlite::Connection;
use uuid::Uuid;

use super::limits::{AuditLimits, AuditOpenError};
use super::queries::{
    StoredIntent, UnresolvedIntent, find_attempt, find_event, find_intent, insert_attempt,
    insert_event, insert_intent, latest_result_any, latest_result_since, unresolved_intents,
    update_status,
};
use super::records::{
    AttemptRow, EventRow, IntentRow, StoredTarget, attempt_state, failure, invalid, parse_signal,
    state_from_status, transition_allowed, uuid_text,
};
use super::retention::delete_expired;
use crate::fs::{FileSystemOps, RealFileSystem};
use crate::sqlite::{
    CapacityPolicy, CapacityView, ConnectionProfile, DatabaseSpec, FaultKind, FreeSpaceProbe,
    SchemaKind, StepError, StorageFootprint, TxError, Unusable, VersionGate, WriteClass, Writer,
    ensure_sqlite_version, fault_kind, fault_message, immediate_transaction, is_rfc3339_utc,
    open_database, pages_and_backlog, path_problem,
};

/// Pages one new intent may touch: table leaf, three index leaves, page 1
/// and a possible split.
const INTENT_WRITE_ESTIMATE_BYTES: u64 = 6 * 4096;
/// Pages one event or attempt may touch.
const EVENT_WRITE_ESTIMATE_BYTES: u64 = 4 * 4096;
/// Intents marked unknown per recovery transaction.
const RECOVERY_BATCH: i64 = 256;
/// Upper bound of the list returned by `recover_unknown` (all are marked).
const MAX_RECOVERED_REPORTED: usize = 4096;

/// Abort reasons inside an intent transaction.
enum IntentAbort {
    Exists(StoredIntent),
}

/// Abort reasons inside an outcome transaction.
enum OutcomeAbort {
    UnknownRequest,
    IntentMismatch,
    AlreadyRecorded,
    DifferentContent,
}

/// The action audit on `audit.sqlite3`.
///
/// A file that is not this build's format, or unreadable, is deleted and
/// created afresh at open (D-85), so the audit starts empty and keeps
/// working; what cannot be fixed that way (version gate, another writer, a
/// state directory that is not private) still yields a repository whose
/// status is `Failed` and whose every write fails closed.
///
/// Nothing here decides when rows go (D-96): its owner (the Manager's
/// process service) asks `expire` with the age it keeps; opening, a status
/// read and an append delete nothing.
pub struct SqliteAuditRepository {
    path: PathBuf,
    limits: AuditLimits,
    policy: CapacityPolicy,
    clock: Arc<dyn TimeProvider>,
    free_space: Arc<dyn FreeSpaceProbe>,
    writer: Writer,
    last_issue: Option<String>,
    recreated: Option<String>,
}

impl fmt::Debug for SqliteAuditRepository {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SqliteAuditRepository")
            .field("path", &self.path)
            .field("limits", &self.limits)
            .field("writer", &self.writer)
            .field("last_issue", &self.last_issue)
            .finish_non_exhaustive()
    }
}

fn validate_path(path: &Path) -> Result<(), AuditOpenError> {
    match path_problem(path) {
        Some(reason) => Err(AuditOpenError::InvalidPath {
            path: path.display().to_string(),
            reason: reason.to_owned(),
        }),
        None => Ok(()),
    }
}

fn unusable_failure(unusable: &Unusable) -> AuditFailure {
    failure(
        ErrorCode::PersistFailed,
        format!("audit unavailable: {}", unusable.reason),
    )
}

/// Existing intent for the same request: the same payload yields the
/// original receipt while nothing happened to it yet; anything else is a
/// conflict, so a receipt can never authorize a second signal.
fn resolve_duplicate(
    existing: &StoredIntent,
    row: &IntentRow,
    current_boot_id: &str,
    request_id: Uuid,
) -> Result<DurableReceipt, AuditFailure> {
    let same_payload = existing.payload_sha256 == row.payload_sha256
        && existing.target_json == row.target_json
        && existing.signal == row.signal;
    if !same_payload {
        return Err(failure(
            ErrorCode::Conflict,
            "request_id already holds an intent with a different payload",
        ));
    }
    if existing.status != ActionState::IntentCommitted.name() || existing.boot_id != current_boot_id
    {
        return Err(failure(
            ErrorCode::Conflict,
            format!(
                "the intent of this request already progressed to {} and is never re-issued",
                existing.status
            ),
        ));
    }
    let intent_id = Uuid::parse_str(&existing.intent_id)
        .map_err(|_| failure(ErrorCode::InternalError, "stored intent_id is not a UUID"))?;
    let committed_boot_ns = parse_fixed_width(&existing.created_boot_ns).map_err(|_| {
        failure(
            ErrorCode::InternalError,
            "stored created_boot_ns is invalid",
        )
    })?;
    Ok(DurableReceipt {
        request_id,
        intent_id,
        committed_boot_ns,
    })
}

fn recovered_intent(row: &UnresolvedIntent) -> Option<RecoveredIntent> {
    let target: StoredTarget = serde_json::from_str(&row.target_json).ok()?;
    Some(RecoveredIntent {
        request_id: Uuid::parse_str(&row.request_id).ok()?,
        intent_id: Uuid::parse_str(&row.intent_id).ok()?,
        process_key: target.key,
        signal: parse_signal(&row.signal)?,
        previous_status: row.status.clone(),
    })
}

impl SqliteAuditRepository {
    /// Opens (creating with mode 0600 when missing) and verifies the audit
    /// database. `Err` only for invalid arguments; see the type docs.
    pub fn open(
        path: &Path,
        limits: AuditLimits,
        clock: Arc<dyn TimeProvider>,
        free_space: Arc<dyn FreeSpaceProbe>,
    ) -> Result<Self, AuditOpenError> {
        Self::open_inner(
            path,
            limits,
            clock,
            free_space,
            &RealFileSystem,
            ensure_sqlite_version,
        )
    }

    pub(crate) fn open_inner(
        path: &Path,
        limits: AuditLimits,
        clock: Arc<dyn TimeProvider>,
        free_space: Arc<dyn FreeSpaceProbe>,
        fs: &dyn FileSystemOps,
        gate: VersionGate,
    ) -> Result<Self, AuditOpenError> {
        limits.validate()?;
        validate_path(path)?;
        let policy = limits.capacity_policy();
        let spec = DatabaseSpec {
            path,
            kind: SchemaKind::Audit,
            profile: ConnectionProfile::audit(limits.wal_checkpoint_bytes),
            policy,
        };
        let (writer, recreated) = Writer::new("audit", open_database(&spec, fs, gate));
        Ok(Self {
            path: path.to_path_buf(),
            limits,
            policy,
            clock,
            free_space,
            writer,
            last_issue: None,
            recreated,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn limits(&self) -> &AuditLimits {
        &self.limits
    }

    /// Why the file found at open was deleted and created afresh (D-85),
    /// if it was.
    pub fn recreated(&self) -> Option<&str> {
        self.recreated.as_deref()
    }

    fn connection(&mut self) -> Result<&mut Connection, AuditFailure> {
        self.writer.connection().map_err(unusable_failure)
    }

    /// Classifies a SQLite error like every partition does; corruption
    /// stops the audit until the next start recreates it (D-85).
    fn sqlite_failure(&mut self, context: &str, error: &rusqlite::Error) -> AuditFailure {
        let message = fault_message(context, error);
        match fault_kind(error) {
            FaultKind::Corrupt => {
                self.writer.stop_if_corrupt(error, &message);
                failure(ErrorCode::PersistFailed, message)
            }
            FaultKind::Full => {
                self.last_issue = Some(message.clone());
                failure(ErrorCode::CapacityExceeded, message)
            }
            FaultKind::Busy => failure(ErrorCode::Unavailable, message),
            FaultKind::Constraint => failure(ErrorCode::Conflict, message),
            FaultKind::Io | FaultKind::ReadOnly | FaultKind::CannotOpen | FaultKind::Other => {
                self.last_issue = Some(message.clone());
                failure(ErrorCode::PersistFailed, message)
            }
        }
    }

    fn tx_failure<E>(
        &mut self,
        context: &str,
        error: TxError<E>,
        on_abort: impl FnOnce(E) -> AuditFailure,
    ) -> AuditFailure {
        match error {
            TxError::Abort(reason) => on_abort(reason),
            TxError::Sqlite(error) => self.sqlite_failure(context, &error),
            // Stricter than the shared InfraError mapping on purpose: a
            // receipt must never stand for an intent that may be lost, so
            // only a refusal SQLite reports before writing is certain.
            TxError::Commit(error) => {
                let certain = matches!(fault_kind(&error), FaultKind::Full | FaultKind::Constraint);
                let mut result = self.sqlite_failure(&format!("{context} (commit)"), &error);
                result.commit_uncertain = !certain;
                result
            }
            TxError::BusyTimeout { elapsed_ms } => failure(
                ErrorCode::Unavailable,
                format!(
                    "{context}: audit database busy for {elapsed_ms} ms (deadline {} ms)",
                    self.limits.busy_deadline_ms
                ),
            ),
        }
    }

    /// Current measurements; for new records also the free disk space (a
    /// failing probe refuses new records instead of guessing).
    fn capacity_view(&mut self, class: WriteClass) -> Result<CapacityView, AuditFailure> {
        let footprint = StorageFootprint::measure(&self.path).map_err(|error| {
            failure(
                ErrorCode::PersistFailed,
                format!("cannot measure audit files: {error}"),
            )
        })?;
        let wal_checkpoint_bytes = self.limits.wal_checkpoint_bytes;
        let measured = pages_and_backlog(
            self.connection()?,
            footprint.wal_bytes,
            wal_checkpoint_bytes,
        );
        let (pages, wal_backlog_bytes) =
            measured.map_err(|error| self.sqlite_failure("audit capacity", &error))?;
        let free_disk_bytes = match class {
            WriteClass::NewRecord => {
                let directory = self.path.parent().unwrap_or(Path::new("/"));
                self.free_space
                    .available_bytes(directory)
                    .map_err(|error| {
                        failure(
                            ErrorCode::Unavailable,
                            format!("free-space probe failed, refusing new records: {error}"),
                        )
                    })?
            }
            WriteClass::ReservedOutcome => u64::MAX,
        };
        Ok(CapacityView {
            footprint,
            pages,
            wal_backlog_bytes,
            free_disk_bytes,
        })
    }

    fn admit(&mut self, class: WriteClass, estimate_bytes: u64) -> Result<(), AuditFailure> {
        let view = self.capacity_view(class)?;
        self.policy
            .admit(&view, class, estimate_bytes)
            .map_err(|refusal| {
                let action = match class {
                    WriteClass::NewRecord => "refused a new record",
                    WriteClass::ReservedOutcome => "cannot record the outcome",
                };
                failure(
                    ErrorCode::CapacityExceeded,
                    format!("audit {action}: {refusal}"),
                )
            })
    }

    fn deadline(&self) -> std::time::Duration {
        self.limits.busy_deadline()
    }

    fn state_and_detail(&mut self) -> (PersistenceState, Option<String>) {
        if let Some(unusable) = self.writer.unusable() {
            return (PersistenceState::Failed, Some(unusable.reason.clone()));
        }
        let view = match self.capacity_view(WriteClass::NewRecord) {
            Ok(view) => view,
            Err(error) => return (PersistenceState::Failed, Some(error.message)),
        };
        if let Err(refusal) =
            self.policy
                .admit(&view, WriteClass::NewRecord, INTENT_WRITE_ESTIMATE_BYTES)
        {
            return (
                PersistenceState::Failed,
                Some(format!("new actions refused: {refusal}")),
            );
        }
        if self.policy.is_near_limits(&view) {
            return (
                PersistenceState::Degraded,
                Some(format!(
                    "near a capacity threshold (wal backlog {} bytes)",
                    view.wal_backlog_bytes
                )),
            );
        }
        match &self.last_issue {
            Some(issue) => (PersistenceState::Degraded, Some(issue.clone())),
            None => (PersistenceState::Healthy, None),
        }
    }
}

impl AuditRepository for SqliteAuditRepository {
    fn append_intent(&mut self, intent: &ActionIntent) -> Result<DurableReceipt, AuditFailure> {
        let boot_id = self.clock.boot_id();
        let row = IntentRow::build(intent, boot_id.as_str())?;
        let existing = find_intent(self.connection()?, &row.request_id);
        let existing =
            existing.map_err(|error| self.sqlite_failure("audit intent lookup", &error))?;
        if let Some(existing) = existing {
            return resolve_duplicate(&existing, &row, boot_id.as_str(), intent.request_id);
        }
        self.admit(WriteClass::NewRecord, INTENT_WRITE_ESTIMATE_BYTES)?;
        let deadline = self.deadline();
        let committed = immediate_transaction(self.connection()?, deadline, |transaction| {
            if let Some(existing) = find_intent(transaction, &row.request_id)? {
                return Err(StepError::Abort(IntentAbort::Exists(existing)));
            }
            insert_intent(transaction, &row)?;
            Ok(())
        });
        match committed {
            // The receipt exists only now that COMMIT (FULL) returned.
            Ok(()) => {
                self.last_issue = None;
                Ok(DurableReceipt {
                    request_id: intent.request_id,
                    intent_id: intent.intent_id,
                    committed_boot_ns: self.clock.now_boot_ns(),
                })
            }
            Err(TxError::Abort(IntentAbort::Exists(existing))) => {
                resolve_duplicate(&existing, &row, boot_id.as_str(), intent.request_id)
            }
            Err(error) => Err(self.tx_failure("audit intent", error, |abort| match abort {
                IntentAbort::Exists(_) => failure(ErrorCode::Conflict, "intent already exists"),
            })),
        }
    }

    fn append_outcome(&mut self, outcome: &ActionOutcomeRecord) -> Result<(), AuditFailure> {
        let boot_id = self.clock.boot_id();
        let row = EventRow::from_outcome(outcome, boot_id.as_str())?;
        let intent_id = uuid_text(outcome.intent_id);
        self.connection()?;
        self.admit(WriteClass::ReservedOutcome, EVENT_WRITE_ESTIMATE_BYTES)?;
        let deadline = self.deadline();
        let recorded = immediate_transaction(self.connection()?, deadline, |transaction| {
            let Some(intent) = find_intent(transaction, &row.request_id)? else {
                return Err(StepError::Abort(OutcomeAbort::UnknownRequest));
            };
            if intent.intent_id != intent_id {
                return Err(StepError::Abort(OutcomeAbort::IntentMismatch));
            }
            if let Some(stored) = find_event(transaction, &row.event_id)? {
                let abort = if row.same_facts(&stored) {
                    OutcomeAbort::AlreadyRecorded
                } else {
                    OutcomeAbort::DifferentContent
                };
                return Err(StepError::Abort(abort));
            }
            insert_event(transaction, &row)?;
            if transition_allowed(&intent.status, row.next_status) {
                update_status(transaction, &row.request_id, row.next_status)?;
            }
            Ok(())
        });
        match recorded {
            Ok(()) | Err(TxError::Abort(OutcomeAbort::AlreadyRecorded)) => {
                self.last_issue = None;
                Ok(())
            }
            Err(error) => Err(
                self.tx_failure("audit outcome", error, |abort| match abort {
                    OutcomeAbort::UnknownRequest => {
                        invalid("no committed intent for this request_id")
                    }
                    OutcomeAbort::IntentMismatch => failure(
                        ErrorCode::Conflict,
                        "intent_id does not match the committed intent of this request",
                    ),
                    OutcomeAbort::DifferentContent | OutcomeAbort::AlreadyRecorded => failure(
                        ErrorCode::Conflict,
                        "event_id already recorded with different content",
                    ),
                }),
            ),
        }
    }

    fn append_attempt(&mut self, attempt: &ActionAttempt) -> Result<(), AuditFailure> {
        let row = AttemptRow::build(attempt)?;
        let existing = find_attempt(self.connection()?, &row.request_id);
        let existing =
            existing.map_err(|error| self.sqlite_failure("audit attempt lookup", &error))?;
        let same_payload = |stored_payload: &str| {
            if stored_payload == row.payload_sha256 {
                Ok(())
            } else {
                Err(failure(
                    ErrorCode::Conflict,
                    "request_id already recorded with a different payload",
                ))
            }
        };
        if let Some(existing) = existing {
            return same_payload(&existing.payload_sha256);
        }
        self.admit(WriteClass::NewRecord, EVENT_WRITE_ESTIMATE_BYTES)?;
        let deadline = self.deadline();
        let recorded = immediate_transaction(self.connection()?, deadline, |transaction| {
            if let Some(existing) = find_attempt(transaction, &row.request_id)? {
                return Err(StepError::Abort(existing.payload_sha256));
            }
            insert_attempt(transaction, &row)?;
            Ok(())
        });
        match recorded {
            Ok(()) => {
                self.last_issue = None;
                Ok(())
            }
            Err(TxError::Abort(stored_payload)) => same_payload(&stored_payload),
            Err(error) => Err(self.tx_failure("audit attempt", error, |_| {
                failure(ErrorCode::Conflict, "attempt already recorded")
            })),
        }
    }

    fn recover_unknown(
        &mut self,
        now_boot_ns: u64,
        now_utc: &str,
    ) -> Result<Vec<RecoveredIntent>, AuditFailure> {
        if !is_rfc3339_utc(now_utc) {
            return Err(invalid("now_utc must be RFC 3339 UTC"));
        }
        let boot_id = self.clock.boot_id();
        let deadline = self.deadline();
        let mut reported = Vec::new();
        let mut unreadable = 0u64;
        loop {
            self.admit(
                WriteClass::ReservedOutcome,
                EVENT_WRITE_ESTIMATE_BYTES.saturating_mul(4),
            )?;
            let batch = immediate_transaction(self.connection()?, deadline, |transaction| {
                let rows = unresolved_intents(transaction, RECOVERY_BATCH)?;
                for row in &rows {
                    let event = EventRow::recovery(
                        &row.request_id,
                        &row.status,
                        now_boot_ns,
                        now_utc,
                        boot_id.as_str(),
                    )
                    .map_err(StepError::Abort)?;
                    insert_event(transaction, &event)?;
                    update_status(transaction, &row.request_id, event.next_status)?;
                }
                Ok(rows)
            });
            let rows =
                batch.map_err(|error| self.tx_failure("audit recovery", error, |abort| abort))?;
            let finished = i64::try_from(rows.len()).unwrap_or(i64::MAX) < RECOVERY_BATCH;
            for row in &rows {
                match recovered_intent(row) {
                    Some(recovered) if reported.len() < MAX_RECOVERED_REPORTED => {
                        reported.push(recovered);
                    }
                    Some(_) => {}
                    None => unreadable = unreadable.saturating_add(1),
                }
            }
            if finished {
                break;
            }
        }
        if unreadable > 0 {
            self.last_issue = Some(format!(
                "{unreadable} recovered intents had unreadable targets (marked unknown)"
            ));
        }
        Ok(reported)
    }

    fn expire(&mut self, before_utc: &str) -> Result<AuditExpiryReport, AuditFailure> {
        if !is_rfc3339_utc(before_utc) {
            return Err(invalid("before_utc must be RFC 3339 UTC"));
        }
        let deadline = self.deadline();
        let result = delete_expired(self.connection()?, before_utc, deadline);
        match result {
            Ok(counts) => Ok(AuditExpiryReport {
                deleted_events: counts.events,
                deleted_attempts: counts.attempts,
                deleted_intents: counts.intents,
                complete: counts.complete,
            }),
            Err(error) => {
                let failure = self.tx_failure("audit expiry", error, |()| {
                    failure(ErrorCode::InternalError, "expiry aborted")
                });
                self.last_issue = Some(format!("expiry failed: {}", failure.message));
                Err(failure)
            }
        }
    }

    fn lookup(&mut self, request_id: Uuid) -> Result<Option<ActionRecordSummary>, AuditFailure> {
        let request = uuid_text(request_id);
        let found = lookup_summary(self.connection()?, &request, request_id);
        found.map_err(|error| self.sqlite_failure("audit lookup", &error))
    }

    fn status(&mut self) -> PersistenceStatus {
        let (state, detail) = self.state_and_detail();
        let footprint = StorageFootprint::measure(&self.path).unwrap_or_default();
        PersistenceStatus {
            kind: PersistenceKind::Audit,
            state,
            detail,
            bytes_used: footprint.total(),
            bytes_limit: self.limits.limit_bytes,
            wal_bytes: footprint.wal_bytes,
        }
    }
}

/// Result code when no event was found since the intent's creation: an
/// intent still waiting for its first outcome reports `Ok`; otherwise the
/// wall clock stepped back and the unbounded scan is used.
fn latest_result(
    connection: &Connection,
    request: &str,
    intent: &StoredIntent,
) -> rusqlite::Result<String> {
    if let Some(code) = latest_result_since(connection, request, &intent.created_utc)? {
        return Ok(code);
    }
    if intent.status == ActionState::IntentCommitted.name() {
        return Ok("Ok".to_owned());
    }
    Ok(latest_result_any(connection, request)?.unwrap_or_else(|| "Ok".to_owned()))
}

fn lookup_summary(
    connection: &Connection,
    request: &str,
    request_id: Uuid,
) -> rusqlite::Result<Option<ActionRecordSummary>> {
    if let Some(intent) = find_intent(connection, request)? {
        let result_code = latest_result(connection, request, &intent)?;
        return Ok(Some(ActionRecordSummary {
            request_id,
            intent_id: Uuid::parse_str(&intent.intent_id).ok(),
            payload_sha256: intent.payload_sha256,
            state: state_from_status(&intent.status).unwrap_or(ActionState::Unknown),
            result_code,
        }));
    }
    Ok(
        find_attempt(connection, request)?.map(|attempt| ActionRecordSummary {
            request_id,
            intent_id: None,
            state: attempt_state(&attempt.result_code),
            payload_sha256: attempt.payload_sha256,
            result_code: attempt.result_code,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::{SqliteGateError, SqliteVersion, StatvfsFreeSpace};
    use dgxtop_core::testing::FakeClock;

    fn failing_gate() -> Result<SqliteVersion, SqliteGateError> {
        Err(SqliteGateError::TooOld {
            found: 3_051_002,
            required: 3_051_003,
        })
    }

    #[test]
    fn version_gate_failure_fails_closed_without_touching_disk() {
        let dir = std::env::temp_dir().join(format!(
            "dgxtop-audit-gate-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.sqlite3");
        let mut repository = SqliteAuditRepository::open_inner(
            &path,
            AuditLimits::default(),
            FakeClock::new(1),
            Arc::new(StatvfsFreeSpace),
            &RealFileSystem,
            failing_gate,
        )
        .unwrap();
        let status = repository.status();
        assert_eq!(status.state, PersistenceState::Failed);
        assert!(status.detail.unwrap().contains("3.51.2"));
        assert!(!path.exists(), "a gated database is never created");
        let error = repository.lookup(Uuid::from_u128(1)).unwrap_err();
        assert_eq!(error.code, ErrorCode::PersistFailed);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn pragma(connection: &Connection, name: &str) -> String {
        connection
            .query_row(&format!("PRAGMA {name}"), [], |row| {
                row.get::<_, rusqlite::types::Value>(0)
            })
            .map(|value| match value {
                rusqlite::types::Value::Integer(number) => number.to_string(),
                rusqlite::types::Value::Text(text) => text,
                other => format!("{other:?}"),
            })
            .unwrap()
    }

    #[test]
    fn writer_connection_is_wal_full_with_manual_checkpoints() {
        let dir = std::env::temp_dir().join(format!(
            "dgxtop-audit-pragmas-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let limits = AuditLimits::default();
        let mut repository = SqliteAuditRepository::open(
            &dir.join("audit.sqlite3"),
            limits,
            FakeClock::new(1),
            Arc::new(StatvfsFreeSpace),
        )
        .unwrap();
        let connection = repository.writer.connection().unwrap();
        assert_eq!(pragma(connection, "journal_mode"), "wal");
        assert_eq!(pragma(connection, "synchronous"), "2");
        assert_eq!(pragma(connection, "cache_size"), "-2048");
        assert_eq!(pragma(connection, "busy_timeout"), "100");
        assert_eq!(pragma(connection, "foreign_keys"), "1");
        assert_eq!(pragma(connection, "wal_autocheckpoint"), "0");
        assert_eq!(
            pragma(connection, "journal_size_limit"),
            (8u64 << 20).to_string()
        );
        let cap = limits.capacity_policy().database_page_cap(4096);
        assert_eq!(pragma(connection, "max_page_count"), cap.to_string());
        assert_eq!(pragma(connection, "user_version"), "2");
        drop(repository);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The audit classifies SQLite failures like every partition and stops
    /// on corruption (sqlite::Writer); it keeps its own codes and its
    /// stricter commit rule.
    #[test]
    fn corruption_stops_the_audit_and_fails_closed() {
        let dir = std::env::temp_dir().join(format!(
            "dgxtop-audit-mapping-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut repository = SqliteAuditRepository::open(
            &dir.join("audit.sqlite3"),
            AuditLimits::default(),
            FakeClock::new(1),
            Arc::new(StatvfsFreeSpace),
        )
        .unwrap();
        let sqlite = |code| rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
        let full = repository.sqlite_failure("audit intent", &sqlite(rusqlite::ffi::SQLITE_FULL));
        assert_eq!(full.code, ErrorCode::CapacityExceeded);
        let busy: TxError<()> = TxError::Commit(sqlite(rusqlite::ffi::SQLITE_BUSY));
        let uncertain = repository.tx_failure("audit intent", busy, |()| unreachable!());
        assert!(uncertain.commit_uncertain, "only a refusal is certain");
        let corrupt =
            repository.sqlite_failure("audit intent", &sqlite(rusqlite::ffi::SQLITE_CORRUPT));
        assert_eq!(corrupt.code, ErrorCode::PersistFailed);
        let status = repository.status();
        assert_eq!(status.state, PersistenceState::Failed);
        assert!(status.detail.unwrap().contains("audit stopped"));
        let refused = repository.lookup(Uuid::from_u128(1)).unwrap_err();
        assert_eq!(refused.code, ErrorCode::PersistFailed);
        drop(repository);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn relative_paths_and_bad_limits_are_argument_errors() {
        let clock = FakeClock::new(1);
        let relative = SqliteAuditRepository::open(
            Path::new("audit.sqlite3"),
            AuditLimits::default(),
            clock.clone(),
            Arc::new(StatvfsFreeSpace),
        );
        assert!(matches!(relative, Err(AuditOpenError::InvalidPath { .. })));
        let limits = AuditLimits {
            busy_deadline_ms: 0,
            ..AuditLimits::default()
        };
        let invalid = SqliteAuditRepository::open(
            Path::new("/tmp/never-created/audit.sqlite3"),
            limits,
            clock,
            Arc::new(StatvfsFreeSpace),
        );
        assert!(matches!(invalid, Err(AuditOpenError::InvalidLimits(_))));
    }
}
