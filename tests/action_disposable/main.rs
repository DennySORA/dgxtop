//! Disposable-child action suite (AT10, AT32).
//!
//! Drives the full process-action stack of dgxtop the way a frontend does,
//! through a `SessionHandle`: the real Manager control thread, the real SQLite
//! audit repository (WAL, `synchronous=FULL`) and the real pidfd action
//! adapter in their single-owner worker actors, and the real file config
//! repository, wired like `crates/dgxtop/src/monitor/` wires them.
//!
//! Safety: the only processes that may ever be signalled are children this
//! suite spawned itself (`sleep 60`), each killed and reaped by a guard on
//! drop. A process of another user is used only as a target that must be
//! refused at preparation; no confirmation is ever submitted for it. All
//! state lives in private temporary directories.
//!
//! Evidence per scenario: the command results and projected actions view a
//! frontend sees; the Manager's event log; a journal of control-thread
//! hand-overs, audit commits, adapter calls and every pidfd system call
//! (spies that delegate to the real code unchanged); the child's own exit
//! status; and the audit database read back through the store's public API.

// Test helpers may unwrap: a broken fixture must fail loudly.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod at10;
mod at32;
mod audit_trail;
mod child;
mod clock;
mod expect;
mod journal;
mod spies;
mod stack;
