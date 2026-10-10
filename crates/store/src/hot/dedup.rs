//! Remembered committed batches for idempotent resends.
//!
//! A resent batch with the same id and content gets the original Ack back
//! (status `Duplicate`) without touching history; the same id with other
//! content is a `Conflict`. Only committed batches are remembered, bounded
//! by count and by bytes. Remembered Acks are result bookkeeping, so they
//! live in the `reserve` partition (control bookkeeping) and never compete
//! with snapshot memory; they may use at most a twentieth of it.

use std::collections::{HashMap, VecDeque};

use dgxtop_core::budget::{ByteLedger, Partition};
use dgxtop_core::canonical::{AckStatus, StoreAck};
use uuid::Uuid;

use crate::history::accounted::Account;

/// Committed batches remembered for duplicate detection.
pub(crate) const REMEMBERED_BATCHES: usize = 4096;
/// Accounted bytes of one remembered batch (id, fingerprint, ack, map slot).
const ENTRY_BYTES: u64 = 256;
/// Share of the reserve partition remembered batches may use (1/20: 1 MiB
/// of the 20 MiB reserve at the 256 MiB reference budget).
const RESERVE_SHARE_DIVISOR: u64 = 20;

/// What a batch id means to the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Recall {
    New,
    /// Same id and content: the original Ack, marked as duplicate.
    Duplicate(StoreAck),
    /// Same id, different content: the original Ack for context.
    Conflict(StoreAck),
}

struct Remembered {
    fingerprint: String,
    ack: StoreAck,
}

pub(crate) struct RememberedBatches {
    order: VecDeque<Uuid>,
    entries: HashMap<Uuid, Remembered>,
    capacity: usize,
    account: Account,
}

impl RememberedBatches {
    pub(crate) fn new(ledger: &ByteLedger, capacity: usize) -> Self {
        let budget = ledger.limits().limit(Partition::Reserve) / RESERVE_SHARE_DIVISOR;
        let by_bytes = usize::try_from(budget / ENTRY_BYTES).unwrap_or(usize::MAX);
        Self {
            order: VecDeque::new(),
            entries: HashMap::new(),
            capacity: capacity.min(by_bytes),
            account: Account::new(ledger, Partition::Reserve),
        }
    }

    pub(crate) fn recall(&self, batch_id: Uuid, fingerprint: &str) -> Recall {
        match self.entries.get(&batch_id) {
            None => Recall::New,
            Some(entry) if entry.fingerprint == fingerprint => {
                let mut ack = entry.ack.clone();
                ack.status = AckStatus::Duplicate;
                Recall::Duplicate(ack)
            }
            Some(entry) => Recall::Conflict(entry.ack.clone()),
        }
    }

    /// Remembers a committed batch, forgetting the oldest when full. When
    /// even that leaves no memory the batch is not remembered: a resend then
    /// fails the base check instead of committing twice.
    pub(crate) fn remember(&mut self, batch_id: Uuid, fingerprint: String, ack: StoreAck) {
        if self.capacity == 0 || self.entries.contains_key(&batch_id) {
            return;
        }
        if self.order.len() >= self.capacity {
            self.forget_oldest();
        }
        while self.account.try_grow(ENTRY_BYTES).is_err() {
            if !self.forget_oldest() {
                return;
            }
        }
        self.order.push_back(batch_id);
        self.entries
            .insert(batch_id, Remembered { fingerprint, ack });
    }

    fn forget_oldest(&mut self) -> bool {
        let Some(oldest) = self.order.pop_front() else {
            return false;
        };
        self.entries.remove(&oldest);
        let remaining = u64::try_from(self.order.len()).unwrap_or(u64::MAX);
        self.account
            .shrink_to(remaining.saturating_mul(ENTRY_BYTES));
        true
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.order.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::budget::PartitionLimits;
    use dgxtop_core::canonical::ArchiveState;
    use dgxtop_core::identity::CollectorId;

    fn ack(batch_id: Uuid) -> StoreAck {
        StoreAck {
            batch_id,
            collector_id: CollectorId::new("cpu").unwrap(),
            status: AckStatus::Committed,
            committed_revision: 7,
            state_revision: 3,
            archive: ArchiveState::Disabled,
            reason: None,
        }
    }

    #[test]
    fn duplicates_return_the_original_ack_and_conflicts_are_detected() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut batches = RememberedBatches::new(&ledger, 4);
        let id = Uuid::new_v4();
        assert_eq!(batches.recall(id, "a"), Recall::New);
        batches.remember(id, "a".to_owned(), ack(id));
        let Recall::Duplicate(duplicate) = batches.recall(id, "a") else {
            panic!("expected duplicate");
        };
        assert_eq!(duplicate.status, AckStatus::Duplicate);
        assert_eq!(duplicate.committed_revision, 7);
        assert!(matches!(batches.recall(id, "b"), Recall::Conflict(_)));
    }

    #[test]
    fn remembered_batches_are_bounded_by_count_and_bytes() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut batches = RememberedBatches::new(&ledger, 3);
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        for id in &ids {
            batches.remember(*id, "x".to_owned(), ack(*id));
        }
        assert_eq!(batches.len(), 3);
        assert_eq!(batches.recall(ids[0], "x"), Recall::New);
        assert_eq!(ledger.used(Partition::Reserve), 3 * ENTRY_BYTES);
        assert_eq!(ledger.used(Partition::HotState), 0);
        let reference = ByteLedger::for_memory_limit_mib(256);
        assert_eq!(
            RememberedBatches::new(&reference, REMEMBERED_BATCHES).capacity,
            REMEMBERED_BATCHES
        );

        let tiny = ByteLedger::new(PartitionLimits::for_memory_limit_mib(0));
        let mut starved = RememberedBatches::new(&tiny, 3);
        starved.remember(ids[0], "x".to_owned(), ack(ids[0]));
        assert_eq!(starved.len(), 0);
    }
}
