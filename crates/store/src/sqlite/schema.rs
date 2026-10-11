//! Spec DDL (verbatim), schema inspection, migration safety (integrity check
//! plus backup) and idempotent initialization (`STORAGE.md` §8).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::fs::FileSystemOps;

/// `contracts/audit.sql`, applied verbatim.
pub const AUDIT_DDL: &str = include_str!("../../../../spec/dgxtop-v2/contracts/audit.sql");
/// `contracts/history.sql`, applied verbatim.
pub const HISTORY_DDL: &str = include_str!("../../../../spec/dgxtop-v2/contracts/history.sql");
/// The process history's DDL (D-76, D-77), dgxtop's own: the spec has none.
pub const PROCESS_HISTORY_DDL: &str = include_str!("../process_history/schema.sql");
/// Brings a version 2 process history to version 3 (D-77).
const PROCESS_HISTORY_UPGRADE_FROM_2: &str = include_str!("../process_history/upgrade_from_2.sql");
/// `PRAGMA user_version` written by the spec's DDL files.
pub const SCHEMA_USER_VERSION: i64 = 2;
/// `PRAGMA user_version` of the process history's DDL.
pub const PROCESS_HISTORY_USER_VERSION: i64 = 3;
/// The Overview History tiers' DDL (D-82), dgxtop's own.
pub const HISTORY_TIERS_DDL: &str = include_str!("../history_tiers/schema.sql");
/// `PRAGMA user_version` of the tiers' DDL.
pub const HISTORY_TIERS_USER_VERSION: i64 = 1;

/// Which SQLite partition a database belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaKind {
    Audit,
    History,
    ProcessHistory,
    HistoryTiers,
}

impl SchemaKind {
    pub const fn ddl(self) -> &'static str {
        match self {
            Self::Audit => AUDIT_DDL,
            Self::History => HISTORY_DDL,
            Self::ProcessHistory => PROCESS_HISTORY_DDL,
            Self::HistoryTiers => HISTORY_TIERS_DDL,
        }
    }

    /// The `user_version` its DDL writes.
    pub const fn user_version(self) -> i64 {
        match self {
            Self::Audit | Self::History => SCHEMA_USER_VERSION,
            Self::ProcessHistory => PROCESS_HISTORY_USER_VERSION,
            Self::HistoryTiers => HISTORY_TIERS_USER_VERSION,
        }
    }

    /// The script that brings `version` to the next version, where one does.
    const fn upgrade_from(self, version: i64) -> Option<&'static str> {
        match (self, version) {
            (Self::ProcessHistory, 2) => Some(PROCESS_HISTORY_UPGRADE_FROM_2),
            _ => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Audit => "audit",
            Self::History => "history",
            Self::ProcessHistory => "process history",
            Self::HistoryTiers => "history tiers",
        }
    }

    /// Tables and indexes the DDL creates.
    pub const fn objects(self) -> &'static [&'static str] {
        match self {
            Self::Audit => &[
                "action_intents",
                "action_events",
                "action_attempts",
                "idx_action_events_time",
                "idx_action_intents_time",
                "idx_action_attempts_time",
            ],
            Self::History => &[
                "history_segments",
                "history_buckets",
                "counter_bridges",
                "history_gaps",
                "idx_history_utc",
                "idx_segments_key",
                "idx_bridges_end",
            ],
            Self::ProcessHistory => &[
                "process_runs",
                "process_buckets",
                "idx_process_runs_seen",
                "idx_process_buckets_time",
            ],
            Self::HistoryTiers => &["tier_series", "tier_buckets", "idx_tier_series_last"],
        }
    }
}

/// What the open sequence must do with an existing database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SchemaPlan {
    /// The kind's `user_version` with every object present.
    Current,
    /// Empty database: apply the DDL directly (nothing to back up).
    Initialize,
    /// Pre-v2 content of our own objects: integrity check, backup, DDL.
    Migrate,
    /// An earlier version of this kind: integrity check, backup, the
    /// upgrade script, then the DDL.
    Upgrade { from: i64 },
}

impl SchemaPlan {
    /// Whether it rewrites existing content (so a backup comes first).
    pub(crate) const fn rewrites(&self) -> bool {
        matches!(self, Self::Migrate | Self::Upgrade { .. })
    }
}

/// Why a database cannot be used for writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SchemaRefusal {
    /// Newer schema: diagnostics only, never rewritten or downgraded.
    Future { found: i64 },
    /// Unknown version or foreign objects: kept untouched.
    Unknown(String),
    /// The current version but objects are missing.
    Incomplete(Vec<String>),
}

pub(crate) fn user_version(connection: &Connection) -> rusqlite::Result<i64> {
    connection.pragma_query_value(None, "user_version", |row| row.get(0))
}

/// Names of every non-internal schema object.
fn schema_objects(connection: &Connection) -> rusqlite::Result<BTreeSet<String>> {
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_schema WHERE substr(name, 1, 7) <> 'sqlite_'")?;
    let names = statement.query_map([], |row| row.get::<_, String>(0))?;
    names.collect()
}

