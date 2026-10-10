//! Composition-level fault-injection suite (spec/dgxtop-v2/ACCEPTANCE.md
//! AT06, AT24, AT30, AT31, AT36, AT39).
//!
//! Each scenario assembles the real concretes the composition root wires
//! together — the Manager on its own control thread, the HotStore actor,
//! the SQLite audit and archive behind their actors, the file configuration
//! repository, the collector pool, and the adapter-host supervisor with real
//! `dgxtop --adapter-host` children serving replay fixtures — and fakes only
//! the fault itself: a free-space probe below the low-water mark, a
//! failpoint filesystem, a gate in front of a storage worker ("the disk
//! stopped answering"), a replay `hang` fault, a child whose reap never
//! succeeds, or a panicking port on the control thread. The process-action
//! port is always a recorder: no scenario signals any process.
//!
//! Scenarios run one at a time (`support::probes::serial`) because they count
//! process-wide descriptors and threads and spawn child processes. Every
//! wait is bounded; time-heavy state machines (overload recovery needs 90 s
//! of healthy samples) run on a fake clock.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

mod blocked_hosts;
mod config_uncertainty;
mod overload;
mod safe_shutdown;
mod store_partitions;
