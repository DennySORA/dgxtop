//! How many jobs each worker's queue holds. The composition root sizes its
//! actors with these, and the integration rigs and benches reuse them, so a
//! rig's back-pressure is the program's.
//!
//! The pools sized by the configuration (collectors, history reads) are
//! not here: their queues are `queues.collector_jobs` and
//! `queues.history_requests`.

/// Archive journals waiting (at most 8 transaction batches);
/// a full queue becomes an archive gap, never a wait in the state.
pub const ARCHIVE_QUEUE: usize = 8;
/// State jobs waiting: commits, gaps, maintenance and sampling changes.
pub const STATE_QUEUE: usize = 64;
/// Audit operations waiting: intents, outcomes, attempts and recovery.
pub const AUDIT_QUEUE: usize = 64;
/// Process actions waiting; the Manager keeps few in flight.
pub const ACTION_QUEUE: usize = 16;
/// Configuration swaps waiting; the Manager sends one at a time.
pub const CONFIG_QUEUE: usize = 8;
/// History tier jobs waiting (D-82): the history sends one startup read at
/// a time and a journal of closed buckets per finished minute.
pub const HISTORY_TIERS_QUEUE: usize = 16;
/// Probe jobs waiting (D-76); the process actor keeps one in flight.
pub const PROCESS_PROBE_QUEUE: usize = 2;
/// Inputs waiting for the history's process actor (D-79): a table each
/// process period, a tick each commit, the probe's and the stores' answers.
pub const PROCESS_HISTORY_INPUTS: usize = 64;
/// Process history store jobs waiting (D-76): a journal every 10 s, the
/// short-term file every 30 s, a run's read now and then.
pub const PROCESS_HISTORY_QUEUE: usize = 8;
