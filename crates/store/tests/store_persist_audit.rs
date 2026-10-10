//! Action audit: durable receipts only after COMMIT, idempotency, recovery
//! of intents without outcome (AT32 audit side), fail-closed capacity, WAL
//! and busy handling (AT31), retention, schema/version handling and
//! corruption (STORAGE.md §6–§8).

// Test helpers panic on setup failures; the lints target production code.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dgxtop_core::action::{
    ActionAttempt, ActionIntent, ActionOutcomeRecord, ActionState, PersistenceKind,
    PersistenceState, Signal,
};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::errors::{ErrorCode, InfraError};
use dgxtop_core::identity::{BootId, HostId, Pid, PidNamespace, ProcessKey};
use dgxtop_core::ports::AuditRepository;
use dgxtop_core::testing::FakeClock;
use dgxtop_core::wire::parse_fixed_width;
use dgxtop_store::audit::{AuditLimits, SqliteAuditRepository};
use dgxtop_store::fs::{Failpoint, FailpointFileSystem, Trigger};
use dgxtop_store::sqlite::{
    AUDIT_DDL, FreeSpaceProbe, StorageFootprint, format_utc_rfc3339, shm_path, wal_path,
};
use rusqlite::Connection;
use uuid::Uuid;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;
const DAY_NS: i128 = 86_400 * 1_000_000_000;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "dgxtop-audit-it-{label}-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }

    fn database(&self) -> PathBuf {
        self.0.join("audit.sqlite3")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug)]
struct FakeFreeSpace(AtomicU64);

impl FakeFreeSpace {
    fn plenty() -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(100 * 1024 * MIB)))
    }

    fn set(&self, bytes: u64) {
        self.0.store(bytes, Ordering::SeqCst);
    }
}

impl FreeSpaceProbe for FakeFreeSpace {
    fn available_bytes(&self, _path: &Path) -> Result<u64, InfraError> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

struct Fixture {
    dir: TempDir,
    clock: Arc<FakeClock>,
    free: Arc<FakeFreeSpace>,
}

impl Fixture {
    fn new(label: &str) -> Self {
        Self {
            dir: TempDir::new(label),
            clock: FakeClock::new(5_000_000_000),
            free: FakeFreeSpace::plenty(),
        }
    }

    fn open(&self, limits: AuditLimits) -> SqliteAuditRepository {
        SqliteAuditRepository::open(
            &self.dir.database(),
            limits,
            self.clock.clone(),
            self.free.clone(),
        )
        .unwrap()
    }

    fn now_utc(&self) -> String {
        format_utc_rfc3339(self.clock.now_utc_ns())
    }

    fn utc_days_ago(&self, days: i128) -> String {
        format_utc_rfc3339(self.clock.now_utc_ns() - days * DAY_NS)
    }

    fn intent(&self, request: u128) -> ActionIntent {
        ActionIntent {
            request_id: Uuid::from_u128(request),
            intent_id: Uuid::from_u128(request + 1_000_000),
            session_digest: "session-digest-1".to_owned(),
            process_key: process_key(4242),
            uid: 1000,
            basename: "/opt/conda/bin/python3".to_owned(),
            signal: Signal::Sigterm,
            policy_revision: 1,
            payload_sha256: format!("{request:064x}"),
            created_boot_ns: self.clock.now_boot_ns(),
            created_utc: self.now_utc(),
        }
    }

    fn outcome(
        &self,
        intent: &ActionIntent,
        event: u128,
        state: ActionState,
    ) -> ActionOutcomeRecord {
        ActionOutcomeRecord {
            event_id: Uuid::from_u128(event),
            request_id: intent.request_id,
            intent_id: intent.intent_id,
            state,
            result_code: "Ok".to_owned(),
            detail: format!("{} recorded", state.name()),
            occurred_boot_ns: self.clock.now_boot_ns(),
            occurred_utc: self.now_utc(),
        }
    }

