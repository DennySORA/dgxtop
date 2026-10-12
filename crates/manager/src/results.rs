//! Idempotency cache for command results.
//!
//! The same `request_id` with the same payload digest returns the original
//! result (or "in progress" while it is still pending); a different payload
//! under a reused `request_id` is a `Conflict`. Completed entries expire after
//! `runtime.result_cache_ttl_ms` and the cache never exceeds
//! `runtime.result_cache_entries`. Pending entries are never evicted: their
//! number is bounded by the outstanding-command limit, which validation keeps
//! below the cache capacity.

use std::collections::{BTreeMap, VecDeque};

use dgxtop_core::commands::CommandResult;
use uuid::Uuid;

/// Outcome of looking a request up before processing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CacheLookup {
    /// Never seen (or expired): process it.
    Miss,
    /// Same payload, still being processed.
    InProgress,
    /// Same payload, already finished: replay this result.
    Completed(CommandResult),
    /// Same id, different payload.
    Conflict,
}

#[derive(Debug)]
struct CacheEntry {
    digest: String,
    result: Option<CommandResult>,
}

#[derive(Debug)]
pub(crate) struct ResultCache {
    capacity: usize,
    ttl_ns: u64,
    entries: BTreeMap<Uuid, CacheEntry>,
    /// Completed entries in completion order (completion times are monotonic).
    completed: VecDeque<(Uuid, u64)>,
}

impl ResultCache {
    pub(crate) fn new(capacity: usize, ttl_ns: u64) -> Self {
        Self {
            capacity: capacity.max(1),
            ttl_ns,
            entries: BTreeMap::new(),
            completed: VecDeque::new(),
        }
    }

    pub(crate) fn lookup(&mut self, request_id: Uuid, digest: &str, now_ns: u64) -> CacheLookup {
        self.expire(now_ns);
        match self.entries.get(&request_id) {
            None => CacheLookup::Miss,
            Some(entry) if entry.digest != digest => CacheLookup::Conflict,
            Some(entry) => entry
                .result
                .clone()
                .map_or(CacheLookup::InProgress, CacheLookup::Completed),
        }
    }

    /// Registers a request that is about to be processed.
    pub(crate) fn begin(&mut self, request_id: Uuid, digest: String) {
        if self.entries.len() >= self.capacity {
            self.evict_oldest_completed();
        }
        if self.entries.len() < self.capacity {
            self.entries.insert(
                request_id,
                CacheEntry {
                    digest,
                    result: None,
                },
            );
        }
    }

    /// Stores the final result of a registered request.
    pub(crate) fn complete(&mut self, request_id: Uuid, result: &CommandResult, now_ns: u64) {
        if let Some(entry) = self.entries.get_mut(&request_id)
            && entry.result.is_none()
        {
            entry.result = Some(result.clone());
            self.completed.push_back((request_id, now_ns));
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    fn expire(&mut self, now_ns: u64) {
        while let Some((request_id, completed_ns)) = self.completed.front().copied() {
            if completed_ns.saturating_add(self.ttl_ns) > now_ns {
                break;
            }
            self.completed.pop_front();
            self.entries.remove(&request_id);
        }
    }

    fn evict_oldest_completed(&mut self) {
        if let Some((request_id, _)) = self.completed.pop_front() {
            self.entries.remove(&request_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(request_id: Uuid) -> CommandResult {
        CommandResult::completed(request_id, "ok")
    }

    #[test]
    fn same_payload_replays_and_different_payload_conflicts() {
        let mut cache = ResultCache::new(8, 1_000);
        let id = Uuid::new_v4();
        assert_eq!(cache.lookup(id, "a", 0), CacheLookup::Miss);
        cache.begin(id, "a".into());
        assert_eq!(cache.lookup(id, "a", 1), CacheLookup::InProgress);
        assert_eq!(cache.lookup(id, "b", 1), CacheLookup::Conflict);
        cache.complete(id, &done(id), 2);
        assert_eq!(cache.lookup(id, "a", 3), CacheLookup::Completed(done(id)));
        assert_eq!(cache.lookup(id, "b", 3), CacheLookup::Conflict);
    }

    #[test]
    fn completed_entries_expire_after_ttl() {
        let mut cache = ResultCache::new(8, 1_000);
        let id = Uuid::new_v4();
        cache.begin(id, "a".into());
        cache.complete(id, &done(id), 10);
        assert!(matches!(
            cache.lookup(id, "a", 1_009),
            CacheLookup::Completed(_)
        ));
        assert_eq!(cache.lookup(id, "a", 1_010), CacheLookup::Miss);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn capacity_evicts_oldest_completed_but_never_pending() {
        let mut cache = ResultCache::new(2, u64::MAX);
        let pending = Uuid::new_v4();
        let finished = Uuid::new_v4();
        cache.begin(pending, "p".into());
        cache.begin(finished, "f".into());
        cache.complete(finished, &done(finished), 1);
        let newest = Uuid::new_v4();
        cache.begin(newest, "n".into());
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.lookup(finished, "f", 2), CacheLookup::Miss);
        assert_eq!(cache.lookup(pending, "p", 2), CacheLookup::InProgress);
        assert_eq!(cache.lookup(newest, "n", 2), CacheLookup::InProgress);
        // Both remaining entries are pending: a new request is processed
        // without caching instead of evicting a pending entry.
        let overflow = Uuid::new_v4();
        cache.begin(overflow, "o".into());
        assert_eq!(cache.lookup(overflow, "o", 2), CacheLookup::Miss);
        assert_eq!(cache.lookup(pending, "p", 2), CacheLookup::InProgress);
    }

    #[test]
    fn completing_twice_keeps_the_first_result() {
        let mut cache = ResultCache::new(4, u64::MAX);
        let id = Uuid::new_v4();
        cache.begin(id, "a".into());
        cache.complete(id, &done(id), 1);
        cache.complete(id, &CommandResult::completed(id, "second"), 2);
        assert_eq!(cache.lookup(id, "a", 3), CacheLookup::Completed(done(id)));
    }
}
