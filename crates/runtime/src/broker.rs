//! Read de-duplication (`RUNTIME.md` §2).
//!
//! Identical concurrent reads — same adapter, host generation, catalog
//! revision, config epoch, cycle and read group — are dispatched once and
//! every requester receives the same immutable `Arc` result. A different
//! cycle, generation or epoch is a different key, so an old reply can never
//! be served as a cache hit for a newer slot. After the first completion the
//! entry stays in a bounded recycle area until its deadline; a requester that
//! arrives in that window gets the same result (or the same failure) without
//! a second host request. There is no per-requester retry.

use std::collections::HashMap;
use std::sync::Arc;

use dgxtop_core::identity::{AdapterId, ReadGroup};
use dgxtop_core::observation::ReadPlan;
use dgxtop_core::ports::{AdapterFailure, IngressBatch};
use uuid::Uuid;

/// Shared outcome of one de-duplicated read.
pub type BrokerResult = Result<Arc<IngressBatch>, AdapterFailure>;

/// De-duplication key (`RUNTIME.md` §2 Broker key).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BrokerKey {
    pub adapter_id: AdapterId,
    pub host_generation: u32,
    pub catalog_revision: u32,
    pub epoch: u64,
    pub cycle_id: u64,
    /// Full read group including any `:page` suffix.
    pub read_group: ReadGroup,
}

impl BrokerKey {
    pub fn from_plan(plan: &ReadPlan) -> Self {
        Self {
            adapter_id: plan.adapter_id.clone(),
            host_generation: plan.host_generation,
            catalog_revision: plan.catalog_revision,
            epoch: plan.epoch,
            cycle_id: plan.cycle_id,
            read_group: plan.read_group.clone(),
        }
    }
}

/// Decision for one incoming read.
#[derive(Debug)]
pub enum Admission {
    /// No identical read is known: the caller must dispatch it.
    Dispatch,
    /// Joined an identical in-flight read; the shared result will follow.
    Joined,
    /// An identical read already completed and has not expired.
    Cached(BrokerResult),
    /// The table (or the joiner list) is full; nothing was recorded.
    Full,
}

/// Counters for diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BrokerStats {
    pub dispatched: u64,
    pub joined: u64,
    pub cached_hits: u64,
    pub rejected_full: u64,
    pub evicted: u64,
}

#[derive(Debug)]
enum EntryState {
    InFlight { requesters: Vec<Uuid> },
    Completed(BrokerResult),
}

#[derive(Debug)]
struct Entry {
    deadline_boot_ns: u64,
    sequence: u64,
    state: EntryState,
}

/// Bounded de-duplication table.
#[derive(Debug)]
pub struct ReadBroker {
    capacity: usize,
    max_requesters: usize,
    next_sequence: u64,
    entries: HashMap<BrokerKey, Entry>,
    stats: BrokerStats,
}