    fn reader(&self) -> Connection {
        Connection::open_with_flags(
            self.dir.database(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap()
    }
}

fn process_key(pid: i64) -> ProcessKey {
    ProcessKey {
        host_id: HostId::new("test-host").unwrap(),
        boot_id: BootId::new("fake-boot").unwrap(),
        pid_namespace: PidNamespace::new("pid:[4026531836]").unwrap(),
        pid: Pid::new(pid).unwrap(),
        start_time_ticks: 90_100,
    }
}

fn small_limits() -> AuditLimits {
    AuditLimits {
        limit_bytes: 512 * KIB,
        wal_checkpoint_bytes: 32 * KIB,
        wal_stop_bytes: 64 * KIB,
        outcome_reserve_bytes: 64 * KIB,
        busy_deadline_ms: 200,
        ..AuditLimits::default()
    }
}

fn count(connection: &Connection, table: &str) -> i64 {
    connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn receipt_exists_only_after_a_committed_full_transaction() {
    let fixture = Fixture::new("receipt");
    let mut audit = fixture.open(AuditLimits::default());
    let intent = fixture.intent(1);
    let receipt = audit.append_intent(&intent).unwrap();
    assert_eq!(receipt.request_id, intent.request_id);
    assert_eq!(receipt.intent_id, intent.intent_id);
    assert_eq!(receipt.committed_boot_ns, fixture.clock.now_boot_ns());
    // Visible to an independent connection: the transaction is committed.
    let reader = fixture.reader();
    let (status, boot_ns, target): (String, String, String) = reader
        .query_row(
            "SELECT status, created_boot_ns, process_key_json FROM action_intents WHERE request_id = ?1",
            [intent.request_id.hyphenated().to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(status, "intent_committed");
    assert_eq!(boot_ns.len(), 20);
    assert_eq!(parse_fixed_width(&boot_ns).unwrap(), intent.created_boot_ns);
    // Privacy: identity, uid and sanitized basename only.
    let value: serde_json::Value = serde_json::from_str(&target).unwrap();
    let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, vec!["basename", "key", "uid"]);
    assert_eq!(value["basename"], "python3");
    assert_eq!(value["uid"], 1000);
    assert!(!target.contains("/opt/conda"));
}

#[test]
fn no_receipt_and_no_row_when_the_writer_is_busy_past_the_deadline() {
    let fixture = Fixture::new("busy");
    let limits = AuditLimits {
        busy_deadline_ms: 150,
        ..AuditLimits::default()
    };
    let mut audit = fixture.open(limits);
    let holder = Connection::open(fixture.dir.database()).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let error = audit.append_intent(&fixture.intent(2)).unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert!(error.message.contains("busy"), "{}", error.message);
    assert!(!error.commit_uncertain);
    holder.execute_batch("ROLLBACK").unwrap();
    assert_eq!(count(&fixture.reader(), "action_intents"), 0);
    assert!(audit.append_intent(&fixture.intent(2)).is_ok());
}

#[test]
fn duplicate_request_with_identical_payload_returns_the_existing_receipt() {
    let fixture = Fixture::new("duplicate");
    let mut audit = fixture.open(AuditLimits::default());
    let intent = fixture.intent(3);
    let first = audit.append_intent(&intent).unwrap();
    fixture.clock.advance_ms(10);
    let second = audit.append_intent(&intent).unwrap();
    assert_eq!(second.request_id, first.request_id);
    assert_eq!(second.intent_id, first.intent_id);
    assert_eq!(second.committed_boot_ns, intent.created_boot_ns);
    assert_eq!(count(&fixture.reader(), "action_intents"), 1);
}

#[test]
fn different_payload_or_progressed_intent_conflicts() {
    let fixture = Fixture::new("conflict");
    let mut audit = fixture.open(AuditLimits::default());
    let intent = fixture.intent(4);
    audit.append_intent(&intent).unwrap();
    let mut other_payload = intent.clone();
    other_payload.payload_sha256 = "f".repeat(64);
    assert_eq!(
        audit.append_intent(&other_payload).unwrap_err().code,
        ErrorCode::Conflict
    );
    let mut other_target = intent.clone();
    other_target.process_key = process_key(7);
    assert_eq!(
        audit.append_intent(&other_target).unwrap_err().code,
        ErrorCode::Conflict
    );
    let mut other_signal = intent.clone();
    other_signal.signal = Signal::Sigkill;
    assert_eq!(
        audit.append_intent(&other_signal).unwrap_err().code,
        ErrorCode::Conflict
    );
    // Once the intent is being sent, even the identical payload gets no
    // receipt: a receipt can never authorize a second signal.
    audit
        .append_outcome(&fixture.outcome(&intent, 40, ActionState::Sending))
        .unwrap();
    let progressed = audit.append_intent(&intent).unwrap_err();
    assert_eq!(progressed.code, ErrorCode::Conflict);
    assert!(progressed.message.contains("sending"));
    // Another request reusing the intent id is a conflict too.
    let mut reused_intent_id = fixture.intent(5);
    reused_intent_id.intent_id = intent.intent_id;
    assert_eq!(
        audit.append_intent(&reused_intent_id).unwrap_err().code,
        ErrorCode::Conflict
    );
}

#[test]
fn recover_unknown_marks_unresolved_intents_and_never_returns_them_as_sendable() {
    let fixture = Fixture::new("recover");
    let mut audit = fixture.open(AuditLimits::default());
    let committed = fixture.intent(10);
    let sending = fixture.intent(11);
    let sent = fixture.intent(12);
    let cancelled = fixture.intent(13);
    for intent in [&committed, &sending, &sent, &cancelled] {
        audit.append_intent(intent).unwrap();
    }
    audit
        .append_outcome(&fixture.outcome(&sending, 1, ActionState::Sending))
        .unwrap();
    audit
        .append_outcome(&fixture.outcome(&sent, 2, ActionState::Sending))
        .unwrap();
    audit
        .append_outcome(&fixture.outcome(&sent, 3, ActionState::SignalSent))
        .unwrap();
    audit
        .append_outcome(&fixture.outcome(&cancelled, 4, ActionState::Cancelled))
        .unwrap();
    // Crash and restart: a new process (and boot clock reading) recovers.
    drop(audit);
    fixture.clock.advance_ms(1000);
    let mut audit = fixture.open(AuditLimits::default());
    let recovered = audit
        .recover_unknown(fixture.clock.now_boot_ns(), &fixture.now_utc())
        .unwrap();
    let mut recovered_ids: Vec<(Uuid, String)> = recovered
        .iter()
        .map(|intent| (intent.request_id, intent.previous_status.clone()))
        .collect();
    recovered_ids.sort();
    assert_eq!(
        recovered_ids,
        vec![
            (committed.request_id, "intent_committed".to_owned()),
            (sending.request_id, "sending".to_owned()),
        ]
    );
    assert!(
        recovered
            .iter()
            .all(|intent| intent.process_key == process_key(4242))
    );
    assert!(
        recovered
            .iter()
            .all(|intent| intent.signal == Signal::Sigterm)
    );
    for intent in [&committed, &sending] {
        let summary = audit.lookup(intent.request_id).unwrap().unwrap();
        assert_eq!(summary.state, ActionState::Unknown);
        assert_eq!(summary.result_code, "UnknownOutcome");
        // Never re-issued: no receipt for the same request, ever again.
        assert_eq!(
            audit.append_intent(intent).unwrap_err().code,
            ErrorCode::Conflict
        );
    }
    assert_eq!(
        audit.lookup(sent.request_id).unwrap().unwrap().state,
        ActionState::SignalSent
    );
    assert_eq!(
        audit.lookup(cancelled.request_id).unwrap().unwrap().state,
        ActionState::Cancelled
    );
    assert!(
        audit
            .recover_unknown(fixture.clock.now_boot_ns(), &fixture.now_utc())
            .unwrap()
            .is_empty()
    );
    let unknown_events: i64 = fixture
        .reader()
        .query_row(
            "SELECT count(*) FROM action_events WHERE event_kind = 'unknown'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unknown_events, 2);
    assert!(audit.recover_unknown(1, "not-a-time").is_err());
}

#[test]
fn outcomes_are_idempotent_by_event_id_and_only_move_status_forward() {
    let fixture = Fixture::new("outcomes");
    let mut audit = fixture.open(AuditLimits::default());
    let intent = fixture.intent(20);
    audit.append_intent(&intent).unwrap();
    let sending = fixture.outcome(&intent, 100, ActionState::Sending);
    audit.append_outcome(&sending).unwrap();
    audit.append_outcome(&sending).unwrap();
    let mut changed = sending.clone();
    changed.detail = "something else".to_owned();
    assert_eq!(
        audit.append_outcome(&changed).unwrap_err().code,
        ErrorCode::Conflict
    );
    // Past the point of no return a cancellation cannot rewrite history.
    audit
        .append_outcome(&fixture.outcome(&intent, 101, ActionState::Cancelled))
        .unwrap();
    assert_eq!(
        audit.lookup(intent.request_id).unwrap().unwrap().state,
        ActionState::Sending
    );
    audit
        .append_outcome(&fixture.outcome(&intent, 102, ActionState::SignalSent))
        .unwrap();
    let mut exited = fixture.outcome(&intent, 103, ActionState::ExitObserved);
    exited.result_code = "Ok".to_owned();
    audit.append_outcome(&exited).unwrap();
    let summary = audit.lookup(intent.request_id).unwrap().unwrap();
    assert_eq!(summary.state, ActionState::ExitObserved);
    assert_eq!(summary.intent_id, Some(intent.intent_id));
    assert_eq!(count(&fixture.reader(), "action_events"), 4);
    let mut wrong_intent = fixture.outcome(&intent, 104, ActionState::StillRunning);
    wrong_intent.intent_id = Uuid::from_u128(999);
    assert_eq!(
        audit.append_outcome(&wrong_intent).unwrap_err().code,
        ErrorCode::Conflict
    );
    let orphan = fixture.outcome(&fixture.intent(21), 105, ActionState::Sending);
    assert_eq!(
        audit.append_outcome(&orphan).unwrap_err().code,
        ErrorCode::ValidationFailed
    );
    let mut premature = fixture.outcome(&intent, 106, ActionState::Confirming);
    premature.state = ActionState::Confirming;
    assert_eq!(
        audit.append_outcome(&premature).unwrap_err().code,
        ErrorCode::ValidationFailed
    );
}

#[test]
fn attempts_are_idempotent_by_request_id() {
    let fixture = Fixture::new("attempts");
    let mut audit = fixture.open(AuditLimits::default());
    let attempt = ActionAttempt {
        request_id: Uuid::from_u128(30),
        payload_sha256: "c".repeat(64),
        session_digest: "session".to_owned(),
        result_code: "CapabilityDenied".to_owned(),
        detail: "target belongs to another user".to_owned(),
        occurred_utc: fixture.now_utc(),
    };
    audit.append_attempt(&attempt).unwrap();
    audit.append_attempt(&attempt).unwrap();
    let mut different = attempt.clone();
    different.payload_sha256 = "d".repeat(64);
    assert_eq!(
        audit.append_attempt(&different).unwrap_err().code,
        ErrorCode::Conflict
    );
    let summary = audit.lookup(attempt.request_id).unwrap().unwrap();
    assert_eq!(summary.intent_id, None);
    assert_eq!(summary.state, ActionState::Rejected);
    assert_eq!(summary.result_code, "CapabilityDenied");
    assert_eq!(count(&fixture.reader(), "action_attempts"), 1);
    assert_eq!(audit.lookup(Uuid::from_u128(31)).unwrap(), None);
}

#[test]
fn low_free_disk_refuses_new_intents_but_records_outcomes() {
    let fixture = Fixture::new("lowdisk");
    let mut audit = fixture.open(AuditLimits::default());
    let intent = fixture.intent(40);
    audit.append_intent(&intent).unwrap();
    fixture.free.set(10 * MIB);
    let refused = audit.append_intent(&fixture.intent(41)).unwrap_err();
    assert_eq!(refused.code, ErrorCode::CapacityExceeded);
    assert!(refused.message.contains("low-water"), "{}", refused.message);
    let status = audit.status();
    assert_eq!(status.state, PersistenceState::Failed);
    assert!(!status.accepts_writes());
    audit
        .append_outcome(&fixture.outcome(&intent, 1, ActionState::Sending))
        .unwrap();
    fixture.free.set(100 * 1024 * MIB);
    audit.append_intent(&fixture.intent(41)).unwrap();
    assert!(audit.status().accepts_writes());
}

#[test]
fn size_cap_refuses_intents_but_still_records_outcomes() {
    let fixture = Fixture::new("cap");
    let limits = small_limits();
    let mut audit = fixture.open(limits);
    let mut committed = Vec::new();
    let refusal = loop {
        let intent = fixture.intent(1000 + u128::try_from(committed.len()).unwrap());
        match audit.append_intent(&intent) {
            Ok(_) => committed.push(intent),
            Err(error) => break error,
        }
        assert!(committed.len() < 10_000, "the cap never triggered");
    };
    assert_eq!(refusal.code, ErrorCode::CapacityExceeded);
    assert!(committed.len() > 10, "only {} intents fit", committed.len());
    let footprint = StorageFootprint::measure(&fixture.dir.database()).unwrap();
    assert!(footprint.total() <= limits.limit_bytes, "{footprint:?}");
    let status = audit.status();
    assert_eq!(status.state, PersistenceState::Failed);
    assert_eq!(status.bytes_limit, limits.limit_bytes);
    // Outcomes of committed intents use the reserve.
    for (index, intent) in committed.iter().take(8).enumerate() {
        let event = 50_000 + u128::try_from(index).unwrap();
        audit
            .append_outcome(&fixture.outcome(intent, event, ActionState::Sending))
            .unwrap();
    }
    // Retention never deletes unexpired rows to make room.
    let report = audit.run_retention().unwrap();
    assert_eq!(
        report.deleted_intents + report.deleted_events + report.deleted_attempts,
        0
    );
    assert_eq!(
        audit
            .append_intent(&fixture.intent(99_999))
            .unwrap_err()
            .code,
        ErrorCode::CapacityExceeded
    );
}

#[test]
fn wal_backlog_pinned_by_a_reader_stops_new_intents_until_released() {
    let fixture = Fixture::new("wal");
    let limits = AuditLimits {
        limit_bytes: 8 * MIB,
        wal_checkpoint_bytes: 16 * KIB,
        wal_stop_bytes: 48 * KIB,
        outcome_reserve_bytes: 64 * KIB,
        ..AuditLimits::default()
    };
    let mut audit = fixture.open(limits);
    let first = fixture.intent(60);
    audit.append_intent(&first).unwrap();
    let reader = fixture.reader();
    reader.execute_batch("BEGIN").unwrap();
    let pinned: i64 = reader
        .query_row("SELECT count(*) FROM action_intents", [], |row| row.get(0))
        .unwrap();
    assert_eq!(pinned, 1);
    let mut accepted = 0u128;
    let refusal = loop {
        match audit.append_intent(&fixture.intent(61 + accepted)) {
            Ok(_) => accepted += 1,
            Err(error) => break error,
        }
        assert!(accepted < 1000, "the WAL stop never triggered");
    };
    assert_eq!(refusal.code, ErrorCode::CapacityExceeded);
    assert!(
        refusal.message.contains("WAL backlog"),
        "{}",
        refusal.message
    );
    // Outcomes are still recorded while new intents are stopped.
    audit
        .append_outcome(&fixture.outcome(&first, 1, ActionState::Sending))
        .unwrap();
    assert_eq!(audit.status().state, PersistenceState::Failed);
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    audit.append_intent(&fixture.intent(5000)).unwrap();
    assert!(audit.status().accepts_writes());
}

#[test]
fn retention_deletes_only_rows_older_than_the_retention_window() {
    let fixture = Fixture::new("retention");
    let mut audit = fixture.open(AuditLimits::default());
    let mut old = fixture.intent(70);
    old.created_utc = fixture.utc_days_ago(40);
    let mut recent = fixture.intent(71);
    recent.created_utc = fixture.utc_days_ago(10);
    let mut old_but_recently_recovered = fixture.intent(72);
    old_but_recently_recovered.created_utc = fixture.utc_days_ago(45);
    for intent in [&old, &recent, &old_but_recently_recovered] {
        audit.append_intent(intent).unwrap();
    }
    let mut old_event = fixture.outcome(&old, 1, ActionState::Sending);
    old_event.occurred_utc = fixture.utc_days_ago(40);
    audit.append_outcome(&old_event).unwrap();
    let mut recent_event = fixture.outcome(&recent, 2, ActionState::Sending);
    recent_event.occurred_utc = fixture.utc_days_ago(10);
    audit.append_outcome(&recent_event).unwrap();
    let mut fresh_event = fixture.outcome(&old_but_recently_recovered, 3, ActionState::Cancelled);
    fresh_event.occurred_utc = fixture.utc_days_ago(1);
    audit.append_outcome(&fresh_event).unwrap();
    let old_attempt = ActionAttempt {
        request_id: Uuid::from_u128(73),
        payload_sha256: "e".repeat(64),
        session_digest: "s".to_owned(),
        result_code: "Cancelled".to_owned(),
        detail: String::new(),
        occurred_utc: fixture.utc_days_ago(31),
    };
    audit.append_attempt(&old_attempt).unwrap();
    let report = audit.run_retention().unwrap();
    assert_eq!(report.deleted_events, 1);
    assert_eq!(report.deleted_intents, 1);
    assert_eq!(report.deleted_attempts, 1);
    assert!(report.complete);
    assert_eq!(report.cutoff_utc, fixture.utc_days_ago(30));
    assert_eq!(audit.lookup(old.request_id).unwrap(), None);
    assert_eq!(audit.lookup(old_attempt.request_id).unwrap(), None);
    assert!(audit.lookup(recent.request_id).unwrap().is_some());
    // An old intent referenced by a recent event lives until it expires.
    assert!(
        audit
            .lookup(old_but_recently_recovered.request_id)
            .unwrap()
            .is_some()
    );
}

#[test]
fn database_files_are_private() {
    let fixture = Fixture::new("modes");
    let mut audit = fixture.open(AuditLimits::default());
    audit.append_intent(&fixture.intent(90)).unwrap();
    for path in [
        fixture.dir.database(),
        wal_path(&fixture.dir.database()),
        shm_path(&fixture.dir.database()),
    ] {
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", path.display());
    }
    let status = audit.status();
    assert_eq!(status.kind, PersistenceKind::Audit);
    let footprint = StorageFootprint::measure(&fixture.dir.database()).unwrap();
    assert_eq!(status.bytes_used, footprint.total());
    assert_eq!(status.wal_bytes, footprint.wal_bytes);
    assert_eq!(status.bytes_limit, 128 * MIB);
}

#[test]
fn future_user_version_is_a_read_only_diagnostic() {
    let fixture = Fixture::new("future");
    {
        let connection = Connection::open(fixture.dir.database()).unwrap();
        connection
            .execute_batch("CREATE TABLE action_intents(x TEXT); PRAGMA user_version = 3;")
            .unwrap();
    }
    std::fs::set_permissions(
        fixture.dir.database(),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let before = std::fs::read(fixture.dir.database()).unwrap();
    let mut audit = fixture.open(AuditLimits::default());
    let status = audit.status();
    assert_eq!(status.state, PersistenceState::ReadOnlyDiagnostic);
    assert!(status.detail.unwrap().contains("user_version 3"));
    let error = audit.append_intent(&fixture.intent(100)).unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert!(audit.recover_unknown(1, &fixture.now_utc()).is_err());
    drop(audit);
    assert_eq!(
        std::fs::read(fixture.dir.database()).unwrap(),
        before,
        "never rewritten"
    );
}

#[test]
fn garbage_file_is_failed_and_preserved() {
    let fixture = Fixture::new("garbage");
    let garbage: Vec<u8> = (0..16_384u32)
        .map(|index| u8::try_from(index % 251).unwrap())
        .collect();
    std::fs::write(fixture.dir.database(), &garbage).unwrap();
    std::fs::set_permissions(
        fixture.dir.database(),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let mut audit = fixture.open(AuditLimits::default());
    let status = audit.status();
    assert_eq!(status.state, PersistenceState::Failed);
    assert!(status.detail.unwrap().contains("file kept"));
    assert_eq!(
        audit.append_intent(&fixture.intent(110)).unwrap_err().code,
        ErrorCode::PersistFailed
    );
    drop(audit);
    assert_eq!(std::fs::read(fixture.dir.database()).unwrap(), garbage);
    assert!(!wal_path(&fixture.dir.database()).exists());
}

#[test]
fn damaged_pages_behind_a_valid_header_are_detected_and_kept() {
    let fixture = Fixture::new("damaged");
    {
        let mut audit = fixture.open(AuditLimits::default());
        for request in 0..200 {
            audit
                .append_intent(&fixture.intent(2000 + request))
                .unwrap();
        }
    }
    // Closing the last connection checkpointed the WAL into the main file.
    let mut bytes = std::fs::read(fixture.dir.database()).unwrap();
    assert!(bytes.len() > 8 * 4096);
    for byte in bytes.iter_mut().skip(4096).take(4 * 4096) {
        *byte = 0xA5;
    }
    std::fs::write(fixture.dir.database(), &bytes).unwrap();
    let mut audit = fixture.open(AuditLimits::default());
    assert_eq!(audit.status().state, PersistenceState::Failed);
    assert!(audit.append_intent(&fixture.intent(3000)).is_err());
    drop(audit);
    assert_eq!(std::fs::read(fixture.dir.database()).unwrap(), bytes);
}

#[test]
fn symlinked_or_shared_database_files_are_refused() {
    let fixture = Fixture::new("unsafe");
    let real = fixture.dir.0.join("real.sqlite3");
    std::fs::write(&real, b"").unwrap();
    std::os::unix::fs::symlink(&real, fixture.dir.database()).unwrap();
    let mut audit = fixture.open(AuditLimits::default());
    assert_eq!(audit.status().state, PersistenceState::Failed);
    drop(audit);
    std::fs::remove_file(fixture.dir.database()).unwrap();
    std::fs::write(fixture.dir.database(), b"").unwrap();
    std::fs::set_permissions(
        fixture.dir.database(),
        std::fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    let mut audit = fixture.open(AuditLimits::default());
    let status = audit.status();
    assert_eq!(status.state, PersistenceState::Failed);
    assert!(status.detail.unwrap().contains("writable"));
}

#[test]
fn migration_of_pre_v2_content_backs_up_first() {
    let fixture = Fixture::new("migrate");
    let partial_ddl = AUDIT_DDL
        .split("CREATE TABLE IF NOT EXISTS action_events")
        .next()
        .unwrap()
        .replace("BEGIN IMMEDIATE;", "")
        .replace("PRAGMA foreign_keys = ON;", "");
    {
        let connection = Connection::open(fixture.dir.database()).unwrap();
        connection.execute_batch(&partial_ddl).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 0);
    }
    std::fs::set_permissions(
        fixture.dir.database(),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let mut audit = fixture.open(AuditLimits::default());
    assert_eq!(audit.status().state, PersistenceState::Healthy);
    let backup = audit.migration_backup().unwrap().to_path_buf();
    assert!(backup.exists());
    assert_eq!(
        std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let copy = Connection::open(&backup).unwrap();
    let tables: i64 = copy
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'action_intents'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 1);
    audit.append_intent(&fixture.intent(120)).unwrap();
    let version: i64 = fixture
        .reader()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 2);
}

#[test]
fn failed_backup_aborts_the_migration_and_keeps_the_file() {
    let fixture = Fixture::new("backup-fails");
    {
        let connection = Connection::open(fixture.dir.database()).unwrap();
        connection
            .execute_batch("CREATE TABLE action_intents(request_id TEXT PRIMARY KEY);")
            .unwrap();
    }
    std::fs::set_permissions(
        fixture.dir.database(),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let before = std::fs::read(fixture.dir.database()).unwrap();
    let fs = FailpointFileSystem::over_real();
    fs.arm(Failpoint::CreateFile, Trigger::Always);
    let mut audit = SqliteAuditRepository::open_with_filesystem(
        &fixture.dir.database(),
        AuditLimits::default(),
        fixture.clock.clone(),
        fixture.free.clone(),
        &fs,
    )
    .unwrap();
    let status = audit.status();
    assert_eq!(status.state, PersistenceState::Failed);
    assert!(status.detail.unwrap().contains("backup"));
    drop(audit);
    assert_eq!(std::fs::read(fixture.dir.database()).unwrap(), before);
}

#[test]
fn unknown_schema_versions_and_foreign_objects_are_kept_untouched() {
    for setup in [
        "CREATE TABLE action_intents(x TEXT); PRAGMA user_version = 1;",
        "CREATE TABLE somebody_elses(x TEXT);",
    ] {
        let fixture = Fixture::new("unknown");
        {
            let connection = Connection::open(fixture.dir.database()).unwrap();
            connection.execute_batch(setup).unwrap();
        }
        std::fs::set_permissions(
            fixture.dir.database(),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let before = std::fs::read(fixture.dir.database()).unwrap();
        let mut audit = fixture.open(AuditLimits::default());
        assert_eq!(audit.status().state, PersistenceState::Failed, "{setup}");
        drop(audit);
        assert_eq!(
            std::fs::read(fixture.dir.database()).unwrap(),
            before,
            "{setup}"
        );
    }
}

#[test]
fn missing_parent_directory_fails_closed() {
    let fixture = Fixture::new("noparent");
    let path = fixture.dir.0.join("absent").join("audit.sqlite3");
    let mut audit = SqliteAuditRepository::open(
        &path,
        AuditLimits::default(),
        fixture.clock.clone(),
        fixture.free.clone(),
    )
    .unwrap();
    assert_eq!(audit.status().state, PersistenceState::Failed);
    assert!(audit.append_intent(&fixture.intent(130)).is_err());
}
