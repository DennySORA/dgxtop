//! SQLite runtime gate (AT31), the verbatim spec DDL and every
//! partition's idempotent initialization.

// Test helpers panic on setup failures; the lints target production code.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use dgxtop_core::action::PersistenceState;
use dgxtop_core::ports::{ArchiveSink, AuditRepository};
use dgxtop_core::testing::FakeClock;
use dgxtop_store::archive::{ArchiveLimits, SqliteArchive};
use dgxtop_store::audit::{AuditLimits, SqliteAuditRepository};
use dgxtop_store::history_tiers::SqliteHistoryTiers;
use dgxtop_store::process_history::ProcessHistoryFiles;
use dgxtop_store::sqlite::{
    AUDIT_DDL, HISTORY_DDL, MIN_SQLITE_VERSION_NUMBER, SchemaKind, SqliteGateError,
    StatvfsFreeSpace, check_version_number, check_versions, ensure_sqlite_version,
};
use rusqlite::Connection;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "dgxtop-sqlite-it-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).unwrap();
        // Like the real state directory: a file is recreated only in a
        // private one (D-85).
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn bundled_sqlite_passes_the_runtime_gate() {
    let version = ensure_sqlite_version().unwrap();
    assert!(version.runtime_number >= MIN_SQLITE_VERSION_NUMBER);
    assert_eq!(version.runtime_number, rusqlite::version_number());
    assert_eq!(version.runtime_number, version.built_number);
    assert_eq!(version.runtime_text, rusqlite::version());
    let source_id = version.source_id.unwrap();
    assert!(source_id.len() > 20, "{source_id}");
}

#[test]
fn older_runtimes_and_mismatched_builds_are_refused() {
    assert_eq!(
        check_version_number(3_051_002),
        Err(SqliteGateError::TooOld {
            found: 3_051_002,
            required: 3_051_003
        })
    );
    assert_eq!(check_version_number(3_051_003), Ok(()));
    assert!(matches!(
        check_versions(3_053_002, 3_051_003),
        Err(SqliteGateError::BuildMismatch { .. })
    ));
}

#[test]
fn ddl_is_the_spec_contract_verbatim() {
    let audit = include_str!("../../../spec/dgxtop-v2/contracts/audit.sql");
    let history = include_str!("../../../spec/dgxtop-v2/contracts/history.sql");
    assert_eq!(AUDIT_DDL, audit);
    assert_eq!(HISTORY_DDL, history);
    assert_eq!(SchemaKind::Audit.ddl(), audit);
    assert_eq!(SchemaKind::History.ddl(), history);
}

/// Every partition gets its DDL and format stamp once; reopening keeps
/// the file as it is.
#[test]
fn every_partition_initializes_once_and_reopens_idempotently() {
    let dir = TempDir::new("schema");
    let audit_path = dir.0.join("audit.sqlite3");
    let history_path = dir.0.join("history.sqlite3");
    let tiers_path = dir.0.join("history_tiers.sqlite3");
    let process_path = dir.0.join("process_history.sqlite3");
    for _ in 0..3 {
        let tiers = SqliteHistoryTiers::open(&tiers_path);
        assert_eq!(tiers.recreated(), None);
        let process = ProcessHistoryFiles::open(&process_path, &dir.0.join("process_recent.json"));
        assert_eq!(process.recreated(), None);
        let mut audit = SqliteAuditRepository::open(
            &audit_path,
            AuditLimits::default(),
            FakeClock::new(1),
            Arc::new(StatvfsFreeSpace),
        )
        .unwrap();
        assert_eq!(audit.status().state, PersistenceState::Healthy);
        let mut archive = SqliteArchive::open(
            &history_path,
            ArchiveLimits::default(),
            FakeClock::new(1),
            Arc::new(StatvfsFreeSpace),
        )
        .unwrap();
        assert_eq!(archive.status().state, PersistenceState::Healthy);
    }
    for (path, kind) in [
        (&audit_path, SchemaKind::Audit),
        (&history_path, SchemaKind::History),
        (&tiers_path, SchemaKind::HistoryTiers),
        (&process_path, SchemaKind::ProcessHistory),
    ] {
        let connection = Connection::open(path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, kind.user_version(), "{}", kind.label());
        let stamp: i32 = connection
            .query_row("PRAGMA application_id", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stamp, kind.format_stamp(), "{}", kind.label());
        for object in kind.objects() {
            let present: i64 = connection
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE name = ?1",
                    [object],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(present, 1, "{object}");
        }
        let mode: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }
}