impl ReadBroker {
    /// `capacity` bounds entries (in flight + recycle area); `max_requesters`
    /// bounds the requesters sharing one in-flight read.
    pub fn new(capacity: usize, max_requesters: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            max_requesters: max_requesters.max(1),
            next_sequence: 0,
            entries: HashMap::new(),
            stats: BrokerStats::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub const fn stats(&self) -> BrokerStats {
        self.stats
    }

    /// Records `request_id` for `key` and says what the caller must do.
    pub fn admit(
        &mut self,
        key: &BrokerKey,
        request_id: Uuid,
        deadline_boot_ns: u64,
        now_boot_ns: u64,
    ) -> Admission {
        self.purge_expired(now_boot_ns);
        if let Some(entry) = self.entries.get_mut(key) {
            return Self::admit_existing(entry, request_id, self.max_requesters, &mut self.stats);
        }
        if self.entries.len() >= self.capacity && !self.evict_oldest_completed() {
            self.stats.rejected_full += 1;
            return Admission::Full;
        }
        self.next_sequence += 1;
        self.entries.insert(
            key.clone(),
            Entry {
                deadline_boot_ns,
                sequence: self.next_sequence,
                state: EntryState::InFlight {
                    requesters: vec![request_id],
                },
            },
        );
        self.stats.dispatched += 1;
        Admission::Dispatch
    }

    fn admit_existing(
        entry: &mut Entry,
        request_id: Uuid,
        max_requesters: usize,
        stats: &mut BrokerStats,
    ) -> Admission {
        match &mut entry.state {
            EntryState::InFlight { requesters } => {
                if requesters.contains(&request_id) {
                    return Admission::Joined;
                }
                if requesters.len() >= max_requesters {
                    stats.rejected_full += 1;
                    return Admission::Full;
                }
                requesters.push(request_id);
                stats.joined += 1;
                Admission::Joined
            }
            EntryState::Completed(result) => {
                stats.cached_hits += 1;
                Admission::Cached(result.clone())
            }
        }
    }

    /// Forgets a read that was admitted but never dispatched.
    pub fn abandon(&mut self, key: &BrokerKey, request_id: Uuid) {
        let remove = match self.entries.get_mut(key) {
            Some(Entry {
                state: EntryState::InFlight { requesters },
                ..
            }) => {
                requesters.retain(|requester| *requester != request_id);
                requesters.is_empty()
            }
            _ => false,
        };
        if remove {
            self.entries.remove(key);
            self.stats.dispatched = self.stats.dispatched.saturating_sub(1);
        }
    }

    /// Stores the shared result and returns every requester to notify, the
    /// dispatching request first. Unknown keys return no requesters.
    pub fn complete(&mut self, key: &BrokerKey, result: BrokerResult) -> Vec<Uuid> {
        let Some(entry) = self.entries.get_mut(key) else {
            return Vec::new();
        };
        let previous = std::mem::replace(&mut entry.state, EntryState::Completed(result));
        match previous {
            EntryState::InFlight { requesters } => requesters,
            EntryState::Completed(_) => Vec::new(),
        }
    }

    /// Drops completed entries whose deadline passed. In-flight entries are
    /// never purged: the supervisor always completes them (success, failure,
    /// timeout or shutdown), so no requester is left without an answer.
    pub fn purge_expired(&mut self, now_boot_ns: u64) {
        let before = self.entries.len();
        self.entries.retain(|_, entry| {
            !(matches!(entry.state, EntryState::Completed(_))
                && entry.deadline_boot_ns <= now_boot_ns)
        });
        let removed = before - self.entries.len();
        self.stats.evicted += u64::try_from(removed).unwrap_or(u64::MAX);
    }

    fn evict_oldest_completed(&mut self) -> bool {
        let oldest = self
            .entries
            .iter()
            .filter(|(_, entry)| matches!(entry.state, EntryState::Completed(_)))
            .min_by_key(|(_, entry)| entry.sequence)
            .map(|(key, _)| key.clone());
        match oldest {
            Some(key) => {
                self.entries.remove(&key);
                self.stats.evicted += 1;
                true
            }
            None => false,
        }
    }

    /// Fails every in-flight entry (supervisor shutdown) and returns the
    /// requesters per key.
    pub fn fail_all_in_flight(&mut self, failure: &AdapterFailure) -> Vec<(BrokerKey, Vec<Uuid>)> {
        let keys: Vec<BrokerKey> = self
            .entries
            .iter()
            .filter(|(_, entry)| matches!(entry.state, EntryState::InFlight { .. }))
            .map(|(key, _)| key.clone())
            .collect();
        keys.into_iter()
            .map(|key| {
                let requesters = self.complete(&key, Err(failure.clone()));
                (key, requesters)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::ports::AdapterFailureKind;
    use dgxtop_core::testing::{adapter_id, batch_of, read_group};

    fn key(cycle: u64) -> BrokerKey {
        BrokerKey {
            adapter_id: adapter_id("nvml"),
            host_generation: 1,
            catalog_revision: 1,
            epoch: 1,
            cycle_id: cycle,
            read_group: read_group("fast"),
        }
    }

    fn shared_batch() -> Arc<IngressBatch> {
        Arc::new(IngressBatch::new(
            batch_of("nvml", "fast", 1, Vec::new()),
            None,
        ))
    }

    fn timed_out() -> AdapterFailure {
        AdapterFailure {
            kind: AdapterFailureKind::TimedOut,
            message: "deadline passed".into(),
        }
    }

    #[test]
    fn identical_concurrent_reads_are_dispatched_once_and_share_the_result() {
        let mut broker = ReadBroker::new(8, 4);
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        assert!(matches!(
            broker.admit(&key(1), first, 1_000, 0),
            Admission::Dispatch
        ));
        assert!(matches!(
            broker.admit(&key(1), second, 1_000, 10),
            Admission::Joined
        ));
        // The same request id joining twice is idempotent.
        assert!(matches!(
            broker.admit(&key(1), second, 1_000, 10),
            Admission::Joined
        ));
        let batch = shared_batch();
        let requesters = broker.complete(&key(1), Ok(Arc::clone(&batch)));
        assert_eq!(requesters, vec![first, second]);
        let stats = broker.stats();
        assert_eq!((stats.dispatched, stats.joined), (1, 1));
    }

    #[test]
    fn different_cycle_generation_epoch_or_group_never_hit() {
        let mut broker = ReadBroker::new(16, 4);
        let base = key(1);
        assert!(matches!(
            broker.admit(&base, Uuid::new_v4(), 1_000, 0),
            Admission::Dispatch
        ));
        broker.complete(&base, Ok(shared_batch()));
        let variants = [
            BrokerKey {
                cycle_id: 2,
                ..base.clone()
            },
            BrokerKey {
                host_generation: 2,
                ..base.clone()
            },
            BrokerKey {
                epoch: 2,
                ..base.clone()
            },
            BrokerKey {
                catalog_revision: 2,
                ..base.clone()
            },
            BrokerKey {
                read_group: read_group("fast:1"),
                ..base.clone()
            },
            BrokerKey {
                adapter_id: adapter_id("procfs"),
                ..base.clone()
            },
        ];
        for variant in &variants {
            assert!(
                matches!(
                    broker.admit(variant, Uuid::new_v4(), 1_000, 1),
                    Admission::Dispatch
                ),
                "{variant:?} must not hit"
            );
        }
    }

    #[test]
    fn completed_results_are_served_until_the_deadline_then_expire() {
        let mut broker = ReadBroker::new(8, 4);
        broker.admit(&key(1), Uuid::new_v4(), 500, 0);
        let batch = shared_batch();
        broker.complete(&key(1), Ok(Arc::clone(&batch)));
        match broker.admit(&key(1), Uuid::new_v4(), 500, 499) {
            Admission::Cached(Ok(cached)) => assert!(Arc::ptr_eq(&cached, &batch)),
            other => panic!("expected a cached hit, got {other:?}"),
        }
        assert!(matches!(
            broker.admit(&key(1), Uuid::new_v4(), 900, 500),
            Admission::Dispatch
        ));
        assert_eq!(broker.stats().cached_hits, 1);
    }

    #[test]
    fn failures_are_shared_without_per_requester_retry() {
        let mut broker = ReadBroker::new(8, 4);
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        broker.admit(&key(1), first, 500, 0);
        broker.admit(&key(1), second, 500, 0);
        let requesters = broker.complete(&key(1), Err(timed_out()));
        assert_eq!(requesters, vec![first, second]);
        match broker.admit(&key(1), Uuid::new_v4(), 500, 400) {
            Admission::Cached(Err(failure)) => {
                assert_eq!(failure.kind, AdapterFailureKind::TimedOut);
            }
            other => panic!("expected the shared failure, got {other:?}"),
        }
    }

    #[test]
    fn table_and_joiners_are_bounded() {
        let mut broker = ReadBroker::new(2, 2);
        assert!(matches!(
            broker.admit(&key(1), Uuid::new_v4(), 1_000, 0),
            Admission::Dispatch
        ));
        assert!(matches!(
            broker.admit(&key(2), Uuid::new_v4(), 1_000, 0),
            Admission::Dispatch
        ));
        // Both entries are in flight: nothing can be evicted.
        assert!(matches!(
            broker.admit(&key(3), Uuid::new_v4(), 1_000, 0),
            Admission::Full
        ));
        assert!(matches!(
            broker.admit(&key(1), Uuid::new_v4(), 1_000, 0),
            Admission::Joined
        ));
        assert!(matches!(
            broker.admit(&key(1), Uuid::new_v4(), 1_000, 0),
            Admission::Full
        ));
        // A completed entry in the recycle area makes room.
        broker.complete(&key(2), Ok(shared_batch()));
        assert!(matches!(
            broker.admit(&key(3), Uuid::new_v4(), 1_000, 0),
            Admission::Dispatch
        ));
        assert_eq!(broker.len(), 2);
        assert_eq!(broker.stats().rejected_full, 2);
    }

    #[test]
    fn in_flight_entries_are_never_purged_by_time() {
        let mut broker = ReadBroker::new(4, 4);
        let first = Uuid::new_v4();
        broker.admit(&key(1), first, 100, 0);
        broker.purge_expired(10_000);
        assert_eq!(broker.complete(&key(1), Err(timed_out())), vec![first]);
        broker.purge_expired(10_000);
        assert!(broker.is_empty());
    }

    #[test]
    fn abandon_and_fail_all() {
        let mut broker = ReadBroker::new(4, 4);
        let first = Uuid::new_v4();
        broker.admit(&key(1), first, 100, 0);
        broker.abandon(&key(1), first);
        assert!(broker.is_empty());
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        broker.admit(&key(2), a, 100, 0);
        broker.admit(&key(3), b, 100, 0);
        let failure = AdapterFailure {
            kind: AdapterFailureKind::Shutdown,
            message: "stopping".into(),
        };
        let mut failed = broker.fail_all_in_flight(&failure);
        failed.sort_by_key(|(key, _)| key.cycle_id);
        assert_eq!(failed.len(), 2);
        assert_eq!(failed[0].1, vec![a]);
        assert_eq!(failed[1].1, vec![b]);
        assert!(broker.complete(&key(2), Err(failure)).is_empty());
    }
}
