//! Prepared statements of the audit tables. Every value is a bound
//! parameter; nothing user-controlled is ever formatted into SQL.

use rusqlite::{Connection, OptionalExtension, params};

use super::records::{AttemptRow, EventRow, IntentRow, StoredEvent};

const SELECT_INTENT: &str = "SELECT intent_id, payload_sha256, process_key_json, signal, status, \
     boot_id, created_boot_ns, created_utc FROM action_intents WHERE request_id = ?1";
const INSERT_INTENT: &str = "INSERT INTO action_intents (request_id, intent_id, session_digest, \
     process_key_json, signal, policy_revision, created_utc, status, payload_sha256, boot_id, \
     created_boot_ns) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'intent_committed', ?8, ?9, ?10)";
const UPDATE_STATUS: &str = "UPDATE action_intents SET status = ?2 WHERE request_id = ?1";
const SELECT_EVENT: &str = "SELECT request_id, occurred_utc, occurred_boot_ns, event_kind, \
     result_code, detail_json FROM action_events WHERE event_id = ?1";
const INSERT_EVENT: &str = "INSERT INTO action_events (event_id, request_id, occurred_utc, \
     boot_id, occurred_boot_ns, event_kind, result_code, detail_json) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";
// `action_events` has no request_id index (DDL is verbatim); the time index
// bounds the scan to events at or after the intent's creation.
const SELECT_LATEST_RESULT_SINCE: &str = "SELECT result_code FROM action_events \
     WHERE occurred_utc >= ?2 AND request_id = ?1 ORDER BY rowid DESC LIMIT 1";
const SELECT_LATEST_RESULT_ANY: &str = "SELECT result_code FROM action_events \
     WHERE request_id = ?1 ORDER BY rowid DESC LIMIT 1";
const SELECT_ATTEMPT: &str =
    "SELECT payload_sha256, result_code FROM action_attempts WHERE request_id = ?1";
const INSERT_ATTEMPT: &str = "INSERT INTO action_attempts (request_id, payload_sha256, \
     session_digest, occurred_utc, result_code, detail_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
const SELECT_UNRESOLVED: &str = "SELECT request_id, intent_id, process_key_json, signal, status \
     FROM action_intents WHERE status IN ('intent_committed', 'sending') \
     ORDER BY created_utc, request_id LIMIT ?1";

/// An `action_intents` row read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredIntent {
    pub intent_id: String,
    pub payload_sha256: String,
    pub target_json: String,
    pub signal: String,
    pub status: String,
    pub boot_id: String,
    pub created_boot_ns: String,
    pub created_utc: String,
}

/// An intent that has no provable outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnresolvedIntent {
    pub request_id: String,
    pub intent_id: String,
    pub target_json: String,
    pub signal: String,
    pub status: String,
}

/// An `action_attempts` row read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredAttempt {
    pub payload_sha256: String,
    pub result_code: String,
}

pub(crate) fn find_intent(
    connection: &Connection,
    request_id: &str,
) -> rusqlite::Result<Option<StoredIntent>> {
    connection
        .prepare_cached(SELECT_INTENT)?
        .query_row(params![request_id], |row| {
            Ok(StoredIntent {
                intent_id: row.get(0)?,
                payload_sha256: row.get(1)?,
                target_json: row.get(2)?,
                signal: row.get(3)?,
                status: row.get(4)?,
                boot_id: row.get(5)?,
                created_boot_ns: row.get(6)?,
                created_utc: row.get(7)?,
            })
        })
        .optional()
}

pub(crate) fn insert_intent(connection: &Connection, row: &IntentRow) -> rusqlite::Result<()> {
    connection.prepare_cached(INSERT_INTENT)?.execute(params![
        row.request_id,
        row.intent_id,
        row.session_digest,
        row.target_json,
        row.signal,
        row.policy_revision,
        row.created_utc,
        row.payload_sha256,
        row.boot_id,
        row.created_boot_ns,
    ])?;
    Ok(())
}

pub(crate) fn update_status(
    connection: &Connection,
    request_id: &str,
    status: &str,
) -> rusqlite::Result<()> {
    connection
        .prepare_cached(UPDATE_STATUS)?
        .execute(params![request_id, status])?;
    Ok(())
}

pub(crate) fn find_event(
    connection: &Connection,
    event_id: &str,
) -> rusqlite::Result<Option<StoredEvent>> {
    connection
        .prepare_cached(SELECT_EVENT)?
        .query_row(params![event_id], |row| {
            Ok(StoredEvent {
                request_id: row.get(0)?,
                occurred_utc: row.get(1)?,
                occurred_boot_ns: row.get(2)?,
                event_kind: row.get(3)?,
                result_code: row.get(4)?,
                detail_json: row.get(5)?,
            })
        })
        .optional()
}

pub(crate) fn insert_event(connection: &Connection, row: &EventRow) -> rusqlite::Result<()> {
    connection.prepare_cached(INSERT_EVENT)?.execute(params![
        row.event_id,
        row.request_id,
        row.occurred_utc,
        row.boot_id,
        row.occurred_boot_ns,
        row.event_kind,
        row.result_code,
        row.detail_json,
    ])?;
    Ok(())
}

/// Result code of the most recent event recorded since `created_utc`.
pub(crate) fn latest_result_since(
    connection: &Connection,
    request_id: &str,
    created_utc: &str,
) -> rusqlite::Result<Option<String>> {
    connection
        .prepare_cached(SELECT_LATEST_RESULT_SINCE)?
        .query_row(params![request_id, created_utc], |row| row.get(0))
        .optional()
}

/// Same without the time bound (full scan; only for clock-step fallbacks).
pub(crate) fn latest_result_any(
    connection: &Connection,
    request_id: &str,
) -> rusqlite::Result<Option<String>> {
    connection
        .prepare_cached(SELECT_LATEST_RESULT_ANY)?
        .query_row(params![request_id], |row| row.get(0))
        .optional()
}

pub(crate) fn find_attempt(
    connection: &Connection,
    request_id: &str,
) -> rusqlite::Result<Option<StoredAttempt>> {
    connection
        .prepare_cached(SELECT_ATTEMPT)?
        .query_row(params![request_id], |row| {
            Ok(StoredAttempt {
                payload_sha256: row.get(0)?,
                result_code: row.get(1)?,
            })
        })
        .optional()
}

pub(crate) fn insert_attempt(connection: &Connection, row: &AttemptRow) -> rusqlite::Result<()> {
    connection.prepare_cached(INSERT_ATTEMPT)?.execute(params![
        row.request_id,
        row.payload_sha256,
        row.session_digest,
        row.occurred_utc,
        row.result_code,
        row.detail_json,
    ])?;
    Ok(())
}

pub(crate) fn unresolved_intents(
    connection: &Connection,
    limit: i64,
) -> rusqlite::Result<Vec<UnresolvedIntent>> {
    let mut statement = connection.prepare_cached(SELECT_UNRESOLVED)?;
    let rows = statement.query_map(params![limit], |row| {
        Ok(UnresolvedIntent {
            request_id: row.get(0)?,
            intent_id: row.get(1)?,
            target_json: row.get(2)?,
            signal: row.get(3)?,
            status: row.get(4)?,
        })
    })?;
    rows.collect()
}