/// Decides how to treat the database given its `user_version` and objects.
pub(crate) fn plan(
    connection: &Connection,
    kind: SchemaKind,
) -> rusqlite::Result<Result<SchemaPlan, SchemaRefusal>> {
    let version = user_version(connection)?;
    let objects = schema_objects(connection)?;
    let expected: BTreeSet<String> = kind
        .objects()
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    Ok(classify(kind, version, &objects, &expected))
}

fn classify(
    kind: SchemaKind,
    version: i64,
    objects: &BTreeSet<String>,
    expected: &BTreeSet<String>,
) -> Result<SchemaPlan, SchemaRefusal> {
    let current = kind.user_version();
    let foreign = || -> Vec<String> { objects.difference(expected).cloned().collect() };
    match version {
        found if found == current => {
            let missing: Vec<String> = expected.difference(objects).cloned().collect();
            if missing.is_empty() {
                Ok(SchemaPlan::Current)
            } else {
                Err(SchemaRefusal::Incomplete(missing))
            }
        }
        found if found > current => Err(SchemaRefusal::Future { found }),
        0 if objects.is_empty() => Ok(SchemaPlan::Initialize),
        0 if foreign().is_empty() => Ok(SchemaPlan::Migrate),
        0 => Err(SchemaRefusal::Unknown(format!(
            "user_version 0 with foreign objects: {}",
            foreign().join(", ")
        ))),
        from if kind.upgrade_from(from).is_some() && foreign().is_empty() => {
            Ok(SchemaPlan::Upgrade { from })
        }
        other => Err(SchemaRefusal::Unknown(format!(
            "user_version {other} has no migration path to {current}"
        ))),
    }
}

/// Runs the spec DDL (it carries its own `BEGIN IMMEDIATE … COMMIT`). On
/// failure the open transaction is rolled back so nothing half-applied stays.
pub(crate) fn apply_ddl(connection: &Connection, kind: SchemaKind) -> rusqlite::Result<()> {
    run_script(connection, kind.ddl())
}

/// Runs the upgrade from `from` (its own transaction, like the DDL); a
/// version without one is left as it is.
pub(crate) fn apply_upgrade(
    connection: &Connection,
    kind: SchemaKind,
    from: i64,
) -> rusqlite::Result<()> {
    kind.upgrade_from(from)
        .map_or(Ok(()), |script| run_script(connection, script))
}

fn run_script(connection: &Connection, script: &str) -> rusqlite::Result<()> {
    let result = connection.execute_batch(script);
    if result.is_err() && !connection.is_autocommit() {
        let _rollback = connection.execute_batch("ROLLBACK");
    }
    result
}

/// `PRAGMA integrity_check(1)` or `quick_check(1)`: `None` when the database
/// is sound, otherwise the first problem reported.
pub(crate) fn integrity_problem(
    connection: &Connection,
    quick: bool,
) -> rusqlite::Result<Option<String>> {
    let pragma = if quick {
        "PRAGMA quick_check(1)"
    } else {
        "PRAGMA integrity_check(1)"
    };
    let verdict: String = connection.query_row(pragma, [], |row| row.get(0))?;
    Ok((verdict != "ok").then(|| dgxtop_core::text::sanitize_message(&verdict)))
}

