//! Ledger-backed storage helpers.
//!
//! Every byte a container may hold is reserved in the global [`ByteLedger`]
//! before the allocation happens, so a failed reservation leaves the container
//! unchanged and partition limits hold.

use std::collections::VecDeque;
use std::collections::vec_deque;

use dgxtop_core::budget::{ByteLedger, BytePermit, CapacityError, Partition};

/// Smallest capacity a deque grows to, so tiny pushes do not reallocate.
const MIN_CAPACITY: usize = 4;

/// Bytes occupied by `count` values of `T` (saturating).
pub(crate) fn bytes_of<T>(count: usize) -> u64 {
    u64::try_from(count.saturating_mul(std::mem::size_of::<T>())).unwrap_or(u64::MAX)
}

/// A growable byte reservation in one partition; empty until it first grows.
pub(crate) struct Account {
    ledger: ByteLedger,
    partition: Partition,
    permit: Option<BytePermit>,
}

impl Account {
    pub(crate) fn new(ledger: &ByteLedger, partition: Partition) -> Self {
        Self {
            ledger: ledger.clone(),
            partition,
            permit: None,
        }
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.permit.as_ref().map_or(0, BytePermit::bytes)
    }

    /// Grows the reservation, failing without side effects.
    pub(crate) fn try_grow(&mut self, additional: u64) -> Result<(), CapacityError> {
        if additional == 0 {
            return Ok(());
        }
        match &mut self.permit {
            Some(permit) => permit.try_grow(additional),
            None => {
                self.permit = Some(self.ledger.try_reserve(self.partition, additional)?);
                Ok(())
            }
        }
    }

    pub(crate) fn shrink_to(&mut self, bytes: u64) {
        if let Some(permit) = &mut self.permit {
            permit.shrink_to(bytes);
        }
    }

    /// Moves `bytes` of this reservation into a separate permit.
    pub(crate) fn split_off(&mut self, bytes: u64) -> Option<BytePermit> {
        self.permit.as_mut()?.split_off(bytes)
    }
}

/// A `VecDeque` whose capacity is always covered by an [`Account`].
pub(crate) struct AccountedDeque<T> {
    items: VecDeque<T>,
    account: Account,
}

impl<T> AccountedDeque<T> {
    pub(crate) fn new(ledger: &ByteLedger, partition: Partition) -> Self {
        Self {
            items: VecDeque::new(),
            account: Account::new(ledger, partition),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn iter(&self) -> vec_deque::Iter<'_, T> {
        self.items.iter()
    }

    pub(crate) fn front(&self) -> Option<&T> {
        self.items.front()
    }

    pub(crate) fn back(&self) -> Option<&T> {
        self.items.back()
    }

    pub(crate) fn get(&self, index: usize) -> Option<&T> {
        self.items.get(index)
    }

    pub(crate) fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.items.get_mut(index)
    }

    /// Reserved bytes (capacity, not length).
    #[cfg(test)]
    pub(crate) fn reserved_bytes(&self) -> u64 {
        self.account.bytes()
    }

    /// Bytes the next push would have to reserve.
    pub(crate) fn growth_cost(&self) -> u64 {
        if self.items.len() < self.items.capacity() {
            return 0;
        }
        bytes_of::<T>(self.next_capacity() - self.items.capacity())
    }

    fn next_capacity(&self) -> usize {
        self.items
            .capacity()
            .saturating_mul(2)
            .max(MIN_CAPACITY)
            .max(self.items.len().saturating_add(1))
    }

    fn try_make_room(&mut self) -> Result<(), CapacityError> {
        if self.items.len() < self.items.capacity() {
            return Ok(());
        }
        let target = self.next_capacity();
        self.account
            .try_grow(bytes_of::<T>(target - self.items.capacity()))?;
        self.items.reserve_exact(target - self.items.len());
        self.reconcile();
        Ok(())
    }

    /// Keeps the reservation equal to the real capacity: the allocator may
    /// round up, and shrinking returns bytes to the partition.
    fn reconcile(&mut self) {
        let actual = bytes_of::<T>(self.items.capacity());
        let reserved = self.account.bytes();
        if actual < reserved {
            self.account.shrink_to(actual);
        } else if actual > reserved {
            // The extra bytes already exist; recording them keeps the ledger
            // honest even if it briefly passes the soft target.
            let _ = self.account.try_grow(actual - reserved);
        }
    }

    pub(crate) fn try_push_back(&mut self, item: T) -> Result<(), CapacityError> {
        self.try_make_room()?;
        self.items.push_back(item);
        Ok(())
    }

    pub(crate) fn try_insert(&mut self, index: usize, item: T) -> Result<(), CapacityError> {
        self.try_make_room()?;
        self.items.insert(index.min(self.items.len()), item);
        Ok(())
    }

    pub(crate) fn pop_front(&mut self) -> Option<T> {
        self.items.pop_front()
    }

    pub(crate) fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        self.items.retain(keep);
    }

    /// Returns capacity to the partition once at most a quarter is used.
    pub(crate) fn release_spare(&mut self) {
        let capacity = self.items.capacity();
        if capacity > MIN_CAPACITY && self.items.len() <= capacity / 4 {
            self.items.shrink_to(self.items.len().max(MIN_CAPACITY));
            self.reconcile();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deque_capacity_is_reserved_before_growth() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut deque = AccountedDeque::<u64>::new(&ledger, Partition::Aggregates);
        assert_eq!(ledger.used(Partition::Aggregates), 0);
        assert_eq!(deque.growth_cost(), bytes_of::<u64>(MIN_CAPACITY));
        for value in 0..100u64 {
            deque.try_push_back(value).unwrap();
        }
        assert_eq!(
            ledger.used(Partition::Aggregates),
            bytes_of::<u64>(deque.items.capacity())
        );
        while deque.len() > 3 {
            deque.pop_front();
        }
        deque.release_spare();
        assert_eq!(
            ledger.used(Partition::Aggregates),
            bytes_of::<u64>(deque.items.capacity())
        );
        drop(deque);
        assert_eq!(ledger.used(Partition::Aggregates), 0);
    }

    #[test]
    fn failed_growth_leaves_deque_unchanged() {
        let ledger = ByteLedger::new(dgxtop_core::budget::PartitionLimits::for_memory_limit_mib(
            0,
        ));
        let mut deque = AccountedDeque::<u64>::new(&ledger, Partition::Aggregates);
        assert!(deque.try_push_back(1).is_err());
        assert!(deque.is_empty());
        assert_eq!(deque.reserved_bytes(), 0);
    }

    #[test]
    fn account_split_moves_bytes() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut account = Account::new(&ledger, Partition::Reserve);
        account.try_grow(100).unwrap();
        let part = account.split_off(40).unwrap();
        assert_eq!(account.bytes(), 60);
        assert_eq!(ledger.used(Partition::Reserve), 100);
        drop(part);
        assert_eq!(ledger.used(Partition::Reserve), 60);
        account.shrink_to(0);
        assert_eq!(ledger.used(Partition::Reserve), 0);
    }
}
