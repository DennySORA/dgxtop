//! The shared open sequence of a SQLite partition.
//!
//! Nothing is written to an existing file before its format, schema and
//! integrity are known. One rule for every partition (D-85): a file that
//! cannot be used as it is (not this build's format version, `user_version`
//! or objects, unreadable, corrupt, failing its integrity check, a symbolic
//! link, another user's or writable by others) is not migrated or kept but
//! deleted with its `-wal`/`-shm` and created afresh, once; the reason is
//! handed to the partition to log. What is not the file's fault (the
//! SQLite version gate, another writer holding the lock, a directory or
//! FIFO in its place, a state directory that is not private) leaves it
//! alone and makes the partition unusable instead.
//!
//! 1. runtime version gate;
//! 2. path safety (regular file, ours, not group/world-writable) or 0600
//!    pre-creation of a missing file;
//! 3. read-only inspection of the format version, `user_version` and
//!    schema objects;
//! 4. `quick_check` of an existing file;
//! 5. PRAGMA profile and `max_page_count`;
//! 6. the DDL and the format stamp when the file is empty.

use std::io;
use std::path::Path;

use rusqlite::Connection;

use super::capacity::{CapacityPolicy, PageUsage, shm_path, wal_path};
use super::connection::{ConnectionProfile, apply_profile, open_connection};
use super::errors::{FaultKind, describe, fault_kind};
use super::schema::{SchemaKind, SchemaPlan, apply_ddl, plan, quick_check_problem};
use super::version::{SqliteGateError, SqliteVersion};
use crate::fs::{FileKind, FileSystemOps};
use crate::paths::verify_private_directory;

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

/// A database that must not be written: the partition reports `Failed`
/// with this reason (D-85 left no read-only state).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unusable {
    pub reason: String,
}

impl Unusable {
    pub(crate) fn failed(reason: impl AsRef<str>) -> Self {
        Self {
            reason: dgxtop_core::text::sanitize_message(reason.as_ref()),
        }
    }
}

/// A ready writer connection.
#[derive(Debug)]
pub(crate) struct OpenedDatabase {
    pub connection: Connection,
    /// Why the file found was deleted and created afresh, if it was.
    pub recreated: Option<String>,
}

/// Why one open attempt failed.
#[derive(Debug)]
enum OpenFailure {
    /// The file itself cannot be used: it is recreated.
    File(String),
    /// Something around the file: it is left alone.
    Surroundings(Unusable),
}

impl OpenFailure {
    fn sqlite(context: &str, error: &rusqlite::Error) -> Self {
        match fault_kind(error) {
            FaultKind::Busy => Self::Surroundings(Unusable::failed(format!(
                "{context}: database is locked by another writer"
            ))),
            _ => Self::File(format!("{context}: {}", describe(error))),
        }
    }
}

/// Why `path` cannot hold a database or a file of the store: it must be
/// absolute (never relative to wherever dgxtop was started) and name a
/// file. `None` when it can.
pub(crate) fn path_problem(path: &Path) -> Option<&'static str> {
    if !path.is_absolute() {
        Some("must be absolute")
    } else if path.file_name().is_none() {
        Some("must name a file")
    } else {
        None
    }
}

pub(crate) fn open_database(
    spec: &DatabaseSpec<'_>,
    fs: &dyn FileSystemOps,
    gate: VersionGate,
) -> Result<OpenedDatabase, Unusable> {
    gate().map_err(|error| Unusable::failed(format!("SQLite version gate: {error}")))?;
    match open_as_found(spec, fs) {
        Ok(connection) => Ok(OpenedDatabase {
            connection,
            recreated: None,
        }),
        Err(OpenFailure::Surroundings(unusable)) => Err(unusable),
        Err(OpenFailure::File(reason)) => recreate(spec, fs, reason),
    }
}

/// Deletes the file and its journals and opens a fresh one; a second
/// failure is final (no loop).
fn recreate(
    spec: &DatabaseSpec<'_>,
    fs: &dyn FileSystemOps,
    reason: String,
) -> Result<OpenedDatabase, Unusable> {
    remove_database_files(spec.path, fs)
        .map_err(|error| Unusable::failed(format!("{reason}; not deleted: {error}")))?;
    match open_as_found(spec, fs) {
        Ok(connection) => Ok(OpenedDatabase {
            connection,
            recreated: Some(dgxtop_core::text::sanitize_message(&reason)),
        }),
        Err(OpenFailure::Surroundings(unusable)) => Err(unusable),
        Err(OpenFailure::File(again)) => Err(Unusable::failed(format!(
            "{again} (after deleting a file with {reason})"
        ))),
    }
}

/// Removes the database, `-wal` and `-shm`, only inside a private
/// directory of ours; a symbolic link is removed as a link, never its
/// target. Every file is checked before the first is removed, so a
/// journal that cannot go never leaves it without its database.
fn remove_database_files(path: &Path, fs: &dyn FileSystemOps) -> Result<(), String> {
    let directory = path
        .parent()
        .ok_or_else(|| "the database has no directory".to_owned())?;
    verify_private_directory(fs, directory).map_err(|error| error.to_string())?;
    let files = [path.to_path_buf(), wal_path(path), shm_path(path)];
    if let Some(blocked) = files.iter().find(|file| {
        fs.symlink_metadata(file)
            .is_ok_and(|facts| !matches!(facts.kind, FileKind::Regular | FileKind::Symlink))
    }) {
        return Err(format!("{} is not a regular file", blocked.display()));
    }
    for file in files {
        match fs.remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot remove {}: {error}", file.display())),
        }
    }
    fs.sync_directory(directory)
        .map_err(|error| format!("directory fsync after removal failed: {error}"))
}

