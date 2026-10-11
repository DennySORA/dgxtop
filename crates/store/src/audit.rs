//! SQLite action audit (`audit.sqlite3`, WAL, `synchronous=FULL`).
//!
//! The audit is the gate of every destructive action (`SECURITY.md` §3,
//! ADR16, ADR20): a signal may only be sent after [`SqliteAuditRepository`]
//! returned a `DurableReceipt`, which happens strictly after COMMIT of one
//! small FULL transaction. Every condition that could make the intent
//! non-durable (low disk, WAL backlog, size cap, busy past the deadline,
//! corruption, version gate) fails closed with an `AuditFailure`; a file
//! found unusable at open is recreated empty instead (D-85). Outcomes of committed intents may use a reserved slice of
//! the budget so the trail of an action that was sent can still be recorded.
//!
//! Privacy: only the stable process identity, uid and sanitized basename are
//! stored (inside `process_key_json`); never a full command line,
//! environment or confirmation token.

mod limits;
mod queries;
mod records;
mod repository;
mod retention;

pub use limits::{AuditLimits, AuditOpenError};
pub use repository::SqliteAuditRepository;
