//! The shared open sequence of a SQLite partition.
//!
//! Order matters: nothing is written to an existing file before its version,
//! schema and integrity are known, so a future-schema or corrupt database is
//! left byte-for-byte as found.
//!
//! 1. runtime version gate;
//! 2. path safety (regular file, ours, not group/world-writable) or 0600
//!    pre-creation of a missing file;
//! 3. read-only inspection of `user_version` and schema objects;
//! 4. `quick_check` (current schema) or `integrity_check` plus backup
//!    (migration);
//! 5. PRAGMA profile and `max_page_count`;
//! 6. idempotent spec DDL when initializing or migrating.

use std::io;
use std::path::{Path, PathBuf};

use dgxtop_core::action::PersistenceState;
use dgxtop_core::clock::TimeProvider;
use rusqlite::Connection;

use super::capacity::{CapacityPolicy, PageUsage};
use super::connection::{ConnectionProfile, apply_profile, open_connection};
use super::errors::{FaultKind, describe, fault_kind};
use super::schema::{
    SchemaKind, SchemaPlan, SchemaRefusal, apply_ddl, apply_upgrade, backup_before_migration,
    integrity_problem, plan,
};
use super::time::format_utc_rfc3339;
use super::version::{SqliteGateError, SqliteVersion};
use crate::fs::{FileKind, FileSystemOps};

/// Version check used by the open sequence (the real gate in production; a
/// failing stub in tests).
pub(crate) type VersionGate = fn() -> Result<SqliteVersion, SqliteGateError>;

/// What to open and how.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DatabaseSpec<'a> {
    pub path: &'a Path,
    pub kind: SchemaKind,
    pub profile: ConnectionProfile,
    pub policy: CapacityPolicy,
}

/// A database that must not be written; the partition reports this state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unusable {
    pub state: PersistenceState,
    pub reason: String,
}

impl Unusable {
    pub(crate) fn failed(reason: impl AsRef<str>) -> Self {
        Self {
            state: PersistenceState::Failed,
            reason: dgxtop_core::text::sanitize_message(reason.as_ref()),
        }
    }

    fn read_only(reason: impl AsRef<str>) -> Self {
        Self {
            state: PersistenceState::ReadOnlyDiagnostic,
            reason: dgxtop_core::text::sanitize_message(reason.as_ref()),
        }
    }
}

/// A ready writer connection.
#[derive(Debug)]
pub(crate) struct OpenedDatabase {
    pub connection: Connection,
    /// Backup written before a migration, if one ran.
    pub backup: Option<PathBuf>,
}

pub(crate) fn open_database(
    spec: &DatabaseSpec<'_>,
    fs: &dyn FileSystemOps,
    clock: &dyn TimeProvider,
    gate: VersionGate,
) -> Result<OpenedDatabase, Unusable> {
    gate().map_err(|error| Unusable::failed(format!("SQLite version gate: {error}")))?;
    let was_empty = prepare_file(spec.path, fs)?;
    let connection = open_connection(spec.path, spec.profile.busy_timeout)
        .map_err(|error| Unusable::failed(format!("cannot open database: {}", describe(&error))))?;
    let plan = inspect(&connection, spec.kind)?;
    let backup = verify_before_writing(&connection, spec, &plan, was_empty, fs, clock)?;
    configure(&connection, spec)?;
    if let SchemaPlan::Upgrade { from } = plan {
        apply_upgrade(&connection, spec.kind, from)
            .map_err(|error| sqlite_unusable("schema upgrade", &error))?;
    }
    if plan != SchemaPlan::Current {
        initialize_schema(&connection, spec.kind)?;
    }
    Ok(OpenedDatabase { connection, backup })
}