fn open_as_found(
    spec: &DatabaseSpec<'_>,
    fs: &dyn FileSystemOps,
) -> Result<Connection, OpenFailure> {
    let was_empty = prepare_file(spec.path, fs)?;
    let connection = open_connection(spec.path, spec.profile.busy_timeout)
        .map_err(|error| OpenFailure::sqlite("cannot open database", &error))?;
    let plan = inspect(&connection, spec.kind)?;
    if !was_empty && plan == SchemaPlan::Current {
        verify_integrity(&connection)?;
    }
    configure(&connection, spec)?;
    if plan == SchemaPlan::Initialize {
        initialize_schema(&connection, spec.kind)?;
    }
    Ok(connection)
}

/// Validates an existing file or pre-creates a missing one with mode 0600
/// (SQLite gives `-wal`/`-shm` the main file's mode). Returns whether the
/// file is empty.
fn prepare_file(path: &Path, fs: &dyn FileSystemOps) -> Result<bool, OpenFailure> {
    let label = path.display();
    match fs.symlink_metadata(path) {
        Ok(facts) => {
            match facts.kind {
                FileKind::Regular => {}
                FileKind::Symlink => {
                    return Err(OpenFailure::File(format!("{label} is a symbolic link")));
                }
                FileKind::Directory | FileKind::Other => {
                    return Err(OpenFailure::Surroundings(Unusable::failed(format!(
                        "{label} is not a regular file"
                    ))));
                }
            }
            let euid = fs.effective_uid();
            if facts.owner_uid != euid {
                return Err(OpenFailure::File(format!(
                    "{label} is owned by uid {}, expected {euid}",
                    facts.owner_uid
                )));
            }
            if facts.is_group_or_world_writable() {
                return Err(OpenFailure::File(format!(
                    "{label} is group/world-writable (mode {:o})",
                    facts.permissions
                )));
            }
            Ok(facts.len == 0)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => match fs.create_private_file(path)
        {
            Ok(_created) => Ok(true),
            Err(error) => Err(OpenFailure::Surroundings(Unusable::failed(format!(
                "cannot create {label}: {error}"
            )))),
        },
        Err(error) => Err(OpenFailure::Surroundings(Unusable::failed(format!(
            "cannot inspect {label}: {error}"
        )))),
    }
}

/// Read-only look at the format version, `user_version` and objects.
fn inspect(connection: &Connection, kind: SchemaKind) -> Result<SchemaPlan, OpenFailure> {
    match plan(connection, kind) {
        Err(error) => Err(OpenFailure::sqlite("schema inspection", &error)),
        Ok(Ok(plan)) => Ok(plan),
        Ok(Err(refusal)) => Err(OpenFailure::File(format!(
            "{} database of another format ({refusal}; this build writes format {}, \
             user_version {})",
            kind.label(),
            kind.format_version(),
            kind.user_version()
        ))),
    }
}

fn verify_integrity(connection: &Connection) -> Result<(), OpenFailure> {
    match quick_check_problem(connection) {
        Err(error) => Err(OpenFailure::sqlite("integrity check", &error)),
        Ok(Some(problem)) => Err(OpenFailure::File(format!(
            "integrity check failed: {problem}"
        ))),
        Ok(None) => Ok(()),
    }
}

fn configure(connection: &Connection, spec: &DatabaseSpec<'_>) -> Result<(), OpenFailure> {
    let page_size = PageUsage::read(connection)
        .map_err(|error| OpenFailure::sqlite("page accounting", &error))?
        .page_size;
    let page_cap = spec.policy.database_page_cap(page_size);
    apply_profile(connection, &spec.profile, page_cap)
        .map(|_effective| ())
        .map_err(|error| {
            OpenFailure::Surroundings(Unusable::failed(format!(
                "cannot configure database: {error}"
            )))
        })
}

fn initialize_schema(connection: &Connection, kind: SchemaKind) -> Result<(), OpenFailure> {
    apply_ddl(connection, kind)
        .map_err(|error| OpenFailure::sqlite("schema initialization", &error))?;
    match inspect(connection, kind)? {
        SchemaPlan::Current => Ok(()),
        SchemaPlan::Initialize => Err(OpenFailure::Surroundings(Unusable::failed(
            "schema verification after initialization found an empty database",
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;
    use crate::fs::RealFileSystem;

    struct PrivateDir(PathBuf);

    impl PrivateDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "dgxtop-sqlite-database-{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }

    impl Drop for PrivateDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A journal that is no file stops the removal before anything is
    /// deleted: the database is never left without its journals.
    #[test]
    fn nothing_is_removed_when_one_journal_is_not_a_file() {
        let dir = PrivateDir::new("half");
        let path = dir.0.join("db.sqlite3");
        std::fs::write(&path, b"database").unwrap();
        std::fs::write(wal_path(&path), b"wal").unwrap();
        std::fs::create_dir(shm_path(&path)).unwrap();
        let error = remove_database_files(&path, &RealFileSystem).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
        assert!(path.exists(), "the database stays");
        assert!(wal_path(&path).exists(), "the WAL stays");
    }

    /// The database, its journals and a link in its place go; the link's
    /// target stays.
    #[test]
    fn the_database_and_its_journals_are_removed_together() {
        let dir = PrivateDir::new("whole");
        let path = dir.0.join("db.sqlite3");
        let target = dir.0.join("target");
        std::fs::write(&target, b"kept").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        std::fs::write(wal_path(&path), b"wal").unwrap();
        remove_database_files(&path, &RealFileSystem).unwrap();
        assert!(path.symlink_metadata().is_err(), "the link is gone");
        assert!(!wal_path(&path).exists());
        assert!(target.exists(), "a link is removed as a link");
    }
}
