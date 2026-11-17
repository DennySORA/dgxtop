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
//! frontend sees; a journal of control-thread hand-overs, audit commits,
//! adapter calls and every pidfd system call (spies that delegate to the
//! real code unchanged); the child's own exit status; and the audit
//! database read back through the store's public API.

// A test-only crate: its helpers may unwrap and panic, so a broken fixture
// fails loudly (`allow-*-in-tests` reaches the whole crate).
#![cfg(test)]

mod at10;
mod at32;
mod audit_trail;
mod child;
mod clock;
mod expect;
mod journal;
mod spies;
mod stack;
