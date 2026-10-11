//! Spec DDL (verbatim), every partition's format version, schema
//! inspection and idempotent initialization.
//!
//! One rule for every database dgxtop writes (D-85): a file is used only
//! when it carries this build's format version, `user_version` and exactly
//! the DDL's objects. Anything else is not migrated but recreated; this
//! module only tells which ([`plan`]).

use std::collections::BTreeSet;

use rusqlite::Connection;

/// `contracts/audit.sql`, applied verbatim.
pub const AUDIT_DDL: &str = include_str!("../../../../spec/dgxtop-v2/contracts/audit.sql");
/// `contracts/history.sql`, applied verbatim.
pub const HISTORY_DDL: &str = include_str!("../../../../spec/dgxtop-v2/contracts/history.sql");
/// The process history's DDL (D-76, D-77), dgxtop's own: the spec has none.
pub(crate) const PROCESS_HISTORY_DDL: &str = include_str!("../process_history/schema.sql");
/// `PRAGMA user_version` written by the spec's DDL files.
const SCHEMA_USER_VERSION: i64 = 2;
/// `PRAGMA user_version` of the process history's DDL.
const PROCESS_HISTORY_USER_VERSION: i64 = 3;
/// The Overview History tiers' DDL (D-82), dgxtop's own.
const HISTORY_TIERS_DDL: &str = include_str!("../history_tiers/schema.sql");
/// `PRAGMA user_version` of the tiers' DDL.
const HISTORY_TIERS_USER_VERSION: i64 = 1;

/// The high half of `PRAGMA application_id` in every database dgxtop
/// writes ("DG"); the low half is the partition's format version (D-85).
const FORMAT_MAGIC: i32 = 0x4447_0000;

/// The `application_id` that marks `format_version` as dgxtop's.
pub fn format_stamp(format_version: u16) -> i32 {
    FORMAT_MAGIC | i32::from(format_version)
}

/// The format version an `application_id` carries; `None` when it is no
/// dgxtop stamp (a file without any stamp reads 0).
fn format_of(application_id: i32) -> Option<u16> {
    if application_id & !0xFFFF == FORMAT_MAGIC {
        u16::try_from(application_id & 0xFFFF).ok()
    } else {
        None
    }
}

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

    /// The version of the file's data format (D-85), kept in
    /// `PRAGMA application_id`, apart from the app version and the spec's
    /// `user_version`. Bump it whenever what the file holds changes meaning
    /// (tables, columns, packing, units): a file of any other format, or of
    /// none, is deleted and created afresh at the next start.
    pub const fn format_version(self) -> u16 {
        match self {
            Self::Audit | Self::History | Self::ProcessHistory => 1,
            // 2: `tier_series.last_utc_ns` and its index dropped (D-95).
            Self::HistoryTiers => 2,
        }
    }

    /// The `application_id` its files carry.
    pub fn format_stamp(self) -> i32 {
        format_stamp(self.format_version())
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
            Self::HistoryTiers => &["tier_series", "tier_buckets"],
        }
    }
}

/// What the open sequence does with a database it may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchemaPlan {
    /// This build's format, `user_version` and objects.
    Current,
    /// Nothing in it yet: apply the DDL and stamp the format.
    Initialize,
}

/// Why a database is not this build's: it is recreated (D-85).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SchemaRefusal {
    /// Another format version, or no dgxtop stamp at all.
    Format { found: Option<u16> },
    /// This format but another `user_version`.
    Version { found: i64 },
    /// Objects missing or not the DDL's.
    Structure(String),
}

impl std::fmt::Display for SchemaRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Format { found: Some(found) } => write!(formatter, "format version {found}"),
            Self::Format { found: None } => formatter.write_str("no dgxtop format version"),
            Self::Version { found } => write!(formatter, "user_version {found}"),
            Self::Structure(detail) => formatter.write_str(detail),
        }
    }
}

/// What the read-only inspection found in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Found {
    application_id: i32,
    user_version: i64,
    objects: BTreeSet<String>,
}

pub(crate) fn user_version(connection: &Connection) -> rusqlite::Result<i64> {
    connection.pragma_query_value(None, "user_version", |row| row.get(0))
}

fn application_id(connection: &Connection) -> rusqlite::Result<i32> {
    connection.pragma_query_value(None, "application_id", |row| row.get(0))
}

/// Names of every non-internal schema object.
fn schema_objects(connection: &Connection) -> rusqlite::Result<BTreeSet<String>> {
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_schema WHERE substr(name, 1, 7) <> 'sqlite_'")?;
    let names = statement.query_map([], |row| row.get::<_, String>(0))?;
    names.collect()
}

/// Whether the database is this build's, empty, or to be recreated.
pub(crate) fn plan(
    connection: &Connection,
    kind: SchemaKind,
) -> rusqlite::Result<Result<SchemaPlan, SchemaRefusal>> {
    let found = Found {
        application_id: application_id(connection)?,
        user_version: user_version(connection)?,
        objects: schema_objects(connection)?,
    };
    Ok(classify(kind, &found))
}

fn classify(kind: SchemaKind, found: &Found) -> Result<SchemaPlan, SchemaRefusal> {
    if found.application_id == 0 && found.user_version == 0 && found.objects.is_empty() {
        return Ok(SchemaPlan::Initialize);
    }
    if found.application_id != kind.format_stamp() {
        return Err(SchemaRefusal::Format {
            found: format_of(found.application_id),
        });
    }
    if found.user_version != kind.user_version() {
        return Err(SchemaRefusal::Version {
            found: found.user_version,
        });
    }
    let expected: BTreeSet<String> = kind
        .objects()
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    let missing: Vec<&str> = expected
        .difference(&found.objects)
        .map(String::as_str)
        .collect();
    let foreign: Vec<&str> = found
        .objects
        .difference(&expected)
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Err(SchemaRefusal::Structure(format!(
            "objects missing: {}",
            missing.join(", ")
        )));
    }
    if !foreign.is_empty() {
        return Err(SchemaRefusal::Structure(format!(
            "objects not of this schema: {}",
            foreign.join(", ")
        )));
    }
    Ok(SchemaPlan::Current)
}