/// Copies the database into a fresh 0600 file next to it before a migration.
///
/// `VACUUM INTO` writes a consistent snapshot (including WAL content) and
/// accepts a pre-existing empty file, which keeps our 0600 mode; the copy is
/// then fsynced together with its directory.
pub(crate) fn backup_before_migration(
    connection: &Connection,
    (database_path, kind): (&Path, SchemaKind),
    fs: &dyn FileSystemOps,
    utc_label: &str,
) -> Result<PathBuf, String> {
    let directory = database_path
        .parent()
        .ok_or_else(|| "database path has no parent directory".to_owned())?;
    let file_name = database_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "database file name is not UTF-8".to_owned())?;
    let compact_label: String = utc_label
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    let unique: String = uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(8)
        .collect();
    let backup_path = directory.join(format!(
        "{file_name}.pre-v{}-{compact_label}-{unique}.bak",
        kind.user_version()
    ));
    let backup_text = backup_path
        .to_str()
        .ok_or_else(|| "backup path is not UTF-8".to_owned())?
        .to_owned();
    let backup_file = fs
        .create_private_file(&backup_path)
        .map_err(|error| format!("cannot create backup {backup_text}: {error}"))?;
    let copied = connection
        .execute("VACUUM INTO ?1", [backup_text.as_str()])
        .map_err(|error| format!("VACUUM INTO {backup_text} failed: {error}"))
        .and_then(|_| {
            fs.sync_file(&backup_file)
                .map_err(|error| format!("fsync of backup {backup_text} failed: {error}"))
        })
        .and_then(|()| {
            fs.sync_directory(directory)
                .map_err(|error| format!("directory fsync after backup failed: {error}"))
        });
    drop(backup_file);
    match copied {
        Ok(()) => Ok(backup_path),
        Err(message) => {
            let _cleanup = fs.remove_file(&backup_path);
            Err(dgxtop_core::text::sanitize_message(&message))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn expected() -> BTreeSet<String> {
        names(SchemaKind::Audit.objects())
    }

    #[test]
    fn ddl_is_the_spec_text() {
        assert!(AUDIT_DDL.contains("CREATE TABLE IF NOT EXISTS action_intents"));
        assert!(AUDIT_DDL.contains("PRAGMA user_version = 2;"));
        assert!(HISTORY_DDL.contains("CREATE TABLE IF NOT EXISTS counter_bridges"));
        assert!(HISTORY_DDL.contains("BEGIN IMMEDIATE;"));
    }

    #[test]
    fn classification_covers_every_version_case() {
        let full = expected();
        let audit = |version, objects: &BTreeSet<String>| {
            classify(SchemaKind::Audit, version, objects, &full)
        };
        assert_eq!(audit(2, &full), Ok(SchemaPlan::Current));
        assert!(matches!(
            audit(2, &names(&["action_intents"])),
            Err(SchemaRefusal::Incomplete(missing)) if missing.len() == 5
        ));
        assert_eq!(audit(3, &full), Err(SchemaRefusal::Future { found: 3 }));
        assert_eq!(audit(0, &BTreeSet::new()), Ok(SchemaPlan::Initialize));
        assert_eq!(
            audit(0, &names(&["action_intents"])),
            Ok(SchemaPlan::Migrate)
        );
        assert!(matches!(
            audit(0, &names(&["someone_elses_table"])),
            Err(SchemaRefusal::Unknown(_))
        ));
        assert!(matches!(audit(1, &full), Err(SchemaRefusal::Unknown(_))));
        assert!(matches!(audit(-1, &full), Err(SchemaRefusal::Unknown(_))));
    }

    /// The process history is at version 3 and upgrades from 2 (D-77);
    /// the spec's partitions have no upgrade from 2.
    #[test]
    fn the_process_history_upgrades_from_version_2() {
        let full = names(SchemaKind::ProcessHistory.objects());
        let process = |version, objects: &BTreeSet<String>| {
            classify(SchemaKind::ProcessHistory, version, objects, &full)
        };
        assert_eq!(process(3, &full), Ok(SchemaPlan::Current));
        assert_eq!(process(2, &full), Ok(SchemaPlan::Upgrade { from: 2 }));
        assert!(SchemaPlan::Upgrade { from: 2 }.rewrites());
        assert!(!SchemaPlan::Current.rewrites());
        assert_eq!(process(4, &full), Err(SchemaRefusal::Future { found: 4 }));
        assert!(matches!(process(1, &full), Err(SchemaRefusal::Unknown(_))));
        let mut foreign = full.clone();
        foreign.insert("someone_elses_table".to_owned());
        assert!(matches!(
            process(2, &foreign),
            Err(SchemaRefusal::Unknown(_))
        ));
        assert!(PROCESS_HISTORY_DDL.contains("PRAGMA user_version = 3;"));
    }

    /// The tiers are dgxtop's own partition at version 1 (D-82): applied
    /// idempotently, a newer one refused, no upgrade path yet.
    #[test]
    fn the_history_tiers_apply_at_version_1() {
        let connection = Connection::open_in_memory().unwrap();
        apply_ddl(&connection, SchemaKind::HistoryTiers).unwrap();
        apply_ddl(&connection, SchemaKind::HistoryTiers).unwrap();
        assert_eq!(user_version(&connection).unwrap(), 1);
        assert_eq!(
            plan(&connection, SchemaKind::HistoryTiers).unwrap(),
            Ok(SchemaPlan::Current)
        );
        let full = names(SchemaKind::HistoryTiers.objects());
        let tiers = |version| classify(SchemaKind::HistoryTiers, version, &full, &full);
        assert_eq!(tiers(2), Err(SchemaRefusal::Future { found: 2 }));
        assert!(HISTORY_TIERS_DDL.contains("PRAGMA user_version = 1;"));
    }

    #[test]
    fn ddl_applies_idempotently_in_memory() {
        let connection = Connection::open_in_memory().unwrap();
        apply_ddl(&connection, SchemaKind::Audit).unwrap();
        apply_ddl(&connection, SchemaKind::Audit).unwrap();
        assert_eq!(user_version(&connection).unwrap(), 2);
        assert_eq!(
            plan(&connection, SchemaKind::Audit).unwrap(),
            Ok(SchemaPlan::Current)
        );
        let history = Connection::open_in_memory().unwrap();
        apply_ddl(&history, SchemaKind::History).unwrap();
        assert_eq!(
            plan(&history, SchemaKind::History).unwrap(),
            Ok(SchemaPlan::Current)
        );
        assert_eq!(integrity_problem(&history, false).unwrap(), None);
        assert_eq!(integrity_problem(&history, true).unwrap(), None);
    }
}