/// Validates an existing file or pre-creates a missing one with mode 0600
/// (SQLite gives `-wal`/`-shm` the main file's mode). Returns whether the
/// file is empty.
fn prepare_file(path: &Path, fs: &dyn FileSystemOps) -> Result<bool, Unusable> {
    match fs.symlink_metadata(path) {
        Ok(facts) => {
            let label = path.display();
            if facts.kind == FileKind::Symlink {
                return Err(Unusable::failed(format!("{label} is a symbolic link")));
            }
            if facts.kind != FileKind::Regular {
                return Err(Unusable::failed(format!("{label} is not a regular file")));
            }
            let euid = fs.effective_uid();
            if facts.owner_uid != euid {
                return Err(Unusable::failed(format!(
                    "{label} is owned by uid {}, expected {euid}",
                    facts.owner_uid
                )));
            }
            if facts.is_group_or_world_writable() {
                return Err(Unusable::failed(format!(
                    "{label} is group/world-writable (mode {:o})",
                    facts.permissions
                )));
            }
            Ok(facts.len == 0)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => match fs.create_private_file(path)
        {
            Ok(_created) => Ok(true),
            Err(error) => Err(Unusable::failed(format!(
                "cannot create {}: {error}",
                path.display()
            ))),
        },
        Err(error) => Err(Unusable::failed(format!(
            "cannot inspect {}: {error}",
            path.display()
        ))),
    }
}

fn sqlite_unusable(context: &str, error: &rusqlite::Error) -> Unusable {
    match fault_kind(error) {
        FaultKind::Corrupt => Unusable::failed(format!(
            "{context}: database is corrupt or not a database (file kept): {}",
            describe(error)
        )),
        FaultKind::Busy => {
            Unusable::failed(format!("{context}: database is locked by another writer"))
        }
        _ => Unusable::failed(format!("{context}: {}", describe(error))),
    }
}

/// Read-only look at `user_version` and the schema objects.
fn inspect(connection: &Connection, kind: SchemaKind) -> Result<SchemaPlan, Unusable> {
    match plan(connection, kind) {
        Err(error) => Err(sqlite_unusable("schema inspection", &error)),
        Ok(Ok(plan)) => Ok(plan),
        Ok(Err(SchemaRefusal::Future { found })) => Err(Unusable::read_only(format!(
            "{} schema user_version {found} is newer than the supported {}; \
             kept for read-only diagnostics and never rewritten",
            kind.label(),
            kind.user_version()
        ))),
        Ok(Err(SchemaRefusal::Unknown(detail))) => Err(Unusable::failed(format!(
            "unknown {} schema ({detail}); file kept",
            kind.label()
        ))),
        Ok(Err(SchemaRefusal::Incomplete(missing))) => Err(Unusable::failed(format!(
            "{} schema is incomplete (missing {}); file kept",
            kind.label(),
            missing.join(", ")
        ))),
    }
}

/// Integrity verification before the first write; backup before migration.
fn verify_before_writing(
    connection: &Connection,
    spec: &DatabaseSpec<'_>,
    plan: &SchemaPlan,
    was_empty: bool,
    fs: &dyn FileSystemOps,
    clock: &dyn TimeProvider,
) -> Result<Option<PathBuf>, Unusable> {
    if was_empty || *plan == SchemaPlan::Initialize {
        return Ok(None);
    }
    let quick = *plan == SchemaPlan::Current;
    match integrity_problem(connection, quick) {
        Err(error) => return Err(sqlite_unusable("integrity check", &error)),
        Ok(Some(problem)) => {
            return Err(Unusable::failed(format!(
                "integrity check failed (file kept, partition stopped): {problem}"
            )));
        }
        Ok(None) => {}
    }
    if !plan.rewrites() {
        return Ok(None);
    }
    let label = format_utc_rfc3339(clock.now_utc_ns());
    backup_before_migration(connection, (spec.path, spec.kind), fs, &label)
        .map(Some)
        .map_err(|message| {
            Unusable::failed(format!(
                "backup before migration failed, not migrated: {message}"
            ))
        })
}

fn configure(connection: &Connection, spec: &DatabaseSpec<'_>) -> Result<(), Unusable> {
    let page_size = PageUsage::read(connection)
        .map_err(|error| sqlite_unusable("page accounting", &error))?
        .page_size;
    let page_cap = spec.policy.database_page_cap(page_size);
    apply_profile(connection, &spec.profile, page_cap)
        .map(|_effective| ())
        .map_err(|error| Unusable::failed(format!("cannot configure database: {error}")))
}

fn initialize_schema(connection: &Connection, kind: SchemaKind) -> Result<(), Unusable> {
    apply_ddl(connection, kind)
        .map_err(|error| sqlite_unusable("schema initialization", &error))?;
    match inspect(connection, kind)? {
        SchemaPlan::Current => Ok(()),
        other => Err(Unusable::failed(format!(
            "schema verification after initialization found {other:?}"
        ))),
    }
}