/// Runs the DDL (it carries its own `BEGIN IMMEDIATE … COMMIT`), then
/// stamps the format version: the spec's DDL is applied verbatim, so the
/// stamp is a header write of its own (D-85). On failure the open
/// transaction is rolled back so nothing half-applied stays.
pub(crate) fn apply_ddl(connection: &Connection, kind: SchemaKind) -> rusqlite::Result<()> {
    run_script(connection, kind.ddl())?;
    connection.pragma_update(None, "application_id", kind.format_stamp())
}

fn run_script(connection: &Connection, script: &str) -> rusqlite::Result<()> {
    let result = connection.execute_batch(script);
    if result.is_err() && !connection.is_autocommit() {
        let _rollback = connection.execute_batch("ROLLBACK");
    }
    result
}

/// `PRAGMA quick_check(1)`, the open sequence's check of an existing file:
/// `None` when the database is sound, otherwise the first problem reported
/// (the file is then recreated, D-85).
pub(crate) fn quick_check_problem(connection: &Connection) -> rusqlite::Result<Option<String>> {
    let verdict: String = connection.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
    Ok((verdict != "ok").then(|| dgxtop_core::text::sanitize_message(&verdict)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn current(kind: SchemaKind) -> Found {
        Found {
            application_id: kind.format_stamp(),
            user_version: kind.user_version(),
            objects: names(kind.objects()),
        }
    }

    const KINDS: [SchemaKind; 4] = [
        SchemaKind::Audit,
        SchemaKind::History,
        SchemaKind::ProcessHistory,
        SchemaKind::HistoryTiers,
    ];

    #[test]
    fn ddl_is_the_spec_text() {
        assert!(AUDIT_DDL.contains("CREATE TABLE IF NOT EXISTS action_intents"));
        assert!(AUDIT_DDL.contains("PRAGMA user_version = 2;"));
        assert!(HISTORY_DDL.contains("CREATE TABLE IF NOT EXISTS counter_bridges"));
        assert!(HISTORY_DDL.contains("BEGIN IMMEDIATE;"));
        assert!(PROCESS_HISTORY_DDL.contains("PRAGMA user_version = 3;"));
        assert!(HISTORY_TIERS_DDL.contains("PRAGMA user_version = 1;"));
    }

    /// The stamp keeps dgxtop's magic apart from the format version, and
    /// no other application's id reads as one of ours.
    #[test]
    fn the_format_stamp_round_trips() {
        assert_eq!(format_of(format_stamp(1)), Some(1));
        assert_eq!(format_of(format_stamp(u16::MAX)), Some(u16::MAX));
        assert_eq!(format_of(0), None);
        assert_eq!(format_of(0x0F05_5112), None);
        assert!(format_stamp(1) > 0);
    }

    /// Every partition, the audit included, follows one rule (D-85): the
    /// same format, version and objects are kept, every difference is a
    /// refusal the open sequence answers by recreating the file.
    #[test]
    fn every_partition_keeps_only_its_own_format() {
        for kind in KINDS {
            let label = kind.label();
            assert_eq!(classify(kind, &current(kind)), Ok(SchemaPlan::Current));
            let empty = Found {
                application_id: 0,
                user_version: 0,
                objects: BTreeSet::new(),
            };
            assert_eq!(classify(kind, &empty), Ok(SchemaPlan::Initialize));
            let version = kind.format_version();
            for (application_id, found) in [
                (format_stamp(version + 1), Some(version + 1)),
                (format_stamp(version - 1), Some(version - 1)),
                (0, None),
                (0x0F05_5112, None),
            ] {
                let other = Found {
                    application_id,
                    ..current(kind)
                };
                assert_eq!(
                    classify(kind, &other),
                    Err(SchemaRefusal::Format { found }),
                    "{label}"
                );
            }
            for user_version in [0, kind.user_version() - 1, kind.user_version() + 1] {
                let other = Found {
                    user_version,
                    ..current(kind)
                };
                assert_eq!(
                    classify(kind, &other),
                    Err(SchemaRefusal::Version {
                        found: user_version
                    }),
                    "{label}"
                );
            }
            let mut missing = current(kind);
            missing.objects.pop_first();
            assert!(matches!(
                classify(kind, &missing),
                Err(SchemaRefusal::Structure(detail)) if detail.contains("missing")
            ));
            let mut foreign = current(kind);
            foreign.objects.insert("someone_elses_table".to_owned());
            assert!(matches!(
                classify(kind, &foreign),
                Err(SchemaRefusal::Structure(detail)) if detail.contains("someone_elses_table")
            ));
        }
    }

    #[test]
    fn ddl_applies_idempotently_and_stamps_the_format() {
        for kind in KINDS {
            let connection = Connection::open_in_memory().unwrap();
            apply_ddl(&connection, kind).unwrap();
            apply_ddl(&connection, kind).unwrap();
            assert_eq!(user_version(&connection).unwrap(), kind.user_version());
            assert_eq!(
                application_id(&connection).unwrap(),
                kind.format_stamp(),
                "{}",
                kind.label()
            );
            assert_eq!(plan(&connection, kind).unwrap(), Ok(SchemaPlan::Current));
            assert_eq!(quick_check_problem(&connection).unwrap(), None);
        }
    }
}
